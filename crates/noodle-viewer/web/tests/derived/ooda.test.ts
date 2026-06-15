import { describe, expect, it } from "vitest";
import {
  buildSessions,
  type GetMarks,
  type OodaSession,
  type OodaTurn,
} from "../../src/store/derived/ooda";
import type { DecodedMarks, ExchangePair } from "../../src/types";

/** Test helper: builds the pairsById index automatically. */
function buildSessionsTest(pairs: ExchangePair[]): OodaSession[] {
  const m = new Map<string, ExchangePair>();
  for (const p of pairs) m.set(p.event_id, p);
  return buildSessions(pairs, m);
}

/** Test helper: flatten all turns across all agent runs in a session. */
function allTurns(s: OodaSession): OodaTurn[] {
  return s.agentRuns.flatMap((r) => r.turns);
}

function pair(opts: {
  id: string;
  url?: string;
  ts: string;
  sessionHash?: string;
  reqBody?: unknown;
  respBody?: unknown;
}): ExchangePair {
  return {
    event_id: opts.id,
    request: {
      direction: "request",
      timestamp: opts.ts,
      event_id: opts.id,
      provider: "anthropic",
      url: opts.url ?? "https://api.anthropic.com/v1/messages",
      method: "POST",
      session_hash: opts.sessionHash,
      headers: {},
      body: opts.reqBody,
    },
    response: {
      direction: "response",
      timestamp: opts.ts,
      event_id: opts.id,
      provider: "anthropic",
      status: 200,
      headers: {},
      body: opts.respBody,
    },
  };
}

/** §5 marks for a ROOT (main-agent) round-trip. */
function rootMarks(sessionId: string, turnId: string): DecodedMarks {
  return {
    session_id: sessionId,
    role: "main",
    frame_id: "ROOT",
    parent_frame_id: null,
    depth: 0,
    turn_id: turnId,
  };
}

/** Build a `GetMarks` accessor from an explicit golden map. */
function golden(map: Record<string, DecodedMarks>): GetMarks {
  return (id) => map[id] ?? null;
}

describe("buildSessions", () => {
  it("returns empty when no pairs", () => {
    expect(buildSessionsTest([])).toEqual([]);
  });

  it("filters non-chat URLs", () => {
    const p = pair({
      id: "nl-1",
      url: "https://api.anthropic.com/v1/organizations",
      ts: "2026-05-10T00:00:00Z",
      sessionHash: "sess-A",
    });
    expect(buildSessionsTest([p])).toEqual([]);
  });

  it("classifies max_tokens=1 calls as quota probes (auxiliary)", () => {
    const p = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: {
        max_tokens: 1,
        messages: [{ role: "user", content: "quota" }],
      },
      respBody: {
        content: [{ type: "text", text: "#" }],
        stop_reason: "max_tokens",
      },
    });
    const [s] = buildSessionsTest([p]);
    expect(allTurns(s)).toHaveLength(0);
    expect(s.auxiliary).toHaveLength(1);
    expect(s.auxiliary[0].kind).toBe("quota");
  });

  it("classifies title-generation responses as auxiliary", () => {
    const p = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: {
        max_tokens: 100,
        messages: [{ role: "user", content: "Tell me a joke" }],
      },
      respBody: {
        content: [{ type: "text", text: '{"title": "Tell me a joke"}' }],
        stop_reason: "end_turn",
      },
    });
    const [s] = buildSessionsTest([p]);
    expect(allTurns(s)).toHaveLength(0);
    expect(s.auxiliary).toHaveLength(1);
    expect(s.auxiliary[0].kind).toBe("title");
    expect(s.auxiliary[0].summary).toContain("Tell me a joke");
  });

  it("turn boundary uses prior round-trip's stop_reason (tool_use continues; end_turn ends)", () => {
    // RT1: stop_reason: tool_use → RT2 should fold into the same turn
    const a = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: { messages: [{ role: "user", content: "do it" }] },
      respBody: {
        content: [
          { type: "tool_use", id: "tu1", name: "Read", input: {} },
        ],
        stop_reason: "tool_use",
      },
    });
    // RT2: continuation (tool_result), stop_reason: end_turn → RT3 starts new turn
    const b = pair({
      id: "nl-2",
      ts: "2026-05-10T00:00:02Z",
      sessionHash: "s",
      reqBody: {
        messages: [
          { role: "user", content: "do it" },
          { role: "assistant", content: [{ type: "tool_use", id: "tu1", name: "Read", input: {} }] },
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu1", content: "done" }] },
        ],
      },
      respBody: {
        content: [{ type: "text", text: "all done" }],
        stop_reason: "end_turn",
      },
    });
    // RT3: a brand-new user input after end_turn → new turn
    const c = pair({
      id: "nl-3",
      ts: "2026-05-10T00:00:03Z",
      sessionHash: "s",
      reqBody: { messages: [{ role: "user", content: "next question" }] },
      respBody: {
        content: [{ type: "text", text: "answer" }],
        stop_reason: "end_turn",
      },
    });
    const [s] = buildSessionsTest([a, b, c]);
    expect(allTurns(s)).toHaveLength(2);
    expect(allTurns(s)[0].roundtrips).toHaveLength(2); // a and b folded
    expect(allTurns(s)[1].roundtrips).toHaveLength(1); // c
  });

  it("marks fold RTs by turn_id regardless of stop_reason (ADR 052 §6)", () => {
    // Two ROOT RTs whose response bodies have NO stop_reason — the
    // legacy heuristic would split them into two turns. With §5 marks
    // both RTs carry the same turn_id and must fold into one turn.
    // This is the regression: "it thinks that there are two turns ...
    // if I refresh, the logic does in fact fix it to one turn."
    const a = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "sess-A",
      reqBody: { messages: [{ role: "user", content: "do it" }] },
      respBody: { content: [{ type: "text", text: "ok" }] },
    });
    const b = pair({
      id: "nl-2",
      ts: "2026-05-10T00:00:02Z",
      sessionHash: "sess-A",
      reqBody: {
        messages: [
          { role: "user", content: "do it" },
          { role: "assistant", content: "ok" },
        ],
      },
      respBody: { content: [{ type: "text", text: "more" }] },
    });
    const pairsById = new Map<string, ExchangePair>();
    pairsById.set(a.event_id, a);
    pairsById.set(b.event_id, b);

    // Without marks → legacy heuristic splits into two turns.
    const heuristicSessions = buildSessions([a, b], pairsById);
    expect(allTurns(heuristicSessions[0])).toHaveLength(2);

    // With marks pinning both to the same turn_id → one turn.
    const sameTurn: GetMarks = () => rootMarks("sess-A", "01KTURN0001");
    const marksSessions = buildSessions([a, b], pairsById, undefined, sameTurn);
    expect(allTurns(marksSessions[0])).toHaveLength(1);
    expect(allTurns(marksSessions[0])[0].roundtrips).toHaveLength(2);
  });

  it("marks split RTs into new turns when turn_id changes (proxy authoritative)", () => {
    // RT1 has stop_reason: tool_use — the heuristic would fold RT2.
    // But the §5 marks say different turns, so the proxy wins: split.
    const a = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "sess-A",
      reqBody: { messages: [{ role: "user", content: "go" }] },
      respBody: {
        content: [{ type: "tool_use", id: "tu1", name: "Read", input: {} }],
        stop_reason: "tool_use",
      },
    });
    const b = pair({
      id: "nl-2",
      ts: "2026-05-10T00:00:02Z",
      sessionHash: "sess-A",
      reqBody: {
        messages: [
          { role: "user", content: "go" },
          { role: "assistant", content: [{ type: "tool_use", id: "tu1", name: "Read", input: {} }] },
          { role: "user", content: [{ type: "tool_result", tool_use_id: "tu1", content: "" }] },
        ],
      },
      respBody: {
        content: [{ type: "text", text: "done" }],
        stop_reason: "end_turn",
      },
    });
    const pairsById = new Map<string, ExchangePair>();
    pairsById.set(a.event_id, a);
    pairsById.set(b.event_id, b);

    const marks: GetMarks = (id) =>
      rootMarks("sess-A", id === "nl-1" ? "01KTURNA" : "01KTURNB");
    const sessions = buildSessions([a, b], pairsById, undefined, marks);
    expect(allTurns(sessions[0])).toHaveLength(2);
    expect(allTurns(sessions[0])[0].roundtrips).toHaveLength(1);
    expect(allTurns(sessions[0])[1].roundtrips).toHaveLength(1);
  });

  it("two pairs with same session, both user-text → one session, two turns", () => {
    const a = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "sess-A",
      reqBody: { model: "claude", messages: [{ role: "user", content: "hi" }] },
      respBody: {
        content: [{ type: "text", text: "hello" }],
        stop_reason: "end_turn",
      },
    });
    const b = pair({
      id: "nl-2",
      ts: "2026-05-10T00:00:02Z",
      sessionHash: "sess-A",
      reqBody: {
        model: "claude",
        messages: [
          { role: "user", content: "hi" },
          { role: "assistant", content: "hello" },
          { role: "user", content: "again" },
        ],
      },
      respBody: {
        content: [{ type: "text", text: "ok" }],
        stop_reason: "end_turn",
      },
    });
    const sessions = buildSessionsTest([a, b]);
    expect(sessions).toHaveLength(1);
    expect(allTurns(sessions[0])).toHaveLength(2);
    expect(allTurns(sessions[0])[0].turnNum).toBe(1);
    expect(allTurns(sessions[0])[1].turnNum).toBe(2);
    expect(allTurns(sessions[0])[0].roundtrips).toHaveLength(1);
    expect(allTurns(sessions[0])[1].roundtrips).toHaveLength(1);
    expect(sessions[0].model).toBe("claude");
  });

  it("extracts latest user input (text content) into turn.userInput", () => {
    const p = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: {
        messages: [
          { role: "user", content: "first" },
          { role: "assistant", content: "ans" },
          { role: "user", content: "second" },
        ],
      },
      respBody: { content: [], stop_reason: "end_turn" },
    });
    const [s] = buildSessionsTest([p]);
    expect(allTurns(s)[0].userInput).toEqual([{ type: "text", text: "second" }]);
  });

  it("tool-loop continuation folds into the same turn with paired tool_use/tool_result", () => {
    // Roundtrip 1: user asks, assistant requests a tool.
    const a = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "s",
      reqBody: { messages: [{ role: "user", content: "fix the bug" }] },
      respBody: {
        content: [
          { type: "thinking", thinking: "I should read the file first." },
          {
            type: "tool_use",
            id: "toolu_001",
            name: "Read",
            input: { file_path: "/x/y.rs" },
          },
        ],
        stop_reason: "tool_use",
      },
    });
    // Roundtrip 2: user supplies ONLY a tool_result (loop continuation),
    // assistant gives final text.
    const b = pair({
      id: "nl-2",
      ts: "2026-05-10T00:00:02Z",
      sessionHash: "s",
      reqBody: {
        messages: [
          { role: "user", content: "fix the bug" },
          {
            role: "assistant",
            content: [{ type: "tool_use", id: "toolu_001", name: "Read", input: {} }],
          },
          {
            role: "user",
            content: [
              {
                type: "tool_result",
                tool_use_id: "toolu_001",
                content: "file contents here",
              },
            ],
          },
        ],
      },
      respBody: {
        content: [{ type: "text", text: "done" }],
        stop_reason: "end_turn",
      },
    });

    const [s] = buildSessionsTest([a, b]);
    // Both roundtrips fold into a single turn since b's user input
    // is exclusively a tool_result.
    expect(allTurns(s)).toHaveLength(1);
    expect(allTurns(s)[0].roundtrips).toHaveLength(2);

    // Tool pairing inside the turn.
    const firstAssistant = allTurns(s)[0].roundtrips[0].assistant;
    const tu = firstAssistant.find((bl) => bl.type === "tool_use");
    expect(tu).toBeDefined();
    if (tu && tu.type === "tool_use") {
      expect(tu.result).not.toBeNull();
      expect(tu.result!.tool_use_id).toBe("toolu_001");
      expect(tu.result!.content).toBe("file contents here");
    }
  });

  it("synthesizes anonymous session from host when no session_hash", () => {
    const p = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      reqBody: { messages: [{ role: "user", content: "hi" }] },
      respBody: { content: [{ type: "text", text: "ok" }] },
    });
    const [s] = buildSessionsTest([p]);
    expect(s.id).toBe("anon-api.anthropic.com");
  });

  describe("frame tree (ADR 052 §3/§5/§6 — render, do not re-derive)", () => {
    it("a single ROOT round-trip → one main frame, no spawn", () => {
      const a = pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "hi" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      });
      const pairsById = new Map([[a.event_id, a]]);
      const marks = golden({ "nl-1": rootMarks("sess-A", "T1") });
      const [s] = buildSessions([a], pairsById, undefined, marks);
      expect(s.id).toBe("sess-A");
      expect(s.agentRuns).toHaveLength(1);
      expect(s.agentRuns[0].frameId).toBe("ROOT");
      expect(s.agentRuns[0].isMain).toBe(true);
      expect(s.agentRuns[0].depth).toBe(0);
      expect(s.agentRuns[0].spawnedBy).toBeNull();
    });

    it("a sub-agent frame nests under ROOT via parent_frame_id, with spawn metadata from the Agent tool_use", () => {
      // RT1 (ROOT): user prompt → Agent tool_use(toolu_SUB).
      const r1 = pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "review the diff" }] },
        respBody: {
          content: [
            {
              type: "tool_use",
              id: "toolu_SUB",
              name: "Agent",
              input: { subagent_type: "code-reviewer", description: "Review", prompt: "review" },
            },
          ],
          stop_reason: "tool_use",
        },
      });
      // RT-sub: the sub-agent frame's own round-trip.
      const rsub = pair({
        id: "nl-sub",
        ts: "2026-05-10T00:00:02Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "review" }] },
        respBody: { content: [{ type: "text", text: "looks good" }], stop_reason: "end_turn" },
      });
      // RT3 (ROOT continuation): tool_result(toolu_SUB) → back to ROOT.
      const r3 = pair({
        id: "nl-3",
        ts: "2026-05-10T00:00:03Z",
        sessionHash: "s",
        reqBody: {
          messages: [
            { role: "user", content: "review the diff" },
            { role: "assistant", content: [{ type: "tool_use", id: "toolu_SUB", name: "Agent", input: {} }] },
            { role: "user", content: [{ type: "tool_result", tool_use_id: "toolu_SUB", content: "looks good" }] },
          ],
        },
        respBody: { content: [{ type: "text", text: "done" }], stop_reason: "end_turn" },
      });
      const pairs = [r1, rsub, r3];
      const pairsById = new Map(pairs.map((p) => [p.event_id, p]));
      const marks = golden({
        "nl-1": rootMarks("sess-A", "T1"),
        "nl-3": rootMarks("sess-A", "T1"),
        "nl-sub": {
          session_id: "sess-A",
          role: "sub_agent",
          frame_id: "toolu_SUB",
          parent_frame_id: "ROOT",
          depth: 1,
          turn_id: "T1",
        },
      });

      const [s] = buildSessions(pairs, pairsById, undefined, marks);
      // Two frames: ROOT + the sub-agent (nl-3 returns to ROOT, not a 3rd frame).
      expect(s.agentRuns).toHaveLength(2);
      const root = s.agentRuns.find((r) => r.frameId === "ROOT")!;
      const sub = s.agentRuns.find((r) => r.frameId === "toolu_SUB")!;
      expect(root.isMain).toBe(true);
      expect(sub.isMain).toBe(false);
      expect(sub.depth).toBe(1);
      expect(sub.parentRunIndex).toBe(root.index);
      expect(sub.spawnedBy?.toolUseId).toBe("toolu_SUB");
      expect(sub.spawnedBy?.subagentType).toBe("code-reviewer");

      // ROOT owns nl-1 + nl-3 (one turn T1); sub owns nl-sub.
      const rootRTs = root.turns.flatMap((t) => t.roundtrips).map((rt) => rt.exchangeId);
      const subRTs = sub.turns.flatMap((t) => t.roundtrips).map((rt) => rt.exchangeId);
      expect(rootRTs).toEqual(["nl-1", "nl-3"]);
      expect(subRTs).toEqual(["nl-sub"]);
    });

    it("a side_call round-trip is off-tree (auxiliary lane), carries no frame", () => {
      const root = pair({
        id: "nl-1",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "go" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      });
      const monitor = pair({
        id: "nl-mon",
        ts: "2026-05-10T00:00:02Z",
        sessionHash: "s",
        reqBody: { messages: [{ role: "user", content: "<transcript>…</transcript>" }] },
        respBody: { content: [{ type: "text", text: "{}" }], stop_reason: "end_turn" },
      });
      const pairs = [root, monitor];
      const pairsById = new Map(pairs.map((p) => [p.event_id, p]));
      const marks = golden({
        "nl-1": rootMarks("sess-A", "T1"),
        "nl-mon": {
          session_id: "sess-A",
          role: "side_call",
          frame_id: null,
          parent_frame_id: null,
          depth: null,
          turn_id: null,
        },
      });

      const [s] = buildSessions(pairs, pairsById, undefined, marks);
      expect(s.agentRuns).toHaveLength(1);
      expect(s.agentRuns[0].frameId).toBe("ROOT");
      // The monitor is a side-call — not a frame, not a turn.
      expect(s.agentRuns.some((r) => r.frameId === "nl-mon")).toBe(false);
      expect(s.auxiliary.map((a) => a.exchangeId)).toContain("nl-mon");
    });

    it("partitions frames into separate sessions by marks.session_id", () => {
      const x = pair({
        id: "nl-x",
        ts: "2026-05-10T00:00:01Z",
        sessionHash: "ignored",
        reqBody: { messages: [{ role: "user", content: "s1" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      });
      const y = pair({
        id: "nl-y",
        ts: "2026-05-10T00:00:02Z",
        sessionHash: "ignored",
        reqBody: { messages: [{ role: "user", content: "s2" }] },
        respBody: { content: [{ type: "text", text: "ok" }], stop_reason: "end_turn" },
      });
      const pairs = [x, y];
      const pairsById = new Map(pairs.map((p) => [p.event_id, p]));
      const marks = golden({
        "nl-x": rootMarks("sess-1", "T1"),
        "nl-y": rootMarks("sess-2", "T1"),
      });
      const sessions = buildSessions(pairs, pairsById, undefined, marks);
      expect(sessions.map((s) => s.id).sort()).toEqual(["sess-1", "sess-2"]);
    });
  });

  it("records lastActivity from the most recent roundtrip for caller-side sorting", () => {
    // buildSessions is order-agnostic now (the rail does the
    // newest/oldest toggle). It just needs to report lastActivity
    // accurately so callers can sort.
    const older = pair({
      id: "nl-1",
      ts: "2026-05-10T00:00:01Z",
      sessionHash: "A",
      reqBody: { messages: [{ role: "user", content: "hi" }] },
      respBody: { content: [], stop_reason: "end_turn" },
    });
    const newer = pair({
      id: "nl-2",
      ts: "2026-05-10T01:00:00Z",
      sessionHash: "B",
      reqBody: { messages: [{ role: "user", content: "hi" }] },
      respBody: { content: [], stop_reason: "end_turn" },
    });
    const sessions = buildSessionsTest([older, newer]);
    const byId = new Map(sessions.map((s) => [s.id, s]));
    expect(byId.get("A")!.lastActivity).toBe("2026-05-10T00:00:01Z");
    expect(byId.get("B")!.lastActivity).toBe("2026-05-10T01:00:00Z");
  });
});
