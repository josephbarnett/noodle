# Story 021 — Tool-bucket badges in OODA mode

**Value delivered:** Every `TOOL` row in OODA mode now carries a
small color-coded pill identifying which **bucket** the tool came
from:

- **built-in** — Claude Code's native tools (`Bash`, `Read`,
  `Edit`, `Grep`, `Agent`, `Task`, etc.)
- **MCP** — tools served by MCP servers (named
  `mcp__<server>__<tool>`). The pill carries the server name; the
  full tool name still shows on the row itself.
- **skill** — invocations of the `Skill` meta-tool.

The user can scan a long tool-heavy turn and immediately see at a
glance which calls hit external MCP servers vs which were native to
the agent.

## Acceptance criteria

A user can:

1. Open OODA mode on a session that includes tool calls.
2. See each `TOOL` row carry a pill next to the tool name. The pill
   text matches the classifier:
   - `built-in` for native Claude Code tools.
   - `<server>` for MCP tools (e.g. `claude_ai_Gmail`).
   - `skill` for the `Skill` meta-tool.
3. Each bucket renders in a distinct color so a turn can be
   visually scanned in one pass.
4. Hovering the pill on an MCP tool shows the parsed tool name
   (the part after `mcp__<server>__`) as a tooltip.

## Out of scope (deferred)

- A user-tunable bucket → color mapping (preferences UI). Today
  the three colors are hardcoded to repurposed theme tokens.
- Per-server filtering or grouping in the rail. That's a
  follow-up once we know what operators want to slice on.
- Detection of user-authored agents / non-MCP plugins. Today
  anything that isn't `mcp__*` or `Skill` falls into `builtin`.

## Implementation notes

### `classifyTool` (`src/lib/toolBucket.ts`)

One pure function. Inputs: tool name. Outputs:

```ts
interface ToolClassification {
  bucket: "builtin" | "mcp" | "skill";
  display: string;
  mcp?: { server: string; tool: string } | null;
}
```

Rules:
- Starts with `mcp__` → MCP. Splits on the FIRST `__` after the
  prefix so tools whose name contains underscores still parse.
  Malformed (`mcp__` with no second `__`) still classifies as MCP
  but with `mcp: null` and a fallback display.
- Equals `Skill` → skill. Case-sensitive — `skill` lowercase is
  NOT the meta-tool, that's classified as builtin.
- Otherwise → builtin.

Single source of truth for the classification so future buckets
(user-authored agents, web-fetch sub-classes) add in one file.

### `Block.badge` slot

`Block` gains an optional `badge?: ReactNode` that renders between
the role label and the summary. Backwards-compatible with every
existing call-site that doesn't set it. `ToolBlock` is the first
consumer.

### CSS pills

Three classes — `.tool-bucket-builtin`, `.tool-bucket-mcp`,
`.tool-bucket-skill` — each re-using existing theme tokens
(provider-anthropic warm orange for built-in, accent green for
MCP, thinking-block amber for skill) so the palette stays
consistent across themes.

## Test plan

- `web/tests/lib/toolBucket.test.ts` — 8 tests pinning the
  classifier rules: built-ins, Skill (case-sensitive), MCP
  parsing including tools-with-underscores, malformed mcp
  fallback, `mcp__` alone edge case, and that
  `OpenMcpBridge` (`mcp` in the middle) classifies as builtin.
- `npm run build` + `cargo clippy --workspace --all-targets`
  clean.

## Dependencies

- None — purely viewer-side, only touches `noodle-viewer/web`.
