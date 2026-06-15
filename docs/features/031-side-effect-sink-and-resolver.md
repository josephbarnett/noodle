# 031 — Side-effect sink + Resolver wiring + response-encode
(backlog item 4)

**Status:** ADR landed (ADR 020); slice 031.a not yet started.
**Depends on:** done/026 (`Codec` + `Transform` traits),
029 (Detector slice — sink is meaningless without a hint
producer; the marker-strip Filter already exists), 030
(request inject pipeline shipped).
**ADR:**
[`docs/adrs/020-side-effect-sink-and-resolver-wiring.md`](../adrs/020-side-effect-sink-and-resolver-wiring.md)
(pins SideEffectSink port shape, ResponseFlow encode-extension
contract, Resolver-drain timing, CategoryConfig source, slice
plan).
**Other design refs:**
[`docs/adrs/004-attribution-model.md`](../adrs/004-attribution-model.md)
(Resolver definition, category config shape),
[`docs/adrs/015-layered-codec-architecture.md`](../adrs/015-layered-codec-architecture.md)
§5 (side-channel buses), §16 (error contract — `AuditKind::Errored`
emissions need a sink to land in),
[`docs/adrs/017-event-source-mutation-provenance.md`](../adrs/017-event-source-mutation-provenance.md)
§7 (engine-encode wiring gap explicitly flagged as item 4 work;
closed by ADR 020 §2.4 + slice 031.b),
[`docs/adrs/019-endpoint-routed-capability-dispatch.md`](../adrs/019-endpoint-routed-capability-dispatch.md)
§2.5 (correlation scope — informs sink + session-state plumbing;
ADR 020 §2.6 makes `ResolvedRecord.flow_id` forward-compatible
with future correlation-scope capabilities).
**Backlog row:** item 4 in
[`features/000-overview.md`](000-overview.md) —
"Side-effect sink + `Resolver` wiring (+ viewer panel)."

---

## 1. Value delivered

After this story, the extracted attribution tag stops going
nowhere. The end-to-end loop closes:

1. Directive injected outbound (already shipped, story 030).
2. Model emits the tag.
3. `MarkerStripTransform` strips it from the wire (already
   shipped, story 029 Filter slice) **and the mutated bytes
   actually reach the client** (this story — the encode-wiring
   gap ADR 017 §7 flagged).
4. Detectors emit hints to the side channel (this story — the
   sink lets us observe them; first Detector ships in story 029's
   Detector slice).
5. End-of-flow, the engine drains the side channel into a
   `SideEffectSink` and runs `Resolver` over the hints to produce
   a `Resolved` attribution record.
6. The `Resolved` record is observable (in `events.jsonl`,
   tagged on the session) and ready for the viewer follow-on
   to render.

This is the milestone that turns noodle from "a proxy that
rewrites traffic" into "a proxy that emits attribution facts."

## 2. Acceptance criteria

1. `SideEffectSink` port in `noodle-core::layered` consuming
   `SideEffect { Hint | Artifact | Audit }`. Three adapters in
   `noodle-adapters`: `TracingSink`, `EventsJsonlSink` (extends
   the existing tap output), `InMemorySink` (tests). A
   `MultiSideEffectSink` composite, matching the existing
   `MultiAuditSink` / `MultiWireSink` pattern.
2. `ResponseFlow` gains an encode path symmetric to `RequestFlow`:
   `decode → transform → encode → bytes`. Honors
   `EventSource::Upstream` (replay raw verbatim) vs `Mutated`
   (re-serialise from structured fields) per ADR 017 §2.2.
3. `noodle-proxy::wirelog` substitutes the response-side mutated
   bytes onto the response body it forwards to the client (the
   `MarkerStripTransform`'s output actually reaches the wire).
4. Engine drains the per-flow side-effect buffer at flow end and
   (a) routes every `SideEffect` to the `SideEffectSink`, (b)
   feeds the `Hint`s to `resolve(hints, &category_config)`.
5. The resulting `Resolved` map is (a) emitted as a structured
   `AuditEvent` on the sink, (b) stored on `Session` so a
   session accumulates attribution state across its flows.
6. `CategoryConfig::default()` ships with the categories item 5
   will populate (`tool`, etc.); the engine builder takes an
   optional override. YAML-loading is deferred to a follow-on
   story.
7. End-to-end test (fail-before / pass-after): with a stub
   Detector emitting `tool=Claude Code` and the Filter slice in
   place, the test asserts (a) the client-visible response bytes
   no longer contain the marker, (b) `events.jsonl` carries an
   attribution record with `tool=Claude Code` keyed by the flow's
   session.

## 3. Abstractions introduced or refined

- **`SideEffectSink`** port — typed bus consumer; new hexagonal
  port for what was previously surfaced only via `tracing`.
- **`ResponseFlow` encode extension** — closes ADR 017 §7's gap;
  makes the response direction symmetric to the request side
  shipped in story 030.
- **`Session` state extension** — accumulates `Resolved` across
  flows in a session.
- **`CategoryConfig`** — first real consumer of ADR 004's
  resolution config; default-shipped, override-able at build.

## 4. Patterns applied

- **Strategy** — `SideEffectSink` adapters; the engine doesn't
  care which one is wired.
- **Composite** — `MultiSideEffectSink` aggregating sinks.
- **Observer** — sinks observe side-effects without participating
  in the pipeline's data path.
- **Symmetry** — `ResponseFlow` mirrors `RequestFlow`; the engine
  model stays coherent.

## 5. Test plan

- Unit tests for each `SideEffectSink` adapter.
- Unit tests for `ResponseFlow::encode` honoring `EventSource`.
- Integration test on `MarkerStripTransform` end-to-end with the
  new encode path: marker stripped from client bytes (not just
  from `Token.text`).
- E2E with stub Detector → Resolver → events.jsonl assertion.

## 6. PR scope

Likely three PRs to keep review surfaces tight:

- **031.a** — `SideEffectSink` port + adapters + engine drain
  wiring + `Resolver` hand-off (no `ResponseFlow` change yet).
- **031.b** — `ResponseFlow` encode extension + proxy body
  substitution (closes ADR 017 §7).
- **031.c** — `CategoryConfig` plumbing + e2e proof.

## 7. Out of scope

- Viewer panel rendering the `Resolved` records — deferred to a
  follow-on; the data will be in `events.jsonl` and on
  `Session`, so the viewer follow-on is purely UI work.
- YAML config-file loading for `CategoryConfig` — follow-on.
- L5 coverage of `tool_use` / usage / billing — story 032.
- Non-streaming response support — story 033.
- Async `Transform` variant (cost-classifier transforms that
  call a model) — backlog item 9.
