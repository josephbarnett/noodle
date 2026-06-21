# 063 — Mid-stream attach: turn recovery from persisted marks

**Status:** open.
**Surface:** `crates/noodle-adapters/src/marking/frame_tree.rs`,
`crates/noodle-embellish/src/sqlite.rs` (one ADD COLUMN), proxy seed wiring.
**Defect against:** story 058 / ADR 052 §6 (header-driven turn marking).
**Related:** ADR 050 (session-state service — this is its cheap single-process
instantiation), ADR 047 (idempotent ADD COLUMN pattern).

## Problem

`FrameTreeDetector` segments turns from **streaming in-process state** —
`in_turn` / `turn` / `current_turn_id` (`frame_tree.rs:155-163`). A turn id is
minted only on the first **main** round-trip after the previous turn closed
(`frame_tree.rs:228-232`); every other round-trip inherits `current_turn_id`
(`frame_tree.rs:240`). The registry is an in-memory `DashMap` with **no
persistence** (`frame_tree.rs:279`), and the id is a **random ULID**, not a
function of `session_id` (`frame_tree.rs:145`) — so it cannot be recomputed,
only remembered.

Consequence on **mid-stream attach** (proxy restart / late capture, where the
detector first sees a session whose turn opener it never watched):

- A **sub-agent** round-trip seen before any main turn opens → `role=sub_agent`,
  frame/parent correct, but `turn_id = None` (current_turn_id still None). A real
  frame **orphaned off the turn tree** — not skipped, just turn-less.
- A **continuation main** round-trip seen first → `!in_turn` is true, so it
  **opens a spurious fresh turn**, mis-segmenting a turn already in flight.

Genuine session start is unaffected (first real RT is the user prompt = a main
opener). The gap is strictly recovery of an opener that predates this detector's
memory.

## Decision

On first sighting of a `session_id` in a fresh detector, **seed turn state from
the last persisted marks for that session** instead of starting empty. Source of
truth is the proxy's **own `tap.jsonl`** (`WireMarks` carries
`role`/`frame_id`/`parent_frame_id`/`depth`/`turn_id`, `wire.rs:367-382`) — the
proxy rehydrates from its own write-ahead log, not the downstream `rollups.db`
(keeps the hexagonal boundary; the SQLite columns at `sqlite.rs:386-393` are an
equivalent fallback if tap.jsonl is unavailable).

- Seed `current_turn_id` from the session's last marked round-trip.
- If the session has **no** persisted marks → `turn_id` stays `None` (a true
  pre-capture orphan); it self-heals when the next **main** RT opens a turn.

## The one schema/wire gap

Turn-id recovery works with data already persisted. Deciding the **next main
RT**'s "continue vs. open new turn" needs the last turn's **open/closed** state,
which is **not persisted** — neither `WireMarks` nor the SQLite schema carries
`stop_reason` or a turn-open flag. Add one field via the idempotent ADD COLUMN
pattern (ADR 047):

- Persist `stop_reason` (or an explicit `turn_open: bool`) on the round-trip
  marks → `tap.jsonl` `WireMarks` and the `ai_telemetry_v_0_0_2` table.
- Seed `in_turn` from it. (The sub-agent-orphan case does **not** need this — a
  sub-agent attaches to the open turn regardless.)

## Acceptance criteria

- Replay a capture split at an arbitrary mid-turn point through two successive
  detector instances (simulating restart): the second instance reconstructs the
  **same** `turn_id` for the in-flight turn's sub-agent/continuation RTs as a
  single uninterrupted detector would — fail-before (orphan `turn_id=None` /
  spurious new turn), pass-after.
- A sub-agent RT seen first after attach attaches to the recovered turn, not
  `None`.
- A `session_id` with no prior persisted marks yields `turn_id=None`, then opens
  turn-1 on the first main RT (self-heal).
- `stop_reason`/`turn_open` round-trips through `WireMarks` → tap.jsonl and the
  SQLite column (idempotent migration; existing rows unaffected).

## Dependencies / notes

- No new architecture; horizontal extension of the existing marking pipeline.
- Multi-replica (not single-process restart) is ADR 050's shared-store version
  of the same seed mechanism — out of scope here; note the seam.
- Mind the embellish write-lag (~250ms poll): tap.jsonl is the lower-latency
  seed source and is the proxy's own output, so prefer it.
