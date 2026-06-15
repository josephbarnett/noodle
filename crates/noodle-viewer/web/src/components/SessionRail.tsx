import type { AgentRun, OodaSession } from "../store/derived/ooda";

/**
 * Order `runs` so each parent appears immediately before its children
 * (depth-first), and assign a tree depth per run for indent
 * rendering. Depth 0 = root (no parent in the session); depth 1 = child
 * of a root; depth 2 = grandchild; ... — see ADR 048 §11 item 0.
 *
 * Roots themselves are emitted in chronological (insertion) order.
 * Within a parent, children are emitted in chronological order. Cycles
 * (shouldn't happen — `parentRunIndex` is a stable lookup, not a
 * mutable pointer) cause the offending run to render at depth 0 as a
 * defensive fallback.
 */
function flattenRunsForTree(runs: AgentRun[]): { run: AgentRun; depth: number }[] {
  if (runs.length === 0) return [];
  const byIndex = new Map<number, AgentRun>();
  for (const r of runs) byIndex.set(r.index, r);
  const children = new Map<number | null, AgentRun[]>();
  for (const r of runs) {
    const parent = r.parentRunIndex != null && byIndex.has(r.parentRunIndex)
      ? r.parentRunIndex
      : null;
    const list = children.get(parent);
    if (list) list.push(r);
    else children.set(parent, [r]);
  }
  const out: { run: AgentRun; depth: number }[] = [];
  const visited = new Set<number>();
  const visit = (parent: number | null, depth: number) => {
    const kids = children.get(parent);
    if (!kids) return;
    for (const r of kids) {
      if (visited.has(r.index)) continue; // defensive: cycle
      visited.add(r.index);
      out.push({ run: r, depth });
      visit(r.index, depth + 1);
    }
  };
  visit(null, 0);
  // Any run not reached (orphan cycle) falls in at depth 0 in original order.
  for (const r of runs) {
    if (!visited.has(r.index)) out.push({ run: r, depth: 0 });
  }
  return out;
}

export type SessionSort = "newest" | "oldest";

interface Props {
  sessions: OodaSession[];
  activeSessionId: string | null;
  activeRunIdx: number | null;
  sort: SessionSort;
  onSort: (s: SessionSort) => void;
  onSelectSession: (id: string) => void;
  onSelectRun: (sessionId: string, runIdx: number) => void;
}

export function SessionRail({
  sessions,
  activeSessionId,
  activeRunIdx,
  sort,
  onSort,
  onSelectSession,
  onSelectRun,
}: Props) {
  return (
    <aside className="session-rail">
      <header className="session-rail-head">
        <span>Sessions ({sessions.length})</span>
        <button
          className="session-sort"
          onClick={() => onSort(sort === "newest" ? "oldest" : "newest")}
          title="Toggle session sort order"
        >
          {sort === "newest" ? "↓ newest" : "↑ oldest"}
        </button>
      </header>
      <ul className="session-list">
        {sessions.map((s) => (
          <SessionItem
            key={s.id}
            session={s}
            isActiveSession={s.id === activeSessionId}
            activeRunIdx={s.id === activeSessionId ? activeRunIdx : null}
            onSelectSession={() => onSelectSession(s.id)}
            onSelectRun={(idx) => onSelectRun(s.id, idx)}
          />
        ))}
      </ul>
    </aside>
  );
}

function SessionItem({
  session,
  isActiveSession,
  activeRunIdx,
  onSelectSession,
  onSelectRun,
}: {
  session: OodaSession;
  isActiveSession: boolean;
  activeRunIdx: number | null;
  onSelectSession: () => void;
  onSelectRun: (idx: number) => void;
}) {
  const totalRoundtrips = session.agentRuns.reduce(
    (n, r) => n + r.turns.reduce((m, t) => m + t.roundtrips.length, 0),
    0,
  );
  return (
    <>
      <li
        className={`session-item depth-0${isActiveSession ? " active" : ""}`}
        onClick={onSelectSession}
      >
        <div className="session-label" title={session.id}>
          {session.label}
        </div>
        <div className="session-meta">
          {session.model && <span className="session-model">{session.model}</span>}
          <span className="session-ts">{shortTime(session.lastActivity)}</span>
        </div>
        <div className="session-meta-secondary">
          {session.agentRuns.length} run{session.agentRuns.length === 1 ? "" : "s"}
          {" · "}
          {totalRoundtrips} call{totalRoundtrips === 1 ? "" : "s"}
        </div>
      </li>
      {flattenRunsForTree(session.agentRuns).map(({ run, depth }) => {
        // Depth 0 root agents sit one level under the session;
        // children indent further per ADR 048 §11 item 0.
        const itemDepth = depth + 1;
        return (
          <li
            key={run.index}
            className={`session-item depth-${itemDepth}${
              isActiveSession && activeRunIdx === run.index ? " active" : ""
            }`}
            style={{ paddingLeft: `${0.6 + depth * 1.1}rem` }}
            onClick={() => onSelectRun(run.index)}
          >
            <div className="session-label" title={`run #${run.index}`}>
              <span className="session-tree">↳</span>
              {run.isMain
                ? "main agent"
                : run.spawnedBy?.subagentType ?? `sub-agent #${run.index}`}
              {run.spawnedBy?.runInBackground && (
                <span className="session-bg-tag">bg</span>
              )}
            </div>
            <div className="session-meta">
              <span className="session-ts">{shortTime(run.startedAt)}</span>
            </div>
            <div className="session-meta-secondary">
              {run.turns.length} turn{run.turns.length === 1 ? "" : "s"} ·{" "}
              {run.turns.reduce((n, t) => n + t.roundtrips.length, 0)} rt
            </div>
          </li>
        );
      })}
    </>
  );
}

function shortTime(ts: string): string {
  if (!ts) return "";
  const m = ts.match(/T(\d{2}:\d{2}:\d{2})/);
  return m?.[1] ?? "";
}
