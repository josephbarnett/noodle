// Story 059 — each round-trip surfaces its OWN token usage in the OODA
// thread (per-RT, never summed across the turn: a turn re-presents the
// cached context every round-trip, so a turn-level sum would double-count
// it — ADR 056). These tests pin the `headers` item carrying `rt.usage`
// and the compact chip formatter.

import { describe, expect, it } from "vitest";
import { flattenAgentRun } from "./thread";
import { usageChip } from "../../components/HeadersBlock";
import type { AgentRun, OodaTurn, RoundTrip, Usage } from "./ooda";

function rt(exchangeId: string, usage: Usage): RoundTrip {
  return {
    exchangeId,
    timestamp: "2026-06-21T00:00:00Z",
    userMessage: [{ type: "text", text: "hi" }],
    systemBlocks: [],
    systemMutated: false,
    assistant: [{ type: "text", text: "ok" }],
    stopReason: "end_turn",
    usage,
    continuesLoop: false,
  };
}

function run(turns: OodaTurn[]): AgentRun {
  return {
    index: 1,
    frameId: "ROOT",
    startedAt: "2026-06-21T00:00:00Z",
    systemPreview: "",
    isMain: true,
    depth: 0,
    spawnedBy: null,
    parentRunIndex: null,
    turns,
  };
}

describe("flattenAgentRun — per-RT usage on headers items", () => {
  it("each round-trip's headers item carries its OWN usage (not summed)", () => {
    const turn: OodaTurn = {
      turnNum: 1,
      startedAt: "2026-06-21T00:00:00Z",
      turnId: "turn-1",
      userInput: [{ type: "text", text: "hi" }],
      roundtrips: [
        rt("rt-0", { input_tokens: 100, output_tokens: 10, cache_read_input_tokens: 5000 }),
        rt("rt-1", { input_tokens: 200, output_tokens: 20, cache_read_input_tokens: 5000 }),
      ],
    };
    const items = flattenAgentRun(run([turn]));
    const headers = items.filter((i) => i.kind === "headers");
    expect(headers).toHaveLength(2);

    // Per-RT: each headers item carries exactly its own round-trip's usage —
    // no aggregation, no shared/summed total.
    expect(headers[0].kind === "headers" && headers[0].usage?.input_tokens).toBe(100);
    expect(headers[1].kind === "headers" && headers[1].usage?.input_tokens).toBe(200);
    expect(headers[0].kind === "headers" && headers[0].usage?.output_tokens).toBe(10);
    expect(headers[1].kind === "headers" && headers[1].usage?.output_tokens).toBe(20);
  });
});

describe("usageChip — compact per-RT token formatting", () => {
  it("formats input/output and cache, abbreviating thousands", () => {
    expect(usageChip({ input_tokens: 244329, output_tokens: 340, cache_read_input_tokens: 244000 }))
      .toBe("244k↑ 340↓ cache 244k");
    expect(usageChip({ input_tokens: 1200, output_tokens: 50 })).toBe("1.2k↑ 50↓");
  });

  it("omits zero/absent cache and returns null when empty", () => {
    expect(usageChip({ input_tokens: 100, output_tokens: 10, cache_read_input_tokens: 0 }))
      .toBe("100↑ 10↓");
    expect(usageChip(undefined)).toBeNull();
    expect(usageChip({})).toBeNull();
  });
});
