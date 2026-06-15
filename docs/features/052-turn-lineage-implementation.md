# ADR 052 — value cadence (implementation sequence)

Delivery plan for the canonical design in
[`docs/adrs/052-turn-run-lineage-frame-tree.md`](../adrs/052-turn-run-lineage-frame-tree.md).
This does not alter the design; it orders the build by **what can be validated
against the real product** after each slice, and it is honest about the
design's own verification scope: the §6 algorithm is reproduced on the wire
for **single-turn, single-session** topologies; multi-turn ROOT re-entry,
per-session partitioning, and the compactor side-call signal are **unproven
pending captures** (ADR §9). The cadence reflects that — the slices that prove
those are explicitly capture-gated.

## How each slice is proven

Every slice's acceptance is a **deterministic, no-auth regression test**
(golden-replay over the committed captures), not a rancher eyeball. The
`.mitm` captures are gitignored (live bearer tokens); the committed artifact is
the **sanitized fixture** (`tests/fixtures/adr_048/*.fixture.json`, hashes +
structural facts only, produced by `tools/extract_capture_fixture.py`). The
Python oracle (`tools/validate_frame_tree.py` / `analyze_052.py`) is the
**oracle that generates the goldens**; the Rust golden test is what guards the
shipped detector.

## Confirmation gate

**V2 changes interfaces and spans packages** — the `MarkingDetector` trait
inputs (new wire signals), the `WireMarks` §5 reshape, and the
`tap.jsonl → sqlite DDL → OTLP → viewer` marks contract. Per the repo autonomy
model this is a plan-then-confirm change. V1 (below) is additive and needs no
gate; **V2 must be confirmed before the detector rewrite lands.**

---

## V1 — The fail-before golden spine (foundation, no interface change)

Build the deterministic gate before changing any product code. After V1 the
repo has a checked-in golden that **encodes the §6-correct marks** and **fails
against today's shipped detector** (per-`stop_reason` turns, system-hash
identity) — proving the change when V2 turns it green.

- **Extend the fixture extractor to v5.** `tools/extract_capture_fixture.py`
  currently emits canonical-system-hash, first-user-text hashes, stop_reason,
  tool_use names/ids/prompt-hashes, and a tool_result *count*. Add the §6
  signals (all sanitization-safe — hashes / enums / ids, never raw text):
  - `max_tokens` (the quota wrapper signal, `mt==1`);
  - `request_tool_result_ids` (the actual `tool_use` ids the request answers —
    needed for CHAIN routing; these are `toolu_…` ids, not content);
  - `trailing_wrapper_kind` ∈ {`quota`,`session`,`transcript`,`suggestion`,`none`}
    — classification of the trailing-user text prefix (no text emitted);
  - `message_sig` — ordered hash-chain of message identities (role +
    content-hash; tool blocks by id) for `extends_root`.
- **Regenerate the five fixtures** (bash-loop, task-subagent,
  parallel-subagents, quota-and-title, long-session-compaction) at v5.
- **Emit the §5 goldens** `tests/fixtures/adr_052/expected_marks/<capture>.json`
  (per round-trip: `role`, `frame_id`, `parent_frame_id`, `depth`, `turn_id`)
  from the proven §6 oracle.
- **Add the fail-before Rust test** asserting the shipped detector's marks
  against the §5 goldens for the single-turn captures — **RED on `main`.**
- **Acceptance:** the golden test exists, is deterministic/no-auth, and is RED
  against the current detector; the v5 fixtures contain the §6 signals.

## V2 — "This whole multi-agent exchange is one turn; three parallel agents are three frames." (interface gate)

The core algorithm in the product. After V2 the proxy stamps §5 marks and the
V1 golden goes GREEN on bash/task/parallel.

- **Reshape `WireMarks` → §5** (`noodle-core/src/wire.rs`): `agent_run_id` →
  `frame_id`; the four `parent_*` collapse to `parent_frame_id`; add `role`,
  `depth`. Propagate through `noodle-tap` `TapMarks`, sqlite DDL, the shipper
  OTLP mapping, and the viewer `DecodedMarks`/`SideEffectCorrelation` (rename
  only; render later in V5).
- **Extend the `MarkingDetector` trait** (`noodle-core/src/marking.rs`) to carry
  the new wire signals (max_tokens, request tool_result_ids, trailing wrapper
  kind, message_sig) and have the proxy extract them on the wire.
- **Rewrite `AnthropicMarkingDetector`** (`noodle-adapters/src/marking/anthropic.rs`)
  to §6: per-session frame tree, CHAIN → SPAWN → ROOT, `extends_root` /
  `is_harness_wrapper` / `genuine_user_text`. Retire system-hash slots, the
  `pending_children` LIFO, and per-`stop_reason` turn minting.
- **Acceptance:** the V1 golden is GREEN for parent-bash-loop / task-subagent /
  parallel-subagents — RT3→RT15 one `turn_id`, three distinct `frame_id`s under
  ROOT, RT16 `role=side_call`. Delivers FR1–FR4, FR6 at the source.

## V3 — Per-session isolation + side-call catalog hardening

Closes two gaps the ADR §9 marks unproven, on the captures that exercise them.

- **Partition all detector state by `session_id`** — the shipped detector and
  the oracle both keep one global state (ADR §8). Key `frames`/`pending_*`/
  `root_sig`/turn by session.
- **Positive compactor classification** — add a positive side-call signal
  (candidate: `mt==None` + non-streaming) so FR4 stops relying on fallback.
- **Acceptance:** golden-replay over `long-session-compaction` (two session ids
  `790d7283`/`d8df40a6`): the quota RT and compactor RT are `side_call`, the
  real turn is isolated to its session, no cross-session frame leakage.

## V4 — "Multi-turn, proven on the wire." (capture-gated — closes G1/FR3/FR5)

The branch the ADR cannot yet prove. **Blocked on a new capture** (a live,
auth-bound recording — a human action).

- **Record `captures/max/parent-multiturn.mitm`** — `mitmdump -w …` driving
  `claude` through ≥3 user turns in one session (turn 1: Bash + one sub-agent;
  turn 2: same session, new prompt, tool use; turn 3: two parallel sub-agents),
  letting quota / title-gen / security-monitor / **suggestion-mode** side-calls
  interleave.
- Extract v5 fixture; add its golden: **N distinct `turn_id`s**, one
  `session_id`, ROOT persists, the `extends_root` re-entry branch fires, every
  side-call (incl. suggestion postamble) excluded.
- **Acceptance:** the golden proves turn 2..N attribution and the by-turn
  rollup across turns — the part of §6 that fires 0× on today's corpus.

## V5 — "The OODA tree is rendered, not guessed." (viewer)

- Delete the client-side re-derivation (`ooda.ts` heuristic, `systemHash`,
  `session_hash`, `flow_id`); render `frame_id`/`parent_frame_id`/`depth`/`role`
  from the §5 marks.
- **Acceptance:** a TS test (`store/events.test.ts` pattern) renders the tree
  from golden §5 marks — parallel capture shows three siblings under one ROOT;
  side-calls in a separate lane; no re-derivation path remains.

## V6 — "What did this turn cost — summing its sub-agents?" (FR5)

- `noodle-embellish` rollup: `GROUP BY turn_id` (nested sub-agents summed) and
  `GROUP BY frame_id`; `role=side_call` bucketed apart; OTLP canned query.
- **Acceptance:** on parallel-subagents the one turn's total = Σ of the three
  Explore agents; side-call tokens excluded.

## V7 — "What did this round-trip teach, across the recursion?" (ADR 051 re-bind)

- Re-bind 051's LEARNED turn-delta to the depth-0 `turn_id`; render the lineage
  line from `parent_frame_id`; `role=side_call` rows off-turn.
- **Acceptance:** a TS test shows the context delta running continuously across
  the parent→sub-agent boundary; a security-monitor row shows no delta and no
  turn membership.

---

## Dependencies

V1 is foundational and standalone. V2 (interface gate) makes V1 green and is the
precondition for V3/V5/V6/V7. V3 hardens V2 on the multi-session capture. V4 is
capture-gated and provable independently once the recording exists — it proves
what V2/V3 assert for the multi-turn case. V5/V6/V7 are consumer slices,
independent of one another once V2 lands.

## Out of scope (ADR §8)

Cross-session *parents* (none captured), identical concurrent prompts
(fingerprint-ambiguous), multi-replica shared state (ADR 050, not on `main`).
