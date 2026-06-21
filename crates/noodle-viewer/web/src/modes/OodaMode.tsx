import { useEffect, useMemo, useState } from "react";
import type { ExchangePair } from "../types";
import { OodaThread } from "../components/OodaThread";
import { SessionRail, type SessionSort } from "../components/SessionRail";
import { buildSessions, type GetMarks, type ParseCache } from "../store/derived/ooda";

interface Props {
  pairs: ExchangePair[];
  /** Optional memoization cache passed through to `buildSessions`
   *  so SSE/JSON parses survive across ingest ticks. Owned by the
   *  caller (typically `EventStore`) so its lifecycle matches the
   *  capture, not the React render. */
  parseCache?: ParseCache;
  /** Resolve an exchange's §5 `DecodedMarks` (ADR 052). When
   *  supplied, the frame tree and turn boundaries come straight from
   *  the proxy — the UI renders the marks, it does not re-derive the
   *  tree from response bodies (ADR 052 §6). */
  getMarks?: GetMarks;
}

const SORT_STORAGE_KEY = "noodle-viewer:sessionSort";

function readSort(): SessionSort {
  if (typeof window === "undefined") return "newest";
  const v = window.localStorage.getItem(SORT_STORAGE_KEY);
  return v === "oldest" ? "oldest" : "newest";
}

export function OodaMode({ pairs, parseCache, getMarks }: Props) {
  const pairsById = useMemo(() => {
    const m = new Map<string, ExchangePair>();
    for (const p of pairs) m.set(p.event_id, p);
    return m;
  }, [pairs]);
  const rawSessions = useMemo(
    () => buildSessions(pairs, pairsById, parseCache, getMarks),
    [pairs, pairsById, parseCache, getMarks],
  );

  const [sort, setSort] = useState<SessionSort>(() => readSort());
  const [activeId, setActiveId] = useState<string | null>(null);
  // When a session is selected, the user can drill into a specific
  // agent run inside it. Default = main agent (the first run).
  const [activeRunIdx, setActiveRunIdx] = useState<number>(1);
  // Turn the rail asked the main pane to reveal (story 057). `null`
  // until the user clicks a turn leaf; cleared when the run/session
  // changes by other means so no stale highlight lingers.
  const [activeTurnNum, setActiveTurnNum] = useState<number | null>(null);

  useEffect(() => {
    window.localStorage.setItem(SORT_STORAGE_KEY, sort);
  }, [sort]);

  const sessions = useMemo(() => {
    const arr = [...rawSessions];
    arr.sort((a, b) => a.lastActivity.localeCompare(b.lastActivity));
    return sort === "newest" ? arr.reverse() : arr;
  }, [rawSessions, sort]);

  useEffect(() => {
    if (sessions.length === 0) return;
    if (activeId === null || !sessions.some((s) => s.id === activeId)) {
      setActiveId(sessions[0].id);
    }
  }, [sessions, activeId]);

  const active = useMemo(
    () => sessions.find((s) => s.id === activeId) ?? null,
    [sessions, activeId],
  );
  const activeRun = useMemo(() => {
    if (!active) return null;
    return active.agentRuns.find((r) => r.index === activeRunIdx)
      ?? active.agentRuns[0]
      ?? null;
  }, [active, activeRunIdx]);

  if (sessions.length === 0) {
    return (
      <div className="empty">
        Waiting for chat traffic. Drive a request to <code>/v1/messages</code>
        through the proxy to see the conversation thread here.
      </div>
    );
  }

  return (
    <div className="ooda-mode">
      <SessionRail
        sessions={sessions}
        activeSessionId={activeId}
        activeRunIdx={activeRun?.index ?? null}
        activeTurnNum={activeTurnNum}
        sort={sort}
        onSort={setSort}
        onSelectSession={(id) => {
          setActiveId(id);
          setActiveRunIdx(1);
          setActiveTurnNum(null);
        }}
        onSelectRun={(sid, runIdx) => {
          setActiveId(sid);
          setActiveRunIdx(runIdx);
          setActiveTurnNum(null);
        }}
        onSelectTurn={(sid, runIdx, turnNum) => {
          setActiveId(sid);
          setActiveRunIdx(runIdx);
          setActiveTurnNum(turnNum);
        }}
      />
      <div className="ooda-thread">
        {active && activeRun ? (
          <>
            <header className="ooda-session-head">
              <h2>{active.label}</h2>
              {active.model && <span className="ooda-model">{active.model}</span>}
              <span className="ooda-turn-count">
                {active.agentRuns.length} agent run
                {active.agentRuns.length === 1 ? "" : "s"}
                {active.auxiliary.length > 0
                  ? ` · ${active.auxiliary.length} aux`
                  : ""}
              </span>
            </header>
            <div className="agent-run-head">
              <div className="agent-run-title">
                {activeRun.isMain ? "Main agent" : "Sub-agent"}
                {activeRun.spawnedBy?.subagentType && (
                  <span className="agent-run-subtype">
                    {activeRun.spawnedBy.subagentType}
                  </span>
                )}
                {activeRun.spawnedBy?.runInBackground && (
                  <span className="agent-run-bg">(background)</span>
                )}
              </div>
              <div className="agent-run-meta">
                run #{activeRun.index} · {activeRun.turns.length} turn
                {activeRun.turns.length === 1 ? "" : "s"} ·{" "}
                {activeRun.turns.reduce((n, t) => n + t.roundtrips.length, 0)} round-trip
                {activeRun.turns.reduce((n, t) => n + t.roundtrips.length, 0) === 1 ? "" : "s"}
              </div>
              {activeRun.spawnedBy?.description && (
                <div className="agent-run-desc">
                  {activeRun.spawnedBy.description}
                </div>
              )}
            </div>
            <OodaThread
              session={active}
              run={activeRun}
              pairsById={pairsById}
              onJumpToRun={(runIdx) => {
                setActiveRunIdx(runIdx);
                setActiveTurnNum(null);
              }}
              activeTurn={activeTurnNum}
            />
          </>
        ) : (
          <div className="empty">Select a session.</div>
        )}
      </div>
    </div>
  );
}
