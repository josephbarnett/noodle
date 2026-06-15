# Story 015 — Sub-agent parent ↔ child session linking

**Value delivered:** When Claude Code spawns a sub-agent via the `Task`
tool, the parent's `Task` tool_use and the child sub-agent's
conversation are visibly linked in the viewer — both in the session
rail (child indented under parent) and inline in the parent thread
(the `Task` block exposes a "View sub-agent →" affordance).

Per `docs/adrs/008-session-hierarchy.md` (authoritative protocol
knowledge), sub-agent turns are independent conversations from the
API's perspective: they have a distinct `X-Claude-Code-Session-Id`,
a fresh `messages` array, and their own system prompt (= the parent's
`Task.input.prompt`). The proxy sees them as unrelated sessions; this
story reconstructs the link from observable signals.

## Acceptance criteria

A user can:

1. Run a Claude Code session that triggers `Task` (e.g., asking it to
   delegate work to a sub-agent).
2. See in the **session rail**:
   - Parent session at top level.
   - Child sub-agent sessions **indented** under the parent, with a
     "↳ sub-agent" label and a short id.
3. See in the **parent thread**, on the `TOOL Task` block:
   - A "View sub-agent →" link that jumps to the child session.
   - The sub-agent's stop_reason (end_turn / max_tokens) inline so
     you can tell whether it completed successfully.
4. See in the **child session header**:
   - A "← parent: <short id>" link back to the parent session.
   - The parent's spawn-turn number (e.g. "spawned by Turn 3").
5. Live: when a new child session appears mid-conversation, the link
   resolves on the next derived rebuild (no manual refresh).

## Out of scope (deferred)

- Inlining the child conversation directly inside the parent's thread.
  Tempting but visually overwhelming — defer until we have a real
  workflow that needs it.
- Parallel sub-agents from a single round-trip (per the doc's
  hierarchy diagram): the matcher handles each `Task` tool_use
  independently, so two parallel `Task` calls in one round-trip get
  two distinct child links.
- Sub-agents nested inside sub-agents (grandchildren). Possible, just
  not validated until we have fixtures.

## Detection logic

The matcher runs on the full set of `OodaSession`s built by
`buildSessions(pairs)`. Output: a `Map<childSessionId, ParentRef>`
plus a `Map<parentSessionId, ChildRef[]>` for the rail.

**Signals used**, in order:

1. **`Task` tool_use** in a parent's assistant content. Specifically:
   - Name === `"Task"` (Claude Code's spawn tool).
   - `input.prompt` (string) — the system prompt the sub-agent will see.
   - `input.subagent_type` (string, optional) — display hint.
   - `input.description` (string, optional) — short summary.
2. **Child's first `messages[0]`** (or any user-role message) — the
   sub-agent's user prompt is sent in `messages[]`, not `system`.
   Wait — per the doc, the child's `system` prompt = parent's
   `Task.input.prompt`. So the right signal is **system-prompt match**,
   not messages match.
3. **Timestamp proximity**: the child's first round-trip must start
   AFTER the parent's `Task` tool_use, within a window (default 60s).

### Matching algorithm (pure)

```ts
function linkSubAgents(sessions: OodaSession[]): {
  parentOf: Map<string, ParentRef>;     // child id → parent ref
  childrenOf: Map<string, ChildRef[]>;  // parent id → child refs
}
```

For each session `parent`:
  For each turn `t` in parent.turns:
    For each rt in t.roundtrips:
      For each `tool_use` block with name === "Task":
        promptText = block.input.prompt (string)
        For each other session `child` (not yet linked):
          if firstSystemPromptOf(child) === promptText
             AND child.firstActivity > rt.timestamp
             AND child.firstActivity - rt.timestamp < 60s:
            link(parent, child, t.turnNum, block.id, block.input)

The "system prompt" of a session = the `system` field on its first
round-trip's request body. Anthropic accepts `system` as either a
string or an array of blocks; for matching we normalize both to a
canonical string (concat any array entries' `text` fields).

### Edge cases

- **Multiple parents that emit identical Task prompts**: rare but
  possible (e.g., a fixed sub-agent template called multiple times).
  We disambiguate by **timestamp** — the child links to the *closest
  preceding* `Task` tool_use within the window.
- **Parent session capture is partial** (the proxy started mid-flow):
  the child has no detectable parent — it just appears as a top-level
  session with no link. Acceptable.
- **Child capture is partial**: parent's `Task` block still renders
  but the "View sub-agent →" affordance is grayed out with a
  "no capture found" hint.

## Wire shape additions

No engine changes needed. The system field for child sessions already
lands in our captured request body. We just need a viewer-side
matcher.

## UI changes

### Session rail (`web/src/components/SessionRail.tsx`)

Today the rail is a flat list. Story 015 turns it into a two-level
hierarchy:

```
SESSIONS (newest ↓)
┌────────────────────┐
│ 4b2e15… · 12 calls │  ← parent
│ ↳ 9a01ce… · 3 calls│  ← child (indented)
│ ↳ d22e34… · 2 calls│  ← child
│ 8f8c4d… · 5 calls  │  ← unrelated parent
└────────────────────┘
```

The data layer (`linkSubAgents`) provides `childrenOf`; the rail
component renders children inline beneath their parent with an
indented chevron.

### Parent thread (`web/src/components/OodaThread.tsx`)

The `TOOL Task` block gains a link section in its expanded body:

```
TOOL Task · toolu_018… · description="Audit OODA model"
  input  { subagent_type: "general-purpose", description: "...", prompt: "..." }
  ─── sub-agent ───
  ↳ 9a01ce…  ·  4 round-trips  ·  end_turn  ·  view →
```

Click "view →" calls `onSelect(childId)` on the OodaMode container,
which the rail also drives. Switches the active session.

### Child session header

Above the thread:

```
9a01ce… · 4 round-trips · claude-haiku-4-5
← spawned by parent 4b2e15…, Turn 3
```

`← spawned by` is a clickable link.

## Test plan

**Vitest** (`web/tests/derived/sub_agents.test.ts`):

- Two unrelated sessions → no links.
- Parent emits one `Task` tool_use; child's system prompt matches →
  one link, parent→child both directions.
- Parent emits two parallel `Task` tool_uses; two matching children →
  two links.
- Parent emits `Task` with prompt P; an unrelated session also has
  system prompt P but starts 10 minutes after the parent's Task →
  not linked (window too far).
- Child's session has no `system` field → not linked (gracefully).

**Live**:

- Drive Claude Code with a prompt that triggers a sub-agent
  (`"use the Task tool to investigate X"`).
- Confirm the rail shows parent and child linked.
- Click "View sub-agent →" in the parent thread → switches to child.
- Click "← spawned by" in the child header → switches back.

## Dependencies

- `docs/adrs/008-session-hierarchy.md` — the protocol model this
  story implements.
- Story 014 (OODA flat thread) — turn / round-trip data shapes.
- No engine work.

## Why this is Story 015

OODA mode now shows individual sessions correctly. The next gap is
that **a single user action can span multiple sessions** (parent +
sub-agents). Without linking, debugging "what did the agent actually
do?" requires the user to click between unrelated-looking sessions
and reconstruct the parent-child relationship by hand. This story
closes that gap.
