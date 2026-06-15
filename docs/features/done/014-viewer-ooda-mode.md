# Story 014 — Viewer OODA mode

**Value delivered:** A third-tab view that reconstructs the agent ↔ LLM
conversation from `/v1/messages` exchanges: sessions → turns → content
blocks (thinking / text / tool_use / tool_result), with each tool call
paired to its result. Replaces reliance on an external TAP viewer's
OODA rendering with a noodle-native version against our own data
shape.

## Acceptance criteria

A user can:

1. Click the **OODA** tab in the top bar (no longer disabled).
2. Sessions appear in a left rail, one per `session_hash`, sorted by
   most recent activity. Each shows: short identifier, turn count,
   model name, latest timestamp.
3. Click a session → main panel shows the threaded conversation:
   - Each turn is one `/v1/messages` exchange.
   - **User input** at the top of each turn — the latest user-role
     entry from `request.messages` (text content, or a `tool_result`
     block if the prior turn invoked tools).
   - **Assistant output** below — content blocks from
     `response.content` in their original order:
     - `thinking` blocks render dim, with a small "thinking" label,
       collapsible after 6 lines.
     - `text` blocks render as flowing prose.
     - `tool_use` blocks render as a card with name, parameter JSON,
       and **the paired result from the next turn's user message
       inline**.
   - Turn footer shows: model, `stop_reason`, token usage from
     `response.usage`.
4. Live: the conversation appends as new turns land.

## Out of scope (deferred)

- Sub-agent (parent/child) chain detection — Story 015.
- Per-frame SSE rendering (streaming text deltas as they arrive) —
  Story 017 (depends on Story 016's per-frame sink).
- Filtering / search inside a conversation — Story 018.

## Implementation notes

### Pure logic: `web/src/store/derived/ooda.ts`

The data layer transforms `ExchangePair[]` into `OodaSession[]`:

1. **Filter** to chat completions: keep pairs where `request.url`
   contains `/v1/messages`. Anthropic-shaped only in this story
   (OpenAI's `/v1/chat/completions` deferred — its request shape uses
   `choices` and pairs tool calls differently).
2. **Group** by `request.session_hash`. Pairs without a session hash
   collapse into a single "anonymous" session keyed by
   `request.headers.host` so unrelated unauthenticated traffic
   doesn't merge.
3. **Build turns**:
   - One turn per pair, in request-timestamp order.
   - User input = the *last* `messages[]` entry with `role: "user"`.
     Its `content` may be a string (plain text) or an array of
     content blocks (tool_results).
   - Assistant output = `response.body.content[]`.
   - Metadata: `model`, `stop_reason`, `usage` from response.
4. **Pair tools**: For each `tool_use` block in turn N's assistant
   output (carrying an `id`), find the matching `tool_result` in
   turn N+1's user-input content array (matched by `tool_use_id`).
   Attach the result to the tool_use block for inline rendering.
5. **Stable IDs**: each turn carries `event_id` so React keys stay
   stable across re-renders.

Pure functions, all tested with vitest fixtures. The fixtures are
captured-from-real-Claude-Code JSON request/response bodies.

### UI components

- `modes/OodaMode.tsx` — top-level: left rail + main scrollable area.
- `components/SessionRail.tsx` — list of sessions, selectable.
- `components/Turn.tsx` — one turn's view.
- `components/ContentBlocks.tsx` — render an array of content blocks.
- `components/ToolUseCard.tsx` — paired tool_use + tool_result.

Selection state (active session, expanded turns) lifts to `OodaMode`.

### Wiring

- `ModeSwitcher.tsx` enables the OODA tab.
- `App.tsx` routes `mode === "ooda"` to `<OodaMode pairs={pairs} />`.

### Test plan

**Vitest** (`web/tests/derived/ooda.test.ts`):
- Empty input → empty sessions.
- Non-chat URLs are filtered out.
- Two exchanges with same session_hash → one session, two turns.
- A turn with tool_use → next turn's tool_result is paired.
- Anonymous traffic (no session_hash) groups by host.

**Live**:
- Use Claude Code through noodle.
- Verify session rail shows the active Claude Code session.
- Click into it → see user prompts, thinking blocks, tool calls (Read,
  Edit, Bash, …), and their results threaded together.

## Dependencies

- Story 012 (foundation) — event store and pair grouping.
- Story 013 (row detail) — extends the `Exchange` shape we already
  carry; no further engine work needed.
- Session-hash detection: noodle-tap's `session.rs` already covers
  `X-Claude-Code-Session-Id` and the system-prompt-hash fallback.

## Why this is Story 014

013 made the per-exchange view useful; 014 is the answer to *what is
the agent actually trying to do?*. It's the highest-value view for
debugging agent behavior — when an agent goes wrong, you usually need
to see its reasoning chain (thinking blocks), tool choices, and tool
results in sequence, not request/response pairs.
