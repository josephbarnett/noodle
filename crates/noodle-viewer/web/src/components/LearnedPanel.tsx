// LEARNED panel (ADR 051) — the per-round-trip knowledge noodle
// extracted from one round-trip's bytes, shown beside the raw
// traffic. Traffic in, knowledge out. Renders attribution (with the
// per-turn delta), the evidence behind each value, lineage, tool
// pairing, and the context-token delta.
//
// Pure presentation over a LearnedRecord. Distinguishes "nothing
// resolved for this round-trip" from "this round-trip wasn't
// classified" — never implies a value was learned when none was.

import type { LearnedRecord } from "../store/events";

interface Props {
  learned: LearnedRecord | undefined;
  /** Jump to a paired round-trip by event_id (tool_use ↔ result). */
  onJumpTo?: (eventId: string) => void;
}

function fmtSigned(n: number): string {
  return n > 0 ? `+${n}` : `${n}`;
}

export function LearnedPanel({ learned, onJumpTo }: Props) {
  if (!learned) return null;

  const { attribution, evidence, lineage, pairing, context } = learned;
  const values = Object.entries(attribution.values);
  const hasLineage = !!lineage.parent_frame_id;
  const hasPairing =
    !!pairing.resolves_tool_use_in_request_id || !!pairing.resolved_by_request_id;
  const hasContext = context.input_tokens != null;

  return (
    <section className="learned-panel">
      <h4 className="learned-head">LEARNED</h4>

      {/* Attribution — the resolved classification, with the change
          from the prior round-trip in this turn. */}
      <div className="learned-block learned-attribution">
        {values.length === 0 ? (
          <div className="learned-empty">no attribution resolved for this round-trip</div>
        ) : (
          <table className="learned-attr-table">
            <tbody>
              {values.map(([cat, val]) => {
                const changed = cat in attribution.delta;
                const prev = attribution.delta[cat];
                return (
                  <tr key={cat} className={changed ? "learned-changed" : ""}>
                    <td className="learned-cat">{cat}</td>
                    <td className="learned-val">
                      <strong>{val}</strong>
                      {changed && (
                        <span className="learned-delta" title="changed from prior round-trip in this turn">
                          {" "}
                          ← {prev ?? "∅"}
                        </span>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>

      {/* Evidence — which hint/artifact produced the values. */}
      {evidence.length > 0 && (
        <div className="learned-block learned-evidence">
          <div className="learned-sub">evidence</div>
          {evidence.map((e, i) => (
            <div key={`${e.category}-${i}`} className="learned-evidence-row">
              <span className={`learned-evidence-kind ${e.kind}`}>{e.kind}</span>
              <span className="learned-evidence-summary">
                {e.category} = <strong>{e.value}</strong>
              </span>
              <span className="learned-evidence-source">
                ({e.source}
                {e.confidence != null ? `, ${e.confidence.toFixed(2)}` : ""})
              </span>
            </div>
          ))}
        </div>
      )}

      {/* Lineage — the spawning parent frame (ADR 052 §5). The parent
          frame's id is itself the spawning `tool_use.id`. */}
      {hasLineage && (
        <div className="learned-block learned-lineage">
          <div className="learned-sub">lineage</div>
          <div className="learned-lineage-row">
            child of frame <code>{lineage.parent_frame_id}</code>
            {lineage.frame_id && (
              <span className="learned-muted">
                {" "}
                · this frame <code>{lineage.frame_id}</code>
              </span>
            )}
          </div>
        </div>
      )}

      {/* Pairing — tool_use ↔ tool_result across round-trips. */}
      {hasPairing && (
        <div className="learned-block learned-pairing">
          <div className="learned-sub">tool pairing</div>
          {pairing.resolves_tool_use_in_request_id && (
            <button
              type="button"
              className="learned-jump"
              onClick={() =>
                onJumpTo?.(pairing.resolves_tool_use_in_request_id as string)
              }
              title="Jump to the round-trip that emitted this tool_use"
            >
              closes tool_use from {pairing.resolves_tool_use_in_request_id} →
            </button>
          )}
          {pairing.resolved_by_request_id && (
            <button
              type="button"
              className="learned-jump"
              onClick={() => onJumpTo?.(pairing.resolved_by_request_id as string)}
              title="Jump to the round-trip whose tool_result answered this tool_use"
            >
              answered by {pairing.resolved_by_request_id} →
            </button>
          )}
        </div>
      )}

      {/* Context — token growth/shrink vs the prior round-trip. */}
      {hasContext && (
        <div className="learned-block learned-context">
          <div className="learned-sub">context</div>
          <div className="learned-context-row">
            <span>input {context.input_tokens?.toLocaleString()}</span>
            {context.input_delta != null && (
              <span
                className={
                  "learned-delta " +
                  (context.input_delta > 0
                    ? "grow"
                    : context.input_delta < 0
                      ? "shrink"
                      : "")
                }
                title="change vs prior round-trip in this turn"
              >
                {fmtSigned(context.input_delta)}
              </span>
            )}
            {context.cache_read_input_tokens != null && (
              <span className="learned-muted">
                cache-read {context.cache_read_input_tokens.toLocaleString()}
              </span>
            )}
            {context.cache_creation_input_tokens != null && (
              <span className="learned-muted">
                cache-write {context.cache_creation_input_tokens.toLocaleString()}
              </span>
            )}
          </div>
        </div>
      )}
    </section>
  );
}
