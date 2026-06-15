# Story 016 — Viewer OODA derivation cache

**Value delivered:** OODA-mode derivation stays cheap as session
history grows. Today `buildSessions` re-parses every SSE response
body in the entire history on every ingest tick (each event from
the noodle WebSocket triggers a fresh `useMemo` evaluation). This
story caches the parsed `ParsedResponse` per exchange so each SSE
body is parsed at most once for the lifetime of the capture, no
matter how many times derivation runs.

Story 015's predecessor work already collapsed the double-parse
within a single `buildSessions` pass — each pair now parses once
per pass instead of twice (`classify` + `buildRoundTrip` /
`buildAuxCall` share a single `ParsedResponse`). This story closes
the remaining gap: the parse work that repeats *across passes*.

## Acceptance criteria

A user can:

1. Stream a long Claude Code session through the proxy with the
   viewer attached (≥ 50 round-trips, mix of streaming and
   non-streaming bodies).
2. Observe that switching between modes, sorting the rail, or
   appending a new exchange does **not** trigger a measurable
   re-parse of historical SSE bodies. The OODA rebuild stays
   sub-millisecond per ingest tick once steady state is reached.
3. Clear the local capture (`clearLocal()`) → cache entries for
   the cleared exchanges go away (no unbounded growth).

A developer can:

4. Add a `derivedParseCacheSize` field to whatever debug surface
   we expose (e.g., the existing stats badge or a hidden dev
   panel) and confirm the size tracks the count of distinct
   response bodies seen, not the count of derivation passes.

## Out of scope (deferred)

- Caching anything beyond `ParsedResponse`. The `OodaSession[]`
  itself is still rebuilt from scratch on each pass — that work
  is dominated by the parse cost, so eliminating the parse is
  enough. Incrementalizing the rest of `buildSessions` is a
  bigger refactor for a later story if real telemetry says we
  need it.
- Persisting the cache across page reloads. The viewer's
  `EventStore` already drops state on reload; matching that.

## Implementation notes

### Cache key choice

Two viable shapes:

- **`Map<event_id, ParsedResponse>`** colocated on `EventStore`.
  Explicit invalidation on `clearLocal()` and on any path that
  evicts an exchange. Keyed by the stable `event_id` string so
  the cache survives object-identity churn (e.g., if a future
  refactor rebuilds the `ExchangePair` shell on mutation).
- **`WeakMap<Exchange, ParsedResponse>`** keyed on the response
  `Exchange` object. Auto-collects when the `Exchange` is no
  longer reachable. No explicit invalidation needed, but only
  works as long as the response object's identity is preserved
  across rebuilds — which today it is (see `events.ts:34-42`).

Recommendation: start with the `Map<event_id, …>` form. It is
the more boring choice, easier to reason about, easier to expose
size in dev tooling, and easier to test. Move to `WeakMap` later
only if the explicit-invalidation surface gets messy.

### Where the cache lives

The cache belongs to the *store*, not to the derivation module.
`buildSessions` should stay pure (input → output, no closure
state) so it remains trivially testable. Plumb the cache in as
an optional dependency:

```ts
export function buildSessions(
  pairs: ExchangePair[],
  pairsById: Map<string, ExchangePair>,
  parseCache?: ParseCache,
): OodaSession[]
```

…where `ParseCache` is a tiny interface:

```ts
interface ParseCache {
  get(eventId: string): ParsedResponse | undefined;
  set(eventId: string, parsed: ParsedResponse): void;
}
```

Default `undefined` keeps the test path (and any other caller
that does not want caching) zero-overhead.

### Cache lifecycle

- `EventStore.ingest` for a new response event populates the
  cache lazily — derivation does the parse, store memoizes the
  result via the `set` hook.
- `EventStore.clearLocal()` calls `parseCache.clear()`.
- The cache must invalidate an entry when the underlying
  response body changes. Today response bodies are written once
  and never mutated, so no invalidation hook is needed. If that
  ever changes, key on a stable body hash instead of `event_id`.

### Telemetry hook (optional but useful)

A `parseCache.stats()` returning `{ size, hits, misses }` makes
the next perf investigation trivial. Exposable behind a
debug-only flag in the UI; not required for ship.

## Why this is a separate story

The double-parse fix (already shipped) was a strict simplification
— it removed work without changing the architecture. Caching is
different: it adds a piece of mutable state to the store and a
plumbing dependency to `buildSessions`. That's worth its own
review surface, its own tests, and its own performance regression
fixture. Keeping the two changes separate also means we can decide,
based on real telemetry, whether the cache is worth the
machinery — or whether the single-parse pass is already fast
enough on realistic workloads.

## Test plan

**Vitest** (`web/tests/derived/ooda_cache.test.ts`, new):

- Cache hit: second call to `buildSessions` over the same input
  pairs (with a shared cache instance) does not invoke the SSE
  parser. Verify by injecting a counting `parseAnthropicSse`
  spy.
- Cache miss for a new pair: a third call with one new pair only
  parses that pair's body.
- Clear: after `parseCache.clear()`, the next call re-parses
  everything.

**Live**:

- Replay a recorded long session through the viewer and observe
  the OODA rebuild time in DevTools' Performance panel before
  and after this story lands.

## Dependencies

- Story 014 (OODA flat thread) and 015 (sub-agent linking) define
  the derivation shape this story optimizes.
- No engine work.
