// Pins the tool-cluster derivation contract: contiguous runs of
// `tool-use` ThreadItems of length N≥2 get wrapped in a
// `tool-cluster`; singletons and runs broken by non-tool items
// (thinking, agent-text, headers, user, …) stay individual.
//
// This test exercises the pure post-pass `clusterConsecutiveTools`
// + `clusterSummary` so a render regression doesn't have to wait
// on jsdom to surface.

import { describe, expect, it } from "vitest";
import {
  clusterConsecutiveTools,
  clusterSummary,
  type ThreadItem,
  type ToolUseItem,
} from "../../src/store/derived/thread";

function tool(id: string, name: string): ToolUseItem {
  return {
    kind: "tool-use",
    ts: "2026-05-11T12:00:00Z",
    toolUseId: id,
    name,
    input: {},
    result: null,
    isError: false,
  };
}

function think(text = "let me think"): ThreadItem {
  return { kind: "thinking", ts: "2026-05-11T12:00:00Z", text };
}

function agent(text = "hi"): ThreadItem {
  return { kind: "agent-text", ts: "2026-05-11T12:00:00Z", text };
}

describe("clusterConsecutiveTools", () => {
  it("passes a single tool-use through unchanged", () => {
    const input: ThreadItem[] = [tool("t1", "Read")];
    const out = clusterConsecutiveTools(input);
    expect(out).toHaveLength(1);
    expect(out[0].kind).toBe("tool-use");
  });

  it("clusters 2+ consecutive tool-use into one tool-cluster", () => {
    const input: ThreadItem[] = [
      tool("t1", "Read"),
      tool("t2", "Read"),
      tool("t3", "Bash"),
    ];
    const out = clusterConsecutiveTools(input);
    expect(out).toHaveLength(1);
    expect(out[0].kind).toBe("tool-cluster");
    if (out[0].kind === "tool-cluster") {
      expect(out[0].items).toHaveLength(3);
      expect(out[0].items.map((i) => i.name)).toEqual(["Read", "Read", "Bash"]);
    }
  });

  it("does NOT cluster across a thinking interruption", () => {
    // The agent stopped to think between calls — that's meaningful;
    // splitting preserves the visual rhythm.
    const input: ThreadItem[] = [
      tool("t1", "Read"),
      think(),
      tool("t2", "Read"),
    ];
    const out = clusterConsecutiveTools(input);
    expect(out.map((i) => i.kind)).toEqual(["tool-use", "thinking", "tool-use"]);
  });

  it("does NOT cluster across an agent-text interruption", () => {
    const input: ThreadItem[] = [
      tool("t1", "Read"),
      agent("now let me run something"),
      tool("t2", "Bash"),
    ];
    const out = clusterConsecutiveTools(input);
    expect(out.map((i) => i.kind)).toEqual([
      "tool-use",
      "agent-text",
      "tool-use",
    ]);
  });

  it("groups multiple disjoint clusters independently", () => {
    const input: ThreadItem[] = [
      tool("t1", "Read"),
      tool("t2", "Read"),
      agent("done with that batch"),
      tool("t3", "Bash"),
      tool("t4", "Bash"),
      tool("t5", "Bash"),
    ];
    const out = clusterConsecutiveTools(input);
    expect(out.map((i) => i.kind)).toEqual([
      "tool-cluster",
      "agent-text",
      "tool-cluster",
    ]);
    if (out[0].kind === "tool-cluster" && out[2].kind === "tool-cluster") {
      expect(out[0].items).toHaveLength(2);
      expect(out[2].items).toHaveLength(3);
    }
  });

  it("preserves non-tool items unchanged at the boundaries", () => {
    const input: ThreadItem[] = [
      { kind: "turn-divider", turnNum: 1, ts: "t", roundtrips: 1 },
      tool("t1", "Read"),
      tool("t2", "Read"),
      { kind: "turn-end", ts: "t", turnNum: 1 },
    ];
    const out = clusterConsecutiveTools(input);
    expect(out.map((i) => i.kind)).toEqual([
      "turn-divider",
      "tool-cluster",
      "turn-end",
    ]);
  });
});

describe("clusterSummary", () => {
  it("renders insertion order, no dedup needed", () => {
    expect(
      clusterSummary([tool("a", "Bash"), tool("b", "Read")]),
    ).toBe("Bash, Read");
  });

  it("collapses repeats with ×N", () => {
    expect(
      clusterSummary([
        tool("a", "Read"),
        tool("b", "Read"),
        tool("c", "Bash"),
      ]),
    ).toBe("Read ×2, Bash");
  });

  it("preserves first-seen ordering when names interleave", () => {
    // First Read, then Bash, then Read again → Read still listed
    // first (it was seen first).
    expect(
      clusterSummary([
        tool("a", "Read"),
        tool("b", "Bash"),
        tool("c", "Read"),
      ]),
    ).toBe("Read ×2, Bash");
  });

  it("caps at 3 distinct names then `+M more`", () => {
    expect(
      clusterSummary([
        tool("a", "Read"),
        tool("b", "Bash"),
        tool("c", "Edit"),
        tool("d", "Grep"),
        tool("e", "Glob"),
      ]),
    ).toBe("Read, Bash, Edit, +2 more");
  });

  it("counts repeats correctly even when the name is past the cap", () => {
    // 4 distinct: Read, Bash, Edit, Grep. Bash repeats but is in
    // SHOW window so we see Bash ×2; Grep is past the cap.
    expect(
      clusterSummary([
        tool("a", "Read"),
        tool("b", "Bash"),
        tool("c", "Bash"),
        tool("d", "Edit"),
        tool("e", "Grep"),
      ]),
    ).toBe("Read, Bash ×2, Edit, +1 more");
  });
});
