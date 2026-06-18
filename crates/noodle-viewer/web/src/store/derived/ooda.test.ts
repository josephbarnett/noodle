// ADR 052 §6 — the OODA viewer reconstructs the frame tree from the proxy's
// §5 marks (it renders marks, never re-derives — FR6). These tests feed the
// marks of the real `claude-parallel-subagents` capture (1 turn, main + three
// sub-agents) through `buildSessions` and assert the rendered tree, plus turn
// segmentation and side-call routing. The tree is built from marks, not body
// content, so the HTTP pairs are minimal (only the `/v1/messages` url matters).

import { describe, expect, it } from "vitest";
import { buildSessions } from "./ooda";
import type { DecodedMarks, ExchangePair } from "../../types";

function pair(eventId: string): ExchangePair {
  return {
    event_id: eventId,
    request: { url: "https://api.anthropic.com/v1/messages" },
    response: { status: 200 },
  } as unknown as ExchangePair;
}

function build(marks: Record<string, DecodedMarks>) {
  const ids = Object.keys(marks);
  const pairs = ids.map((id) => pair(id));
  const byId = new Map(pairs.map((p) => [p.event_id, p]));
  return buildSessions(pairs, byId, undefined, (id) => marks[id] ?? null);
}

describe("buildSessions — §6 frame-tree reconstruction from marks", () => {
  it("reconstructs parallel-subagents: 1 session, 1 turn, main + 3 sub-agent frames", () => {
    const S = "sess-A";
    const T = "turn-1";
    const main: DecodedMarks = {
      session_id: S, role: "main", frame_id: "ROOT", parent_frame_id: null, depth: 0, turn_id: T,
    };
    const sub = (frame: string): DecodedMarks => ({
      session_id: S, role: "sub_agent", frame_id: frame, parent_frame_id: "ROOT", depth: 1, turn_id: T,
    });

    const sessions = build({
      e1: main, // main RT0 (spawns 3)
      e2: sub("ab096c46"), e3: sub("ab096c46"), e4: sub("ab096c46"), e5: sub("ab096c46"),
      e6: sub("a78ea0e4"), e7: sub("a78ea0e4"), e8: sub("a78ea0e4"),
      e9: sub("abe1f4c6"), e10: sub("abe1f4c6"), e11: sub("abe1f4c6"),
      e12: main, // main RT1 (close)
      e13: { session_id: S, role: "side_call", frame_id: null, turn_id: null }, // off-tree
    });

    expect(sessions).toHaveLength(1);
    const s = sessions[0];

    // 4 frames: ROOT (main) + 3 sub-agents.
    expect(s.agentRuns).toHaveLength(4);

    const root = s.agentRuns.find((r) => r.frameId === "ROOT")!;
    expect(root.isMain).toBe(true);
    expect(root.depth).toBe(0);
    expect(root.parentRunIndex).toBeNull();
    expect(root.turns).toHaveLength(1); // everything is one turn
    expect(root.turns[0].roundtrips).toHaveLength(2); // e1 + e12

    const subs = s.agentRuns.filter((r) => !r.isMain);
    expect(subs).toHaveLength(3);
    for (const sa of subs) {
      expect(sa.depth).toBe(1);
      expect(sa.parentRunIndex).toBe(root.index); // nested under the main frame
      expect(sa.turns).toHaveLength(1); // inherit the turn
    }
    // the 4-RT sub-agent keeps all four of its round-trips
    expect(
      s.agentRuns.find((r) => r.frameId === "ab096c46")!.turns[0].roundtrips,
    ).toHaveLength(4);

    // the harness call is off-tree, in the auxiliary lane — not a frame or turn
    expect(s.auxiliary).toHaveLength(1);
  });

  it("segments the main frame into turns by turn_id (no AUX flood, no cap)", () => {
    const S = "sess-B";
    const m = (turn: string): DecodedMarks => ({
      session_id: S, role: "main", frame_id: "ROOT", parent_frame_id: null, depth: 0, turn_id: turn,
    });
    // 10 turns proves there is no display cap (the "feels like 7" was old data).
    // Turns group by CONTIGUOUS turn_id, so keep t1's two round-trips adjacent.
    const marks: Record<string, DecodedMarks> = { r1: m("t1"), r1b: m("t1") };
    for (let i = 2; i <= 10; i++) marks[`r${i}`] = m(`t${i}`);

    const s = build(marks)[0];
    expect(s.agentRuns).toHaveLength(1);
    expect(s.agentRuns[0].turns).toHaveLength(10); // all ten turns render
    expect(s.agentRuns[0].turns[0].roundtrips).toHaveLength(2); // t1 has 2 RTs
  });
});
