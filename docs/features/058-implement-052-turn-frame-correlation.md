# 058 — Implement ADR 052: stateless capture + server-side turn/frame correlation

Port the ADR 052 two-stage model (stateless edge record §5 → server correlation
§6) into the noodle Rust workspace, replacing the stateful `frame_tree` marking
detector. Sequenced **additive-first**: the new path is built and proven against
real captures *beside* the existing detector; the old machinery is removed only
after parity. The reference proof (`scripts/tap_correlate.py`) is the parity
oracle for every slice — it reconstructs the `session → turn → frame` tree from a
real `tap.jsonl` and is the fail-before/pass-after check.

## Proven before starting (read-only, against `~/.noodle/tap.jsonl`)

- Session grouping signal present: `x-claude-code-session-id` on 148/148 RTs.
- Chain link present: `diagnostics.previous_message_id` on continuation RTs
  (52/148 — only non-openers, as §5 predicts).
- Turn segmentation works: `stop_reason` (in the SSE `message_delta`) segments
  the live session into **13 real turns** — each opener is an actual user prompt.
- Side-call detection is **required and additive to the ADR**: without it,
  monitor/recap/suggestion calls manufacture phantom turns (21 vs 13). The proof
  carries a content-free `side_call` flag (trailing-wrapper kind + quota probe).
- Real per-turn token cost rolls up; 106 side-calls (~14k tok) sit off-tree.

## Proven — sub-agent frame signal (hand-decoded captures)

`analysis/tools/claude/parent_subagent/` and `analysis/tools/opencode/multi_prompt/`
confirm the §5 frame-identity signals on real sub-agent traffic:

- **Claude Code:** main RTs carry `x-claude-code-session-id` only; each sub-agent
  RT adds `x-claude-code-agent-id` (three parallel sub-agents → three distinct
  ids: `abe1f4c6…`, `ab096c46…`, `a78ea0e4…`), same session. Absent agent-id ⟹
  `MAIN`. This **replaces** the fragile `message_sig`/`extends_root`/spawn-
  fingerprint logic — frame identity is read off the header.
- **OpenCode:** frame id is the session id (`x-session-id`); sub-agent carries
  `x-parent-session-id` pointing at the root — arbitrary nesting reconstructs.

Frame identity is therefore header-driven and stateless. The remaining
fingerprint (`open_fp`/`spawn_fps`) only *refines which* parent RT spawned a
child; CC wraps the prompt so it falls back to the parent's last spawning RT
(per §5).

---

## Slice 1 — Stateless §5 record extractor (additive)

**Value:** the proxy emits a content-free per-RT record from a single
request/response, with no cross-request state — the foundation everything else
reads. Lands beside `frame_tree`; nothing removed.

**Acceptance:**
- New extractor computes, per `/v1/messages` RT: `session_id`, `frame_id`,
  `parent_frame_id`, `prev_message_id`, `this_message_id`, `stop_reason`,
  `open_fp` (12-hex of leading user text; null on continuation), `spawn_fps`
  (name-free: any `tool_use` whose input carries a string `prompt`), `n_spawn`,
  `side_call`, `tokens` (in/out/cache_read/cache_creation).
- Unit tests over the existing `adr_052` fixtures assert these fields.
- Holds no per-session state; every field derives from the one RT.

**Notes:** mirror `scripts/tap_correlate.py` §5 exactly so the Rust output and
the proof oracle agree field-for-field.

## Slice 2 — Persist the record (sink + tap.jsonl + SQLite)

**Value:** records survive to where correlation runs.

**Acceptance:**
- tap.jsonl record carries the §5 fields (extend `marks` or a sibling object).
- SQLite gains the columns via the ADR 047 idempotent ADD COLUMN pattern.
- Round-trips through the real proxy and shows the fields populated.

## Slice 3 — Server §6 correlation (port `tree_side`)

**Value:** the `session → turn → frame` tree + `turn_id`/`frame`/`depth` tags,
derived server-side as a pure function of records.

**Acceptance:**
- Rust correlation reproduces `tap_correlate.py`'s tree on the same capture
  (parity test: identical turn count + openers).
- Verified against a **sub-agent capture** (run a Task through the proxy):
  `x-claude-code-agent-id` present, sub-agent frame nests under the spawning
  turn, frame counts match.
- Side-calls land off-tree; turn cost = sum over the turn's RTs.

## Slice 4 — Viewer OODA consumes server tags

**Value:** the OODA tree renders real turns + per-turn cost + an off-tree
side-call lane, from deterministic server tags instead of re-derived `marks`.

**Acceptance:**
- `buildSessions` reads the server `turn_id`/`frame`/`depth`; old body
  re-derivation removed from the required path.
- The live session renders 13 turns (not 21), side-calls in their own lane.

## Slice 5 — Cutover and delete the stateful detector

**Value:** the sensitive `frame_tree`/`extends_root`/`message_sig` machinery is
gone; less per-RT work, no growing state.

**Acceptance:**
- Parity proven on all `adr_052` captures (turn tree matches the oracle).
- `frame_tree.rs` stateful paths + goldens retired or repointed.
- Benchmark the edge before/after; report the real delta (no fabricated number).

---

## Security

Records stay content-free: ids, sha256-prefix fingerprints, counts, enums — no
prompt/response text, headers, or secrets. Prompt text is read transiently to
fingerprint, never persisted. Consistent with the existing data-security ADR.
