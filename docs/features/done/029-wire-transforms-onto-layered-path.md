# 029 — Wire `Transform` capabilities onto the layered path
(backlog item 2)

**Status:** **done** — all three slices (Filter, Injector,
Detector) shipped. Detector slice landed as
`RequestDetector` (ADR 021), not as another role of
`Transform`; story 029 §1's "Detector is a role a Transform
plays" framing is clarified by ADR 021 as a two-tier split.
**Depends on:** done/026 (`Codec` + `Transform` trait surface),
ADR 017 (EventSource provenance — landed)
**Design refs:**
[`docs/adrs/004-attribution-model.md`](../adrs/004-attribution-model.md)
(role definitions: Detector / Injector / Filter — now realised
as `Transform`s, not separate traits),
[`docs/adrs/015-layered-codec-architecture.md`](../adrs/015-layered-codec-architecture.md)
§3 (trait surface), §16 (error contract),
[`docs/adrs/017-event-source-mutation-provenance.md`](../adrs/017-event-source-mutation-provenance.md)
(provenance enabling encode-side fidelity for mutated events)
**Backlog row:** item 2 in
[`features/000-overview.md`](000-overview.md) — "Restate
`Detector`/`Injector`/`Filter` → `Transform`; wire marker-strip +
attribution-injector onto layered path."

---

## 1. Value delivered

After this story, the layered core stops being a read-only
decoder. The marker-strip Filter, the attribution Injector, and
(when added) hint Detectors all run as `Transform`s on the live
flow. The model's tagged response markers are removed from the
client-visible bytes; the directive lands in the outbound request;
detectors emit hints to the side channel. This is the conversion
from "we can see the traffic" to "we can rewrite the traffic and
emit attribution facts."

## 2. Acceptance criteria

Filter slice — **done** (ADR 017 §2.1–2.4, PR #34):
1. `EventSource::{Upstream(ProviderChunk), Mutated}` discriminator
   on `NormalizedEvent::Token`, `ToolCall`, `Metadata`. ✅
2. `LayeredAnthropicCodec::encode` honours provenance: `Upstream`
   replays the original chunk verbatim; `Mutated` re-serialises
   from structured fields. ✅
3. `MarkerStripTransform: Transform<NormalizedEvent>` is a
   faithful port of `MarkerStripFilter`/`MarkerScanner` with all
   partial-match carry-buffer behaviour preserved. ✅
4. End-to-end test asserting on **client-visible output bytes**
   (not `Token.text`) that `<noodle:*>…</noodle:*>` markers are
   stripped from the wire. ✅

Injector slice — **done** (ADR 018, PR #36):
5. `AttributionInjector: Transform<NormalizedRequest>` registered
   at `(api.anthropic.com|claude.ai, completion, request→upstream)`
   cells; first request in a session gets the directive. ✅

Detector slice — **done** (ADR 021):
6. ✅ `UserAgentDetector` ships in `noodle-adapters::request_detector`
   as a `RequestDetector` (ADR 021's new trait shape, sibling
   to `Transform`). Maps `User-Agent` to a `tool` Hint via the
   substring table ported from the v1 inline
   `user_agent_hint` stand-in.
7. ✅ Hint emission observable via the side-effect sink: the
   `e2e_full_attribution_loop` test asserts a UA-derived `tool`
   Hint with `source="user_agent"` lands in
   `side_effects.jsonl` and that the Resolver produces a
   `Resolved { tool: "Claude Code" }` from it.

Engine response-encode wiring — **pending, scoped to story 031**:
8. The mutated bytes from the Filter slice actually reach the
   client. ADR 017 §7 flags the gap: `ResponseFlow` is
   decode-only today; symmetric encode wiring lands in story 031
   (item 4).

## 3. Abstractions introduced or refined

- `Transform<Event>` as the single trait shape for what 005 called
  "Detector / Injector / Filter" — these are *roles* a `Transform`
  plays, not separate traits.
- `EventSource` discriminator (ADR 017) — type-enforced provenance
  so encode can replay or re-serialise based on whether the event
  was mutated.

## 4. Patterns applied

- **Strategy** — each `Transform` is a strategy in the per-flow
  chain.
- **Chain of Responsibility** — `TransformRegistry` orders the
  chain; events thread through transform 1 → transform 2 → …
- **Provenance** — `EventSource` discriminator makes "I have raw
  bytes" vs "I must re-serialise" type-enforceable.

## 5. Test plan

- ✅ Filter slice: `e2e_filter_strips_markers.rs` (client-bytes
  assertions, not `Token.text` assertions).
- ✅ Injector slice: `e2e_request_inject.rs` (fail-before /
  pass-after for both Anthropic and claude.ai).
- Pending Detector slice: unit test emitting hint → side-effect
  buffer; integration test once story 031's sink is in place.

## 6. PR scope

Three landed PRs (Filter); one landed PR (Injector); one
landed PR (Detector — `RequestDetector` trait + registry in
`noodle-core::layered`, `UserAgentDetector` adapter in
`noodle-adapters`, engine wiring at `open_request_flow`,
inline `user_agent_hint` removed from `wirelog.rs`, e2e
re-targets the new path).

## 7. Out of scope

- Side-effect sink wiring → story 031 (item 4).
- Resolver wiring → story 031 (item 4).
- Response-encode plumbing through `ResponseFlow` → story 031
  (item 4).
- L5 coverage for `tool_use` / usage / billing → story 032
  (item 5).
- Non-streaming response support → story 033 (item 6).
