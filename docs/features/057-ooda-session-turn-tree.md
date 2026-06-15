# Feature 057 — OODA session → turn tree (left panel)

**Status:** open.
**Surface:** `crates/noodle-viewer` (OODA mode, left `SESSIONS` panel).
**Enabled by:** ADR 052 (turn / stop-reason `turn_id` tracking) — every
round trip now carries a stable turn id, so turns are groupable without
client-side heuristics.

## Value delivered

In OODA mode the left panel today lists sessions and, under each, the
agent run (e.g. `main agent · 5 turns · 11 rt`) as a flat entry. A user
scanning a long capture cannot jump to a specific turn from the
navigator — they scroll the main pane.

After this, the left panel is a **tree**: each session expands to its
turns, each turn is clickable and scrolls/selects that turn in the main
pane. The navigator mirrors the turn structure the main pane already
renders, so moving around a multi-session, multi-turn capture is direct.

```
session e163c55b-84c · claude-opus-4-8
  ├─ Turn 1 · 2 rt
  ├─ Turn 2 · 6 rt
  ├─ Turn 3 · 1 rt
  ├─ Turn 4 · 1 rt
  └─ Turn 5 · 1 rt
session yyyy · …
  └─ Turn 1 · …
```

## Acceptance criteria

- Each session node in the left panel expands/collapses to show its
  turns, in turn order, labelled `Turn N` with a round-trip count.
- Clicking a turn selects/scrolls to that turn in the main pane; the
  active turn is highlighted in the tree.
- Sessions with one agent run render turns directly; multi-run sessions
  keep the run level (`session → run → turn`) — the tree degrades
  gracefully, it does not assume a single run.
- Live capture: new turns appear in the tree as they arrive (same SSE
  feed that updates the main pane).
- Empty/edge: a session with no completed turn boundary (in-flight
  first turn) shows the session node with a single in-progress turn, not
  a broken/empty branch.

## Dependencies

- ADR 052 turn tracking shipped to the read-model the viewer consumes
  (the `turn_id` / `TURN:xxxx` marks already render as chips in the main
  pane — confirm they are present per session in the viewer read-type).
- No backend change expected: the grouping the main pane performs
  (round trips → turns → runs → session) is lifted into the navigator.

## Implementation notes

- The main pane already groups round trips into turns (the
  `TURN N · k round-trips` headers). Factor that grouping into a shared
  selector so both the main pane and the left panel consume one
  turn-tree derivation — avoid two groupings that can drift.
- Left panel component: `crates/noodle-viewer/web/src/components`
  (the `SESSIONS` list). Add a collapsible turn list per session/run.
- Selection state: a `selectedTurnId` lifted to the OODA view; clicking
  a tree node sets it; the main pane scrolls the matching turn into view
  and highlights it.
- Pairs naturally with the ADR 056 viewer step (the context-weight
  badge / session trend also live in this panel area) — do them in the
  same viewer pass to avoid two left-panel refactors.

## Out of scope

- Per-turn metrics in the tree (edit counts from ADR 055, context cost
  from ADR 056) — additive once those land; this story is navigation.
