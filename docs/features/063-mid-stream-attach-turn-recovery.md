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
the last persisted marks for that session** instead of starting empty.

**Source = the SQLite marks store, behind an ADR 050 session-state port — not
`tap.jsonl`.** Recovery is a point lookup ("the last turn state for *this*
session"), and the DB is built for exactly that:

```sql
SELECT turn_id, role, frame_id, parent_frame_id, depth, stop_reason
FROM ai_telemetry_v_0_0_2
WHERE session_id = ? ORDER BY timestamp DESC LIMIT 1
```

- `ai_telemetry_v_0_0_2` already carries the seed columns (`session_id`,
  `turn_id`, `role`, `frame_id`, `parent_frame_id`, `depth` —
  `sqlite.rs:386-393`) and indexes the lookup (`idx_session_id`, `idx_timestamp`
  — `sqlite.rs:472-473`). The seed state *is* the row — no `WireMarks` re-parse.
- `tap.jsonl` is the wrong tool here: an append-only ingest log, so a
  per-session lookup means scanning from the end and JSON-parsing each line.
  Good for sequential ingest, bad for point recovery.
- The embellish poll lag (~250 ms) is irrelevant for restart recovery: the
  state being recovered was flushed *before* the crash. (Lag would only matter
  for live same-process RT-to-RT continuity, which the in-memory state already
  handles.)

**Wrap it in the [ADR 050](../adrs/050-session-state-service.md) `MarkingStateStore`
port — don't hardcode a `SELECT` against `rollups.db`.** The proxy depends on the
port; the adapter does the lookup. The SQLite read is the cheapest backend now
(columns + indexes already exist); Valkey is the multi-replica backend ADR 050
already chose. The read is **read-only and best-effort** (row absent → `turn_id`
None → self-heal), so it is a soft dependency, not a hard coupling to the
telemetry schema.

- Seed `current_turn_id` from the session's last marked round-trip.
- If the session has **no** persisted marks → `turn_id` stays `None` (a true
  pre-capture orphan); it self-heals when the next **main** RT opens a turn.

> Note: `ai_telemetry_v_0_0_2` is the *telemetry* store owned by embellish, so
> reusing it conflates telemetry with session-state. It is the pragmatic backend
> for single-process restart today; ADR 050's cleaner end-state is a dedicated
> session-state store the proxy owns (the same port, a different adapter).

## The one schema gap

Turn-id recovery works with columns already persisted. Deciding the **next main
RT**'s "continue vs. open new turn" needs the last turn's **open/closed** state,
which is **not persisted** — the `ai_telemetry_v_0_0_2` schema carries no
`stop_reason` or turn-open flag. Add one column via the idempotent ADD COLUMN
pattern (ADR 047):

- Add `stop_reason` (or an explicit `turn_open INTEGER`) to
  `ai_telemetry_v_0_0_2`. Embellish already decodes the response `stop_reason`
  (it reads the SSE `message_delta`), so it can populate the column with no
  `WireMarks`/`tap.jsonl` change — the column is purely a DB-side addition.
- Seed `in_turn` from it. (The sub-agent-orphan case does **not** need this — a
  sub-agent attaches to the open turn regardless, so the column matters only for
  the continuation-main-RT case.)

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
- The `stop_reason`/`turn_open` column is added to `ai_telemetry_v_0_0_2` and
  populated by embellish (idempotent migration; existing rows unaffected), then
  read back in the seed query.
- The recovery read goes through the ADR 050 `MarkingStateStore` port; the
  SQLite adapter is one impl. Unit tests exercise the detector against an
  in-memory store impl, so no DB is needed in the test.

## Dependencies / notes

- New seam: a `MarkingStateStore` port (ADR 050) that the marking detector
  reads on cold-start. SQLite adapter (query `ai_telemetry_v_0_0_2`) is the
  single-process backend; Valkey is the multi-replica backend (ADR 050).
- The proxy gains a **read-only, best-effort** dependency on the marks store —
  row absent → `turn_id` None → self-heal. Soft dependency, not a hard coupling.
- The embellish write-lag (~250 ms) does not affect restart recovery: the state
  recovered was flushed before the crash. Lag matters only for live same-process
  RT-to-RT continuity, which the in-memory state already covers.
