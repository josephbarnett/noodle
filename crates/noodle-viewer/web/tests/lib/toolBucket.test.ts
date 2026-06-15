// Pins the tool-bucket classifier contract — every tool name that
// flows through the viewer hits this code path, so a regression
// here visibly mis-labels every TOOL row in OODA mode.

import { describe, expect, it } from "vitest";
import { classifyTool } from "../../src/lib/toolBucket";

describe("classifyTool", () => {
  it("classifies Claude Code built-ins as builtin", () => {
    for (const name of ["Bash", "Read", "Edit", "Write", "Grep", "Glob", "TodoWrite", "Task", "WebFetch", "Agent"]) {
      const c = classifyTool(name);
      expect(c.bucket).toBe("builtin");
      expect(c.display).toBe("built-in");
      expect(c.mcp).toBeUndefined();
    }
  });

  it("classifies the Skill meta-tool as skill", () => {
    const c = classifyTool("Skill");
    expect(c.bucket).toBe("skill");
    expect(c.display).toBe("skill");
  });

  it("classifies mcp__server__tool as mcp and parses server/tool", () => {
    const c = classifyTool("mcp__claude_ai_Gmail__create_draft");
    expect(c.bucket).toBe("mcp");
    expect(c.display).toBe("claude_ai_Gmail");
    expect(c.mcp).toEqual({ server: "claude_ai_Gmail", tool: "create_draft" });
  });

  it("splits on the FIRST `__` so tool names with underscores survive", () => {
    // The OpenAI MCP search tool: `mcp__openai__some_thing_with_underscores`.
    const c = classifyTool("mcp__openai__some_thing_with_underscores");
    expect(c.mcp?.server).toBe("openai");
    expect(c.mcp?.tool).toBe("some_thing_with_underscores");
  });

  it("flags malformed mcp names (no double-underscore after prefix) as mcp w/ null parse", () => {
    const c = classifyTool("mcp__weirdformat");
    expect(c.bucket).toBe("mcp");
    expect(c.mcp).toBeNull();
    expect(c.display).toBe("weirdformat");
  });

  it("handles the edge case of `mcp__` alone", () => {
    // Implausible but defensive — should NOT throw or produce an
    // empty display.
    const c = classifyTool("mcp__");
    expect(c.bucket).toBe("mcp");
    expect(c.mcp).toBeNull();
    // display falls back to "mcp" when the rest is empty.
    expect(c.display).toBe("mcp");
  });

  it("is case-sensitive on Skill (matches Claude Code's casing)", () => {
    // "skill" lowercase is NOT the meta-tool. Today this falls into
    // the builtin bucket; pin that so a future fuzzy-match doesn't
    // sneak in.
    expect(classifyTool("skill").bucket).toBe("builtin");
    expect(classifyTool("SKILL").bucket).toBe("builtin");
  });

  it("does not strip `mcp__` from non-prefix matches", () => {
    // A built-in tool that happens to have `mcp` in the middle of
    // its name should still be classified as builtin.
    expect(classifyTool("OpenMcpBridge").bucket).toBe("builtin");
  });
});
