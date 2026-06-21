// Story 057 — the rail nests each run's turns as clickable leaves, so a
// long multi-turn capture is navigable without scrolling the main pane.
// Clicking a turn reports (sessionId, runIdx, turnNum) and does NOT also
// fire the run-level select (the click is stopped at the leaf).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { SessionRail } from "./SessionRail";
import type { AgentRun, OodaSession, OodaTurn } from "../store/derived/ooda";

afterEach(cleanup);

function turn(turnNum: number, rts: number): OodaTurn {
  return {
    turnNum,
    startedAt: "2026-06-21T00:00:00Z",
    turnId: `turn-${turnNum}`,
    userInput: [],
    roundtrips: Array.from({ length: rts }, () => ({
      exchangeId: `e${turnNum}`,
      timestamp: "2026-06-21T00:00:00Z",
      userMessage: [],
      systemBlocks: [],
      systemMutated: false,
      assistant: [],
      continuesLoop: false,
    })),
  };
}

function run(index: number, isMain: boolean, turns: OodaTurn[]): AgentRun {
  return {
    index,
    frameId: isMain ? "ROOT" : `agent-${index}`,
    startedAt: "2026-06-21T00:00:00Z",
    systemPreview: "",
    isMain,
    depth: isMain ? 0 : 1,
    spawnedBy: null,
    parentRunIndex: isMain ? null : 1,
    turns,
  };
}

function session(): OodaSession {
  return {
    id: "sess-A",
    label: "sess-A",
    model: "claude-opus-4-8",
    lastActivity: "2026-06-21T00:00:00Z",
    agentRuns: [run(1, true, [turn(1, 2), turn(2, 1), turn(3, 4)])],
    auxiliary: [],
  };
}

function renderRail(overrides: Partial<Parameters<typeof SessionRail>[0]> = {}) {
  const onSelectTurn = vi.fn();
  const onSelectRun = vi.fn();
  render(
    <SessionRail
      sessions={[session()]}
      activeSessionId="sess-A"
      activeRunIdx={1}
      activeTurnNum={null}
      sort="newest"
      onSort={vi.fn()}
      onSelectSession={vi.fn()}
      onSelectRun={onSelectRun}
      onSelectTurn={onSelectTurn}
      {...overrides}
    />,
  );
  return { onSelectTurn, onSelectRun };
}

function turnLeaves(): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(".session-turn"));
}

describe("SessionRail — turn-tree navigation (story 057)", () => {
  it("renders each run's turns as leaves, in order, with round-trip counts", () => {
    renderRail();
    const leaves = turnLeaves();
    expect(leaves.map((l) => l.querySelector(".session-label")?.textContent)).toEqual([
      "·Turn 1",
      "·Turn 2",
      "·Turn 3",
    ]);
    // Turn 3 has 4 round-trips.
    expect(leaves[2].textContent).toContain("4 rt");
  });

  it("clicking a turn reports (sessionId, runIdx, turnNum) and not a run-select", () => {
    const { onSelectTurn, onSelectRun } = renderRail();
    const turn2 = turnLeaves().find((l) => l.textContent?.includes("Turn 2"));
    fireEvent.click(turn2!);
    expect(onSelectTurn).toHaveBeenCalledWith("sess-A", 1, 2);
    expect(onSelectRun).not.toHaveBeenCalled();
  });

  it("highlights the active turn only", () => {
    renderRail({ activeTurnNum: 3 });
    const active = document.querySelectorAll(".session-turn.active");
    expect(active).toHaveLength(1);
    expect(active[0].textContent).toContain("Turn 3");
  });
});
