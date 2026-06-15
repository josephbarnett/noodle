// Tool-bucket classifier — pure function from a tool's name to one of
// three buckets so the viewer can badge tool_use blocks visually.
//
// Rationale: Claude Code names its tools by convention:
//   - "Bash", "Read", "Edit", "Grep", ...           → built-in tools
//   - "mcp__<server>__<tool>"                        → MCP server tools
//   - "Skill"                                         → skill-invocation meta-tool
//
// We keep the classification in one file so the UI never has to grep
// for prefixes in multiple places, and so future buckets (e.g. user-
// authored agents) can be added without touching call-sites.

export type ToolBucket = "builtin" | "mcp" | "skill";

export interface ToolClassification {
  bucket: ToolBucket;
  /** Display name to show on the badge. For MCP tools this is the
   *  parsed server name; for built-ins / skills it's the bucket
   *  label itself. */
  display: string;
  /** For `mcp` only: the parsed `<server>` and `<tool>` from the
   *  `mcp__<server>__<tool>` naming convention. `null` if the name
   *  starts with `mcp__` but doesn't fit the pattern. */
  mcp?: { server: string; tool: string } | null;
}

const MCP_PREFIX = "mcp__";

/** Classify `name` into a bucket. Pure function; no side effects.
 *  Returns the bucket plus a display string for the badge. */
export function classifyTool(name: string): ToolClassification {
  if (name.startsWith(MCP_PREFIX)) {
    const rest = name.slice(MCP_PREFIX.length);
    // `mcp__<server>__<tool>` — the FIRST `__` separates server
    // from tool. Tool names can themselves contain `_`, so we split
    // on the first `__` only.
    const sep = rest.indexOf("__");
    if (sep === -1) {
      // `mcp__weirdthingwithoutdoubleunderscore` — preserve the
      // prefix awareness but flag the parse miss.
      return { bucket: "mcp", display: rest || "mcp", mcp: null };
    }
    const server = rest.slice(0, sep);
    const tool = rest.slice(sep + 2);
    return { bucket: "mcp", display: server, mcp: { server, tool } };
  }
  if (name === "Skill") {
    return { bucket: "skill", display: "skill" };
  }
  return { bucket: "builtin", display: "built-in" };
}
