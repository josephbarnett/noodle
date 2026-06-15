# 040 — Round-trip telemetry records and correlation IDs

**Status:** Shipped via sub-stories 040.a (PR #87), 040.b (PR #88), 040.c (PR #93).
**Depends on:** none (foundation; see [`000-overview.md`](000-overview.md) item 24, P0 — "Job one for shipping")
**Design refs:**
[`docs/adrs/023-roundtrip-telemetry-records-and-correlation-ids.md`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md)
(primary — pins `roundtrips.jsonl`, `RoundTripSink`, the four correlation IDs, and turn / agent-run boundary detection),
[`docs/adrs/020-side-effect-sink-and-resolver-wiring.md`](../adrs/020-side-effect-sink-and-resolver-wiring.md)
(§2.1 `SideEffectSink` port; §5.1 JSONL format),
[`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md)
(two-process boundary; this story is the producer side),
[`docs/adrs/027-tap-jsonl-boundary-format.md`](../adrs/027-tap-jsonl-boundary-format.md)
(sibling boundary; same correlation contract applies),
[`docs/adrs/028-session-store-and-marking-detector-contract.md`](../adrs/028-session-store-and-marking-detector-contract.md)
(source of `session_id`; this story extends the contract with `turn_id` + `agent_run_id`),
[`docs/adrs/031-embellishment-processor.md`](../adrs/031-embellishment-processor.md)
(downstream consumer; depends on the records this story emits),
[`docs/adrs/004-attribution-model.md`](../adrs/004-attribution-model.md)
(`Hint` / `Resolved` semantics that fold into the per-round-trip record),
[`docs/adrs/008-session-hierarchy.md`](../adrs/008-session-hierarchy.md)
(round trip → turn → agent run → session definitions this story ports into the data plane).

---

## 1. Value delivered

After this story ships, a downstream consumer (the downstream telemetry consumer primary, any
OTel-shape consumer by generalisation) can tail **one file**,
`~/.noodle/roundtrips.jsonl`, and consume **one self-contained record
per LLM round trip** — request meta + response meta + extracted
markers + Resolved attribution + token usage + latency + the
contributing Hints / Artifacts / Audits — already correlated by
`session_id` / `agent_run_id` / `turn_id` / `flow_id`. No
client-side reconstruction across the four `SideEffect` line shapes,
no joining against `tap.jsonl` to recover identity. The same four
correlation IDs land on every record in every data-plane file the
proxy writes, so cross-file joins ("what wire bytes produced this
Resolved?") collapse to a key lookup. This is the surface the downstream telemetry consumer
needs to compute per-turn cost, per-agent-run pace, and per-session
attribution without holding state.

## 2. Acceptance criteria

1. `~/.noodle/roundtrips.jsonl` is written by the proxy. Default
   location matches ADR 023 §2.1; configurable through the same
   plumbing as `SideEffectsJsonlSink`.
2. One JSONL line per completed HTTP round trip, written at flow
   finish. Schema matches ADR 023 §4 verbatim.
3. Every record in `roundtrips.jsonl` carries all four IDs:
   `session_id`, `agent_run_id`, `turn_id`, `flow_id`.
4. Every record in `side_effects.jsonl` (existing file) also carries
   all four IDs after this story ships — additively, no removed
   fields.
5. Every record in `tap.jsonl` carries the same four IDs (already
   has `session_id` + `turn_id` via `marks`; this story adds
   `agent_run_id` + `flow_id` symmetrically).
6. Turn boundaries are detected per ADR 023 §2.4 (fresh-session,
   `end_turn` / `max_tokens` boundary, etc.); `turn_id` is stable
   across continuation round-trips within a turn.
7. Agent-run boundaries are detected per ADR 023 §2.5 (system-prompt
   hash change); `agent_run_id` is stable across the run's turns.
8. Hot-path budget: no allocation per record beyond what already
   lands on tap.jsonl; round-trip emission is non-blocking with
   the same drop-on-full posture as `SideEffectsJsonlSink`. Concurrent
   per-flow buffers are bounded by a small constant times
   `concurrent_flows`.
9. E2E test: real `claude -p "..."` exec through the release
   binary produces a `roundtrips.jsonl` whose records correlate
   1:1 with `/v1/messages` records on `tap.jsonl` by `flow_id` and
   carry the expected `session_id` / `agent_run_id` / `turn_id`.
10. E2E test (multi-turn): a tool-using prompt produces multiple
    round trips with one shared `turn_id` until `stop_reason ≠
    tool_use`; the next user input mints a new `turn_id` under the
    same `agent_run_id` and `session_id`.

## 3. Abstractions introduced or refined

- **`RoundTripRecord`** (new, `noodle-core::layered` or a sibling
  module): the self-contained per-flow record shape. ADR 023 §4
  pins the field list. Strongly typed; serde-serialisable.
- **`RoundTripSink`** (new, `noodle-adapters::sink`): a
  `SideEffectSink` impl that buffers per-flow `Hint` / `Artifact` /
  `Audit` indexed by `flow_id`, assembles the `RoundTripRecord` on
  `Resolved` arrival (or an explicit flow-end notification from
  the engine), and writes one JSONL line. Composes alongside
  `SideEffectsJsonlSink` and `TracingSink` under
  `MultiSideEffectSink` — sibling sinks fed from the same bus.
- **`Correlation`** (new, shared across all data-plane records):
  the four-ID block (`session_id`, `agent_run_id`, `turn_id`,
  `flow_id`) stamped onto every record the proxy writes. Existing
  `Hint` / `Artifact` / `AuditEvent` / `ResolvedRecord` schemas
  refine to include this block. `tap.jsonl`'s `marks` block
  extends to carry `agent_run_id` + `flow_id` (already carries
  `session_id` + `turn_id`).
- **Turn / agent-run detector** (refinement of
  `noodle-core::MarkingDetector`): the existing
  `AnthropicMarkingDetector` mints `turn_id`; this story extends it
  to also mint `agent_run_id` on detected system-prompt hash
  changes per ADR 023 §2.5.

Seam for dependency injection: `RoundTripSink` takes the writer
path + a `Clock` trait for `at_unix_ms` stamping. Tests substitute
a `FakeClock` + an in-memory writer; no integration plumbing.

## 4. Patterns applied

- **Composite** — `MultiSideEffectSink` composes
  `SideEffectsJsonlSink`, `RoundTripSink`, and `TracingSink` as
  siblings, all draining the same `SideEffect` stream.
- **Aggregator** — `RoundTripSink` buffers per-`flow_id` events
  until the `Resolved` arrival signals flow finish, then assembles
  one consolidated record. (Symmetric to how `EventsAccumulator`
  on the body tee accumulates SSE events for `tap.jsonl`.)
- **Adapter** — the `Correlation` block is the adapter contract
  between the engine's internal ID minting and the wire shape every
  downstream consumer reads.

## 5. Test plan

Each acceptance criterion maps to at least one test below.

- **Unit (AC #1, #2):** `RoundTripSink` against a `FakeClock` and
  an in-memory writer. Drive one synthetic flow's worth of
  `Hint` / `Artifact` / `Audit` / `Resolved` through it; assert
  exactly one line is written at the moment `Resolved` arrives,
  buffer is dropped, schema matches ADR 023 §4 verbatim.
- **Unit (AC #3, #4, #5):** schema golden tests on each of
  `RoundTripRecord`, `JsonlEntry::{Hint, Artifact, Audit,
  Resolved}` (the on-disk shapes from `SideEffectsJsonlSink`),
  and the `marks` block from `tap.jsonl`. Each asserts the four
  correlation fields are present and serialised under the names
  ADR 023 §2.3 pins.
- **Unit (AC #6):** `AnthropicMarkingDetector` driven through
  the four turn-boundary cases from ADR 023 §2.4 (fresh-session,
  end_turn, max_tokens, tool_use continuation). Assert
  `turn_id` mints exactly when expected.
- **Unit (AC #7):** same detector driven through system-prompt
  hash transitions per ADR 023 §2.5. Assert `agent_run_id` mints
  exactly when expected.
- **Property (AC #8):** flood the sink with N synthetic flows at
  M `SideEffect` records each; assert peak memory bounded by
  `O(N × buffer_per_flow)` and the engine's hot path never blocks
  on the sink's mpsc channel (drop counter increments instead).
- **Integration (AC #9):** the existing exec-claude e2e
  harness. Drive `claude -p "..."` through the release binary;
  assert one `roundtrips.jsonl` line per `/v1/messages` record on
  `tap.jsonl`, joinable by `flow_id`, and `session_id` /
  `agent_run_id` / `turn_id` agree across the three files.
- **Integration (AC #10):** drive a multi-tool-use prompt
  (`"list /tmp and identify owner and size"`); assert the
  continuation round-trips share `turn_id` until
  `stop_reason ≠ tool_use`, then the next user input opens a new
  `turn_id` under the same `agent_run_id`.

## 6. PR scope

This story does **not** fit one PR. Split into three sub-stories
before starting; this file remains the parent and tracks them all.

- **040.a — Correlation block on every existing data-plane
  record.** Extend `Hint` / `Artifact` / `AuditEvent` /
  `ResolvedRecord` schemas with the four IDs; extend
  `tap.jsonl`'s `marks` block with `agent_run_id` + `flow_id`;
  thread minting from the engine drain path. Includes the schema
  golden tests (AC #3, #4, #5). Reviewable in one sitting.
- **040.b — `RoundTripSink` + `RoundTripRecord`.** New sink
  composed under `MultiSideEffectSink`; new on-disk record shape
  per ADR 023 §4; writes `roundtrips.jsonl`. Includes the
  per-flow buffering unit tests + the AC #9 integration. Depends
  on 040.a.
- **040.c — Turn + agent-run boundary detection.** Extend the
  `AnthropicMarkingDetector` to mint `agent_run_id` on
  system-prompt hash change and refine the existing `turn_id`
  detection against ADR 023 §2.4. Includes the AC #6, #7, #10
  tests. Depends on 040.a.

Each sub-story file is created at implementation start, not now —
ADR 023 has the shape; this parent has the value gate. The PR
scope of each sub-story is "one reviewable PR."

## 7. Out of scope

- **OTLP emission.** Lives in [`043`](043-shipper-handoff-contract.md)'s
  `noodle-shipper` per the file-boundary architecture
  (`docs/diagrams/system-architecture.drawio`). This story emits the
  files (`roundtrips.jsonl`, `side_effects.jsonl`) that `noodle-embellish`
  consumes; the shipper reads the resulting rollups SQLite and emits OTLP
  to the collector. **The earlier in-proxy `OtlpSideEffectSink` (slice 041)
  is retired.**
- **Embellishment processor schema mapping** to
  `ai-telemetry` v0.0.2. ADR 031's reference processor depends on
  this story's correlation contract; tracked separately.
- **OTel collector** (identity resolution, cost-rate-card,
  redaction, sampling). Lives in its own repo per ADR 022 §2.
- **Per-OS entry transport** + **watchdog** + **fail-open** (ADR
  023, ADR 024 — design ADRs, not feature 023). Endpoint-product
  concerns; this story assumes explicit `HTTPS_PROXY=…` opt-in.
- **System Keychain CA install** (feature 025).
- **`side_effects.jsonl` retirement.** The two files are siblings
  per ADR 023 §2.2 — both stay; this story does not consolidate
  them. A future story may.
