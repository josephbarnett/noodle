# 020 — Side-effect sink + Resolver wiring + symmetric `ResponseFlow` encode

**Status:** Decided. Implementing (backlog item 4; story 031).
**Author:** Joe Barnett · Claude
**Date:** 2026-05-17
**References:** 004 (attribution model — what `Hint` / `Resolved`
mean), 015 §5 (side-channel buses), 015 §16 (empty-on-error
contract — `AuditKind::Errored` emissions need somewhere to
land), 017 §7 (engine-encode wiring gap explicitly flagged as
item-4 work), 019 §2.5 (correlation scope contract — relevant for
how a Resolved record keys to a flow / session). Story file:
`docs/features/031-side-effect-sink-and-resolver.md`. Backlog
row: `docs/features/000-overview.md` item 4.

---

## 1. Context — the gap

The layered core today decodes the response stream into typed
`NormalizedEvent`s and runs transforms over them. Transforms can
emit `SideEffect::{Hint, Artifact, Audit}` on a per-flow side
channel (`SideChannelTx`, 015 §5). The engine collects those
emissions into a `Vec<SideEffect>` and… **drops the vector on the
floor at flow end.** Three concrete consequences:

1. **The attribution loop does not close.** `MarkerStripTransform`
   removes the `<noodle:NAME>VALUE</noodle:NAME>` markers from
   `Token.text` (ADR 017's provenance discipline makes that part
   correct), but the extracted value goes nowhere. No `Hint` is
   emitted today, and even if one were, there is no `Resolver`
   call, no destination for the `Resolved` record. Items 2 (Filter
   slice) and 3 (Injector) shipped without closing this loop — the
   product is currently "directive injected, marker stripped" with
   the *attribution-of-the-stripped-tag* step missing.
2. **`MarkerStripTransform`'s mutation does not reach the wire.**
   ADR 017 §7 already names this: `ResponseFlow::push_bytes` /
   `finish` return `FlowOutput { events, side_effects }` but
   produce no `bytes` field. The proxy's response body still
   forwards the original upstream bytes (with markers). The
   `MarkerStripTransform` runs and its effect is invisible. Item 2
   shipped with this gap deliberately deferred to item 4.
3. **The §16 error contract is observable only via `tracing`.**
   When a codec or transform empty-returns + emits
   `SideEffect::Audit(AuditEvent { kind: Errored, .. })` per the
   ADR 015 §16 verification contracts, that audit goes into the
   per-flow vector that the engine drops. No file, no metric, no
   structured signal. The C-3 divergence check (§16.3) cannot be
   asserted in production because there is no sink to assert
   against.

Item 4 closes all three gaps with the same set of changes.

## 2. Decision

Three additions, one engine-wiring change, no breaking changes
to the trait surface.

### 2.1 `SideEffectSink` port

A single typed port in `noodle-core::layered` consuming the
existing `SideEffect` enum:

```rust
pub trait SideEffectSink: Send + Sync + 'static {
    /// Non-blocking. Implementations that need I/O must offload
    /// to a background task; the inspection path must never
    /// block on sink writes (mirrors the existing `AuditSink`
    /// non-blocking contract).
    fn record(&self, effect: SideEffect);
}
```

Driven adapters in `noodle-adapters`:

- **`TracingSink`** — emits one `tracing::event!` per `SideEffect`
  variant at level `INFO` for `Hint` / `Artifact`, `WARN` for
  `Audit { kind: Errored | InvariantViolation, .. }`, `DEBUG`
  for the rest. Default-on; ships in `noodle-proxy::tap_setup`.
- **`EventsJsonlSink`** — extends the existing JSONL tap so
  side-effects land in `events.jsonl` alongside frames + bodies.
  Required for the viewer follow-on.
- **`InMemorySink`** — `Arc<Mutex<Vec<SideEffect>>>` for tests.
- **`MultiSideEffectSink`** — composite/fan-out; one child failure
  does not stop the others. Mirrors the existing
  `MultiAuditSink` pattern.

### 2.2 New `SideEffect::Resolved` variant

The `Resolver` (existing free function `resolve(hints, config) ->
Resolved`) produces a `Resolved` map at end-of-flow. That map is
the **attribution record** — the thing the product is for. It
deserves its own typed variant on the side-effect bus rather than
being smuggled inside an `AuditEvent`'s `detail: serde_json::Value`:

```rust
pub enum SideEffect {
    Hint(Hint),
    Artifact(Artifact),
    Audit(AuditEvent),
    Resolved(ResolvedRecord), // new
}

pub struct ResolvedRecord {
    pub session: SessionId,
    pub flow_id: FlowId,
    pub at_unix_ms: u64,
    pub resolved: Resolved,
}
```

Adding a variant to a `pub enum` is a breaking change for
match-exhaustive consumers, but `SideEffect` has no external
consumers yet (sinks are item-4 work; this is the cleanest moment
to add the variant).

### 2.3 Engine wires Resolver at flow end

`InspectionEngine` holds:

- `Arc<dyn SideEffectSink>` (default: `Arc<TracingSink>` if none
  registered).
- `CategoryConfig` (default: hardcoded `CategoryConfig::default()`
  in `noodle-core` with the categories item 5 will populate, e.g.
  `tool`).

`ResponseFlow::finish` and `RequestFlow::finish` already drain
the per-flow side-effect buffer. The engine wrapper (the layer
above the flow, in `noodle-proxy::wirelog`) now:

1. Routes each drained `SideEffect` to the registered
   `SideEffectSink` (verbatim — `Hint`, `Artifact`, `Audit`).
2. Collects the `Hint`s into a `Vec<ContextHint>`.
3. Calls `resolve(&hints, &category_config) -> Resolved`.
4. Stores `Resolved` on the current `Session` (extends
   `InMemorySessionStore`'s entry — accumulates across the
   session's flows; later flow overrides earlier where categories
   collide, per ADR 004's max-confidence rule).
5. Emits a final `SideEffect::Resolved(ResolvedRecord)` to the
   sink for the current flow.

The Resolver call is at **end-of-flow**, not per event. The
side-effect drain order is: drain → fan to sink → run Resolver →
emit Resolved → done.

### 2.4 `ResponseFlow` gains symmetric encode (closes ADR 017 §7)

`ResponseFlow::FlowOutput` gains a `bytes: Vec<Bytes>` field
symmetric to `RequestFlow::RequestOutput`. The L4 encode runs
inside `ResponseFlow::push_bytes` / `finish` after the transform
chain produces the (possibly mutated) `BodyFrameEvent` stream.

Encode dispatch on `FrameSource` (existing 026.d discriminator)
and `EventSource` (ADR 017): `Upstream(raw)` replays verbatim,
`Mutated` / `Synthetic` re-serialises from structured fields.

`noodle-proxy::wirelog` substitutes the response body it forwards
to the client with the encoded bytes (mirrors the request-side
seam shipped in 18.6).

### 2.5 `CategoryConfig::default()` in `noodle-core`

A hardcoded default in `noodle-core::resolver`:

```rust
impl CategoryConfig {
    pub fn default() -> Self {
        Self {
            categories: [
                ("tool".into(), CategoryDef {
                    values: vec![], // open list for v1
                    detectors: vec!["marker".into(), "user_agent".into()],
                    default: None,
                }),
                // additional categories follow as item 5 lands.
            ].into(),
        }
    }
}
```

`InspectionEngineBuilder` takes an optional `CategoryConfig`
override; default is used otherwise. YAML loading is deferred to
a follow-on story (not item 4).

## 2.7 Applicability to the plugin topology

The `SideEffectSink` *port* defined in §2.1 is host-independent.
The file-backed concrete implementations
(`SideEffectsJsonlSink`, `RoundTripSink`, `TracingSink`,
`MultiSideEffectSink`) live in the `noodle-sinks` crate and are
proxy-host-only — they pull `tokio` and file I/O that the plugin
topology cannot use.

A plugin embedded via `noodle-detect` (ADR 039 §2.3) does not run
a `SideEffectSink` at all. Side effects accumulate on a
`SideChannelTx` during the `detect()` call and are returned to the
host gateway as fields of `AttributionFacts` (`hints`, `artifacts`,
`audits`, `resolved`). The host gateway then routes them to its
own telemetry collector — the equivalent of the proxy host's
`MultiSideEffectSink` lives on the host side, not in the plugin.

The trait surface — `SideChannelTx`, `Hint`, `Artifact`,
`AuditEvent`, `ResolvedRecord` — is shared. Plugin authors emit
through the same calls; only the drain target differs.

## 3. Consequences

### What changes

- **`noodle-core::layered`**: new `SideEffectSink` trait, new
  `SideEffect::Resolved` variant, new `ResolvedRecord` struct.
  `ResponseFlow::FlowOutput` gains `bytes`.
- **`noodle-core::engine` / `InspectionEngineBuilder`**: holds a
  sink + `CategoryConfig`.
- **`noodle-core::resolver`**: `CategoryConfig::default()` ships.
- **`noodle-core::store::Session`**: gains a `resolved: Resolved`
  field accumulating across flows in the session.
- **`noodle-adapters`**: new `sink::{TracingSink, EventsJsonlSink,
  InMemorySink, MultiSideEffectSink}` module.
- **`noodle-proxy::tap_setup`**: registers the default
  `MultiSideEffectSink { TracingSink, EventsJsonlSink }` when
  `NOODLE_LAYERED_CORE` is set.
- **`noodle-proxy::wirelog`**: drains response-flow side-effects,
  runs Resolver, substitutes encoded bytes onto the outbound
  response body.

### What does not change

- The trait shapes from ADR 015 (`Codec`, `Transform`) and the
  existing `SideEffect::{Hint, Artifact, Audit}` payload types.
- The §16 empty-on-error contract — already enforced; this ADR
  just gives the `AuditKind::Errored` emissions a real sink.
- The legacy `AuditSink` + `noodle-core::audit::AuditEvent` enum
  used by the pre-layered path. Not touched here; item 12 (flip
  layered → default) is where the legacy paths get removed.
- `RequestFlow` is unchanged. The request-side seam from 18.6
  already drains side-effects via tracing (debug log line); this
  ADR upgrades that to the real sink via the same engine drain
  path used for response, so the request side benefits without a
  separate code path.

### Cross-direction correlation (ADR 019 §2.5)

`ResolvedRecord.flow_id` ties a Resolved back to the flow that
produced its Hints. Future `inject→client` / `harvest←client`
capabilities (ADR 019) will be able to correlate solicited tool
calls with their results by matching against `flow_id` +
correlation-scope fields. The shape is forward-compatible; this
ADR does not implement those capabilities, only leaves the seam
typed correctly.

## 4. Alternatives rejected

- **A separate `AttributionSink` port for `Resolved`.** Rejected:
  proliferates ports for a small consumer set (sink + viewer +
  ledger). The same `SideEffectSink` carrying a typed
  `Resolved` variant is simpler and matches the "one bus per
  flow" frame from 015 §5.
- **`Resolved` smuggled inside `AuditEvent.detail` JSON.** Rejected:
  loses type safety; consumers have to know to look for a magic
  `kind` value and parse JSON. The whole point of the typed
  side-effect bus is that consumers can match on shape.
- **YAML `CategoryConfig` in v1.** Rejected: pulls config-file
  loading into item 4 and grows the surface for testing. Defer
  until a real reason to override the default surfaces.
- **Per-event Resolver call.** Rejected: Hints accumulate across
  the flow (`MarkerStripTransform` emits a Hint per stripped
  marker; a future `UserAgentDetector` emits one Hint per
  request). The Resolver needs the full Hint set to pick max-
  confidence per category. End-of-flow is the correct boundary.
- **Make `SideEffect::Resolved` a fifth variant separate from
  `Audit`.** *Adopted* — see §2.2. The rejected alternative was
  to overload `Audit { kind: AuditKind::Resolved, detail: ... }`,
  which would have meant downstream code switching on
  `AuditKind` rather than on the outer `SideEffect` variant.

## 5. Migration plan (slices)

Three sub-PRs, each independently green + tested:

### 5.1 Slice 031.a — sink port + Resolver hand-off

- Define `SideEffectSink` trait and `ResolvedRecord` struct.
- Define `SideEffect::Resolved` variant.
- Adapters: `TracingSink`, `EventsJsonlSink`, `InMemorySink`,
  `MultiSideEffectSink`.
- `CategoryConfig::default()` in `noodle-core::resolver`.
- `InspectionEngine` / `InspectionEngineBuilder` hold the sink +
  category config.
- Engine drains side-effects to sink at flow end, runs Resolver,
  emits `SideEffect::Resolved`, stores on `Session`.
- **Tests:**
  - `InMemorySink` records appended in order.
  - `MultiSideEffectSink` fans out; child failure isolated.
  - Engine-drain unit test: a flow with 3 Hints + 1 AuditEvent
    produces 4 sink calls + 1 Resolved.
  - Resolver-on-flow-end test: synthetic hints from multiple
    categories produce expected Resolved.
  - Empty hint set → empty Resolved (no errors, no defaults
    until config declares one).
  - Session-state test: Resolved accumulates across flows; per-
    flow buffer cleared between flows.

### 5.2 Slice 031.b — `ResponseFlow` encode (closes ADR 017 §7)

- `ResponseFlow::FlowOutput { events, side_effects, bytes }`.
- L4 encode runs inside `ResponseFlow::push_bytes` / `finish`.
- `noodle-proxy::wirelog` substitutes the encoded bytes onto
  the outbound response body.
- **Tests:**
  - Unmutated stream: `encode(decode(bytes)) == bytes` (the
    015 §2.1.1 round-trip invariant on the full response path).
  - Mutated token: encode honours `EventSource::Mutated` (re-
    serialise); upstream frames around it stay `Upstream(verbatim)`.
  - Empty input → empty output (§16 empty-on-error).
  - Proxy e2e: `MarkerStripTransform` end-to-end — assert on
    **client-visible response bytes**, marker absent. The exact
    test ADR 017 §7 deferred.
  - Proxy e2e: unmodified response stream → client bytes byte-
    identical to upstream.
  - Proxy e2e: unmodelled `content-encoding` on response →
    declined gracefully; client sees verbatim upstream bytes
    (parallel to the request-side §8 contract from ADR 018).

### 5.3 Slice 031.c — `CategoryConfig` plumbing + full-loop proof

- Stub `Detector` for the e2e — e.g. a `UserAgentDetector`
  mapping `User-Agent` → `tool` hint. (Production Detectors are
  out of scope for item 4; this is the demonstrator the e2e
  needs.)
- `tap_setup` wires `CategoryConfig::default()` into the engine.
- **The full-loop fail-before/pass-after e2e** (the milestone
  test): with the directive injected (story 030, shipped),
  `MarkerStripTransform` stripping (031.b), `UserAgentDetector`
  emitting a hint, and `Resolver` running, assert:
  1. Client-visible response bytes have no marker.
  2. `events.jsonl` carries an entry of shape
     `SideEffect::Resolved` with the right session +
     `tool=Claude Code` (or similar).
- This is the closing test for backlog item 4: the attribution
  product loop is now closed end-to-end.

## 6. Security considerations

- **No new attack surface.** The sink is an in-process bus; it
  emits to `tracing` and to the existing `events.jsonl` file.
  Both already exist and have established threat models.
- **No new credential handling.** The sink does not receive raw
  bodies; it receives extracted Hints, Artifacts, and Resolved
  records. `Artifact`'s payload is bounded by the emitting
  transform — that boundary is already audited.
- **Backpressure / DoS.** The sink contract is non-blocking
  (mirrors `AuditSink`). Adapters that do I/O must offload to a
  background task with a bounded queue and a drop-newest /
  drop-oldest policy on overflow. `EventsJsonlSink` will reuse
  the existing tap's bounded channel discipline; `TracingSink`
  inherits `tracing`'s own non-blocking behaviour.
- **Sensitive data in Hints.** Detectors emit data they
  extracted from the wire — a User-Agent string is innocuous; a
  Hint that carries an auth header substring would not be. The
  `SideEffectSink`'s `EventsJsonlSink` writes to a file: the
  same retention and access controls that apply to the existing
  tap's `bodies.jsonl` apply here. No new policy is introduced;
  the constraint is "don't extract things into a Hint that you
  wouldn't put in `events.jsonl`."
- **Session state.** Per-session `Resolved` lives in
  `InMemorySessionStore` — memory only, cleared on process
  restart. The accumulated attribution record is no more
  sensitive than the underlying request/response that produced
  it.
- **§16 audit emissions** become observable. This is a security
  *improvement*: silent-empty failures (the C-3 divergence
  check) now produce a structured signal. Operators can alert on
  `AuditKind::Errored` rate.

## 7. Open questions for follow-on stories

These are explicitly *not* in scope for item 4 and need their
own stories or ADRs:

- **Viewer panel for `Resolved`.** The data lands in
  `events.jsonl` and on `Session`; the viewer renders it. Out of
  scope; follow-on to story 031.
- **YAML `CategoryConfig` loader.** Out of scope; new follow-on
  story when a real reason to override the default surfaces.
- **Production `Detector` set.** `UserAgentDetector` is the
  demonstrator. Real detectors (tool / team / user) come with
  item 5 / story 032 (L5 coverage) and beyond. Detectors are not
  this ADR's job.
- **Async `Transform` variant.** Backlog item 9. When a
  Detector calls a model, it needs async. The sink + Resolver
  wiring is sync (matching ADR 015 §14.1 #2); the async story
  works on top of this.
- **`inject→client` and `harvest←client` capability paths.** ADR
  019 §2.5 named the correlation contract; this ADR makes the
  `flow_id` typing forward-compatible. The capabilities
  themselves are future work.

## 8. Cross-references summary

| Closes / depends on | Where |
|---|---|
| ADR 017 §7 engine-encode wiring gap | §2.4 above |
| ADR 015 §16 (`AuditKind::Errored` needs a sink) | §1, §2.1 |
| ADR 015 §5 (side-channel bus) | §2.1 |
| ADR 004 attribution model (Resolver semantics) | §2.3 |
| ADR 019 §2.5 (correlation scope) | §2.6 |
| Story 031 (the slices) | §5 |
| Backlog item 4 | overview row 4 |
