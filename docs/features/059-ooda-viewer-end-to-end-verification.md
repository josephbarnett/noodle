# 059 — OODA viewer works end-to-end on the new detector marks

Prove (and fix where needed) that the OODA debugger renders a correct
`session → turn → frame → round-trip` tree from the **new** header-driven marks
shipped in `#9`. The marks pipeline is wired (proxy stamps `role`/`frame_id`/
`parent_frame_id`/`depth`/`turn_id` → `WireMarks` → tap.jsonl → viewer
`getMarks`), but it has **never been run live** with the new detector — current
`~/.noodle` data is old-detector output (no `role`, `distinct_turns=1`). This is
the verify-against-real-product gate before we call the UI done.

## Value

A user drives a real Claude Code session (multi-turn, with a Task sub-agent)
through the proxy, opens the viewer in OODA mode, and sees: real user turns as
turns (not the AUX flood), sub-agents nested under the spawning turn, side-calls
(quota/title/monitor/recap) in their own off-tree lane, and per-turn token cost.

## Acceptance criteria

- Production proxy (release build, new detector) run against a live session that
  (a) spans ≥3 user prompts and (b) spawns at least one Task sub-agent.
- In the resulting tap.jsonl, `marks.role` ∈ {main, sub_agent, side_call} with
  genuine prompts → `main`, harness calls → `side_call`; `turn_id` segments the
  real prompts; sub-agent RTs carry their `frame_id` + `parent_frame_id=ROOT`.
- OODA mode renders N turns matching the real prompts (the live capture in this
  session reconstructs to **13 turns**, not 21), side-calls in the auxiliary
  lane, sub-agents as nested runs, per-turn cost shown.
- Fail-before/pass-after captured (screenshot or row counts) against the same
  workflow.

## Dependencies

- `#9` (merged) — the header-driven detector + marks contract.

## Implementation notes

- Run: `make run-release` (or the proxy release bin) with the viewer attached;
  drive `claude` through `HTTPS_PROXY`; spawn a Task so `x-claude-code-agent-id`
  appears.
- Viewer consumption is already marks-driven: `web/src/store/derived/ooda.ts`
  `buildSessions` → `buildSessionFromMarks` reads `role`/`frame_id`/`turn_id`;
  `App.tsx` feeds `getMarks` from the decoded-exchange SSE. Confirm the SSE feed
  carries the new marks (it rides the same `WireEvent`).

## Concrete suspects to check (likely the only real fixes)

1. **Turn-display cap** — the "feels like a limit of 7 turns" observation. Audit
   `OodaThread`/`OodaMode` for a slice/pagination cap; remove or paginate.
2. **Side-call lane** — confirm `ooda.ts` routes `role==="side_call"` to
   `auxiliary` (it did pre-swap); verify recap/monitor now land there, not as
   turns.
3. **Per-turn cost** — confirm the turn header sums usage across the turn's RTs
   (ties to ADR 056 context-weight already merged in `#8`).

## Out of scope

- OTel trace export (separate ADR + stories).
- Deep (depth-2+) OpenCode nesting — proxy detector is depth-1 (server §6 later).
