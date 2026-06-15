# ADR 021 — Detector vs Transform: a two-tier read-only/pipeline split

**Status:** accepted (lands with item 2 Detector slice; story 029).
**Date:** 2026-05-17.
**Supersedes:** none (clarifies, does not replace).
**Clarifies:** ADR 015 §3 (trait surface), ADR 019 §2 (capability
cells), story 029 §1 (which framed Detector/Injector/Filter as
"roles a Transform plays").

---

## Context

Story 029 ("Wire Transforms onto the layered path") was framed
around a single trait surface: `Detector / Injector / Filter` are
*roles* a `Transform` plays, not separate traits. The Filter slice
(`MarkerStripTransform`) and the Injector slice
(`AttributionInjector`) shipped under that framing without issue —
both operate on typed pipeline events (`NormalizedEvent` and
`NormalizedRequest` respectively), both fit `Transform<E>` cleanly.

The Detector slice surfaced a gap. The first detector we need is
`UserAgentDetector` (substring match on the request's `User-Agent`
header, emit a `tool` hint). It cannot be a `Transform` over any
existing pipeline event:

- `Transform<NormalizedRequest>` runs after the request codec has
  decoded the body into a vendor-agnostic shape. By design,
  `NormalizedRequest` carries `model` + `messages` + `system` —
  body content only. HTTP headers are gone by then. Adding a
  `headers: HeaderMap` field to `NormalizedRequest` would pollute
  the vendor-agnostic body abstraction with transport metadata.
- `Transform<Bytes>` doesn't exist and shouldn't — codecs decode
  bytes; transforms work on typed events.
- `Transform<RequestProbe>` is plausible (the probe carries
  headers) but extends the Transform trait surface to operate
  on a borrowed view of pre-decode state. That muddies "a
  Transform is a chain-of-responsibility node in the codec
  pipeline" — probes are not in the pipeline.

The honest fix is to admit there are **two kinds of read sites**:

1. **Pipeline reads** — `Transform<E>` operating on the typed
   event stream after a codec has run. Content-derived facts
   (system-prompt analysis, message-content patterns,
   tool-name extraction) belong here.
2. **Boundary reads** — read-only inspection of the request
   probe at flow open, before any body is decoded. Header-derived
   facts (User-Agent, custom `X-Tool-*` headers, auth-header
   shape) belong here.

Forcing both into one trait shape obscures which kind a given
adapter is, makes the trait's read surface ambiguous (does it see
headers? bodies? both?), and makes detector wiring depend on
introducing artificial pipeline events.

## Decision

Add a second trait, **`RequestDetector`**, alongside `Transform`.
Both live in `noodle-core::layered`. The split is structural,
not cosmetic:

| | `Transform<E>` | `RequestDetector` |
|--|--|--|
| Runs when | Per-event during codec pipeline | Once at request-flow open |
| Reads | Owned `E` (event) | Borrowed `&CodecProbe` |
| May mutate | Yes (returns `Vec<E>`) | No (read-only) |
| Emits | Side effects (Hint/Artifact/Audit) and/or events | Side effects only (Hint typical) |
| Failure | Empty-on-error (ADR 015 §16) | Silent — return without emitting |
| Statefulness | Per-flow instance | Stateless factory call |
| Examples | `MarkerStripTransform`, `AttributionInjector` | `UserAgentDetector` |

The `RequestDetector` trait shape:

```rust
pub trait RequestDetector: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn detect(&self, probe: &CodecProbe<'_>, side: &mut SideChannelTx<'_>);
}
```

Stateless by construction. No `open()` factory step — a detector
is its own per-flow worker because the only state it has is the
configuration set at registration time (and the probe is the
input).

`InspectionEngine` holds `Vec<Arc<dyn RequestDetector>>` registered
via `InspectionEngineBuilder::request_detectors`. At
`open_request_flow` time, the engine runs every detector against
the probe and stashes emitted side-effects on the new `RequestFlow`;
they merge into the `RequestFlow::finish` output and reach the sink
via the existing `drain_to_sink` path.

Detectors do not gate flow opening — a detector that emits nothing
is equivalent to no detector. The engine still opens the flow on
codec match; the detector's emissions ride alongside.

## Why not one of the alternatives

- **Add `headers: HeaderMap` to `NormalizedRequest`.** Pollutes the
  vendor-agnostic body abstraction with HTTP metadata that codecs
  intentionally strip. Future per-domain request codecs would have
  to decide whether to preserve, forward, or discard headers in
  their decode contract — answering a question that should never
  be asked.
- **`Transform<RequestProbe>`.** Extends `Transform` to work on
  borrowed pre-decode views. The Transform trait's invariants are
  defined for owned events flowing through a pipeline; a probe is
  neither. Also opens "should there be a `Transform<&ResponseProbe>`"
  and similar trait-surface drift.
- **Repurpose legacy `crate::detector::Detector`.** The legacy
  trait reads via a `FlowResolver` port that gives access to
  fully-buffered bodies and response headers — a heavier surface
  than the layered request-flow open site has (the response body
  hasn't arrived yet). Reusing it would force the layered engine to
  either implement `FlowResolver` (which it doesn't have the data
  to populate) or invent a partial implementation. Cleaner to add
  a small, focused trait that matches the layered engine's read
  shape.

## Consequences

**Required follow-on (this slice):**

- New `RequestDetector` trait + `RequestDetectorRegistry` in
  `noodle-core::layered`. Same registration pattern as `Codec`
  and `Transform`.
- `InspectionEngine` gains `request_detectors: Vec<Arc<dyn RequestDetector>>`
  and `InspectionEngineBuilder::request_detectors`.
- `RequestFlow` gains a `detector_effects: Vec<SideEffect>` field
  populated at flow open; `RequestFlow::finish` appends them to
  the returned `RequestOutput.side_effects`.
- `UserAgentDetector` (in `noodle-adapters`) implements
  `RequestDetector`. The substring table currently inline in
  `noodle-proxy::wirelog::user_agent_hint` moves with it.
- `noodle-proxy::wirelog` loses the inline `user_agent_hint`
  call (and the v1 stand-in comment block). The detector
  reaches the sink via the existing engine drain path.

**Future:**

- `ResponseDetector` may follow with the same shape over a
  `ResponseProbe<'a>` (status + response headers, available at
  `open_response_flow` time). Not built v1; introduced when
  the first response-header-derived signal needs it.
- Per-cell binding per ADR 019 §2 (detector registered against
  a specific `(domain, endpoint, direction)` cell rather than
  globally). For v1 we register detectors globally — the
  UA detector's table is host-neutral by intent. Per-cell
  binding is a registry refactor when the second detector
  arrives and proves the need.

## Security considerations

Detectors are read-only; they cannot mutate the request, the
response, or any caller state. They emit only into the engine's
side-effect channel, which is already trusted to carry
`Hint`/`Artifact`/`Audit` data downstream to the sink. No new
attack surface.

A detector that panics or hangs would block flow opening on its
calling thread — the engine runs them synchronously per request.
We treat detectors as trusted code (the operator compiled them
in), same posture as codecs and transforms. Panic isolation at
the multi-sink boundary already exists; if cross-detector
isolation becomes a concern, wrap the `detect` call in
`catch_unwind` at the engine — out of scope for v1.

## Test plan

- Unit: `RequestDetector` object-safe; `RequestDetectorRegistry`
  registers + iterates in order.
- Unit: `UserAgentDetector` ported from the existing
  `user_agent_hint_tests` module in `wirelog.rs` (every existing
  case must still pass; same UA strings, same canonical names,
  same confidence values).
- Integration: a request flow opened against a probe with a
  Claude-Code UA produces a `Hint { category: "tool", value:
  "Claude Code", source: "user_agent" }` in the
  `RequestOutput.side_effects` returned from `finish()`.
- E2e: existing `e2e_full_attribution_loop.rs` continues to
  pass — the loop still resolves `tool: "Claude Code"` from a
  UA-bearing request. The hint arrives via the detector path,
  not the inline `wirelog` fallback.

## Open questions deferred

- Async detectors (`async fn detect`) — needed when a detector
  calls a model or a remote registry. Same posture as backlog
  item 9 (async transforms): not v1; revisit when a real
  async detector lands.
- Multi-detector ordering / dedupe — when two detectors emit
  hints for the same category (e.g. UA says "Claude Code",
  body-content says "Cursor"), the Resolver's confidence
  ranking already handles disambiguation per ADR 004. No
  detector-side dedupe needed.
