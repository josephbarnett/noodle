// Attribution panel — item 4 viewer-panel slice (ADR 020 §7).
//
// Renders the rolling feed of side-effects from
// `side_effects.jsonl`. Primary content: `Resolved` records as
// per-session attribution rows (category → value table).
// Secondary: collapsible drill-down into contributing
// `Hint`/`Artifact`/`Audit` rows.
//
// Pure presentation. Reads `AttributionRow[]` from the store via
// `useSyncExternalStore`; renders newest-first; groups Resolveds
// by `session_prefix`.

import { useMemo, useState } from "react";
import type { AttributionRow } from "../store/events";
import type { SideEffectEvent } from "../types";

interface Props {
  rows: AttributionRow[];
  onClose: () => void;
}

interface SessionGroup {
  session_prefix: string;
  /** Newest Resolved for the session (drives the summary row). */
  latest_resolved: AttributionRow | undefined;
  /** All side-effects in the session, oldest → newest. */
  effects: AttributionRow[];
}

/** Group attribution rows by session_prefix, prefer the most
 *  recent Resolved as the headline value per session. Rows
 *  without a session (Hints emitted before a Resolved closes —
 *  they have no session field) are bucketed under
 *  `<no-session>`. */
function groupBySession(rows: AttributionRow[]): SessionGroup[] {
  const map = new Map<string, SessionGroup>();
  for (const row of rows) {
    const key = sessionPrefixOf(row.event) ?? "<no-session>";
    let group = map.get(key);
    if (!group) {
      group = { session_prefix: key, latest_resolved: undefined, effects: [] };
      map.set(key, group);
    }
    group.effects.push(row);
    if (row.event.kind === "resolved") {
      group.latest_resolved = row;
    }
  }
  // Newest session first — sort by the seq of the last effect.
  return Array.from(map.values()).sort((a, b) => {
    const aSeq = a.effects[a.effects.length - 1]?.seq ?? 0;
    const bSeq = b.effects[b.effects.length - 1]?.seq ?? 0;
    return bSeq - aSeq;
  });
}

function sessionPrefixOf(ev: SideEffectEvent): string | null {
  // Only Resolved carries a session_prefix on the wire. Hints,
  // Artifacts, Audits ride a flow_id but no session — we group
  // them under the Resolved that closed their flow only when
  // they share that flow_id (best-effort correlation).
  if (ev.kind === "resolved") return ev.session_prefix;
  return null;
}

function fmtTime(ms: number): string {
  const d = new Date(ms);
  return d.toLocaleTimeString(undefined, { hour12: false });
}

function ResolvedTable({
  resolved,
}: {
  resolved: Record<string, string>;
}) {
  const entries = Object.entries(resolved);
  if (entries.length === 0) {
    return (
      <div className="attribution-empty">
        no categories resolved
      </div>
    );
  }
  return (
    <table className="attribution-resolved">
      <tbody>
        {entries.map(([cat, val]) => (
          <tr key={cat}>
            <td className="attribution-cat">{cat}</td>
            <td className="attribution-val">{val}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function EffectRow({ row }: { row: AttributionRow }) {
  const { event } = row;
  switch (event.kind) {
    case "hint":
      return (
        <div className="attribution-effect attribution-hint">
          <span className="effect-kind">hint</span>
          <span className="effect-summary">
            {event.category} = <strong>{event.value}</strong>
          </span>
          <span className="effect-source">
            ({event.source}, {event.confidence.toFixed(2)})
          </span>
        </div>
      );
    case "artifact":
      return (
        <div className="attribution-effect attribution-artifact">
          <span className="effect-kind">artifact</span>
          <span className="effect-summary">
            {event.name} = <strong>{event.value}</strong>
          </span>
          <span className="effect-source">
            ({event.source_transform})
          </span>
        </div>
      );
    case "audit":
      return (
        <div className="attribution-effect attribution-audit">
          <span className="effect-kind">{event.kind_inner}</span>
          <span className="effect-summary">{event.transform}</span>
        </div>
      );
    case "resolved": {
      // Render the actual category=value pairs, not just a count —
      // the count-only summary hid what was resolved (e.g. you could
      // see "1 category" but not that it was `tool = Claude Code`).
      const cats = Object.entries(event.resolved);
      return (
        <div className="attribution-effect attribution-resolved-row">
          <span className="effect-kind">resolved</span>
          <span className="effect-summary">
            {cats.length === 0 ? (
              <em>no categories</em>
            ) : (
              cats.map(([cat, val], i) => (
                <span key={cat} className="resolved-pair">
                  {i > 0 ? ", " : ""}
                  {cat} = <strong>{val}</strong>
                </span>
              ))
            )}
          </span>
        </div>
      );
    }
  }
}

function SessionCard({ group }: { group: SessionGroup }) {
  const [expanded, setExpanded] = useState(false);
  const latest = group.latest_resolved;
  const resolvedMap =
    latest && latest.event.kind === "resolved" ? latest.event.resolved : {};

  return (
    <div className="attribution-session">
      <button
        type="button"
        className="attribution-session-head"
        onClick={() => setExpanded(!expanded)}
        aria-expanded={expanded}
      >
        <span className="attribution-chev">{expanded ? "▾" : "▸"}</span>
        <span className="attribution-session-prefix">
          {group.session_prefix}
        </span>
        <span className="attribution-effect-count">
          {group.effects.length} effect
          {group.effects.length === 1 ? "" : "s"}
        </span>
        {latest && (
          <span className="attribution-last-ts">
            {fmtTime(latest.received_unix_ms)}
          </span>
        )}
      </button>
      <ResolvedTable resolved={resolvedMap} />
      {expanded && (
        <div className="attribution-effects-list">
          {group.effects
            .slice()
            .reverse()
            .map((row) => (
              <EffectRow key={row.seq} row={row} />
            ))}
        </div>
      )}
    </div>
  );
}

export function AttributionPanel({ rows, onClose }: Props) {
  const groups = useMemo(() => groupBySession(rows), [rows]);
  const resolvedCount = rows.filter((r) => r.event.kind === "resolved").length;

  return (
    <aside className="attribution-panel">
      <header className="attribution-panel-head">
        <h2>Attribution</h2>
        <span className="attribution-counts">
          {resolvedCount} resolved · {rows.length} effects
        </span>
        <button
          type="button"
          className="attribution-close"
          onClick={onClose}
          aria-label="Close attribution panel"
        >
          ✕
        </button>
      </header>
      <div className="attribution-body">
        {groups.length === 0 ? (
          <div className="attribution-empty">
            No attribution records yet. Side effects appear here as
            the engine resolves them.
          </div>
        ) : (
          groups.map((g) => (
            <SessionCard key={g.session_prefix} group={g} />
          ))
        )}
      </div>
    </aside>
  );
}
