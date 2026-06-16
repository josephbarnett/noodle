// Side panel showing the full request and response for the selected
// exchange. Each section (REQUEST / RESPONSE) is independently
// collapsible; the collapse choice persists across row selections
// via localStorage, so an operator can fold REQUEST once and then
// scan response bodies across many rows.

import { BeforeAfterPanel } from "./BeforeAfterPanel";
import { BodyView } from "./BodyView";
import { ContentBlockTags } from "./ContentBlockTags";
import { EnvelopeInspector } from "./EnvelopeInspector";
import { LearnedPanel } from "./LearnedPanel";
import { ToolPairingArrow } from "./ToolPairingArrow";
import { TurnIdBadge } from "./TurnIdBadge";
import { UsagePanel } from "./UsagePanel";
import { usePersistedToggle } from "../lib/persistedToggle";
import type { LearnedRecord } from "../store/events";
import type { ContextWeight, DecodedExchange, Exchange, ExchangePair } from "../types";

interface Props {
  pair: ExchangePair;
  onClose: () => void;
  /** S22: typed decoded layer for the selected row, if any.
   *  Renders panels for marks/usage/envelope/content blocks/
   *  pairing alongside the existing request/response sections. */
  decoded?: DecodedExchange;
  /** ADR 051: the per-round-trip LEARNED record — what noodle
   *  extracted from this round-trip's bytes. */
  learned?: LearnedRecord;
  /** Optional callback for tool-pairing arrows — jumps to the
   *  paired event_id. */
  onJumpTo?: (eventId: string) => void;
  /** ADR 056: per-round-trip context weight — carried vs new tokens
   *  plus structural system/tools/preamble sizes. */
  contextWeight?: ContextWeight;
}

const REQ_OPEN_KEY = "noodle-viewer:rowDetail.request.open";
const RES_OPEN_KEY = "noodle-viewer:rowDetail.response.open";

export function RowDetail({ pair, onClose, decoded, learned, onJumpTo, contextWeight }: Props) {
  const { request, response } = pair;
  const url = request?.url ?? "—";
  const method = request?.method ?? "—";
  const status = response?.status ?? null;

  // Collapse state lives ON RowDetail (not Section) so the choice is
  // keyed by section name, not by the current event_id — that's
  // what makes the user's pick persist as they change selection.
  const [reqOpen, toggleReq] = usePersistedToggle(REQ_OPEN_KEY, true);
  const [resOpen, toggleRes] = usePersistedToggle(RES_OPEN_KEY, true);

  return (
    <aside className="row-detail">
      <header className="row-detail-head">
        <div className="row-detail-title">
          <span className="row-detail-method">{method}</span>
          <span className="row-detail-url" title={url}>
            {url}
          </span>
        </div>
        <div className="row-detail-meta">
          {status !== null && (
            <span className={`status-pill ${statusClass(status)}`}>
              {status}
            </span>
          )}
          <span className="row-detail-eid">{pair.event_id}</span>
          <button onClick={onClose} title="Close (Esc)">
            Close
          </button>
        </div>
      </header>

      {/* ADR 051: what noodle LEARNED from this round-trip —
          attribution + delta + evidence + lineage + pairing +
          context, shown above the raw traffic. */}
      <LearnedPanel learned={learned} onJumpTo={onJumpTo} />

      {/* S22 (refactor-overview §10): decoded-layer summary
          rendered above the legacy request/response sections.
          Only appears when the SSE feed has surfaced a record
          for this event_id — graceful degradation for rows that
          predate the typed feed. */}
      {decoded && (
        <section className="row-detail-decoded">
          <div className="row-detail-decoded-chips">
            {decoded.marks?.turn_id && (
              <TurnIdBadge turnId={decoded.marks.turn_id} />
            )}
            {decoded.usage && <UsagePanel usage={decoded.usage} mode="inline" />}
            {decoded.pairing && (
              <ToolPairingArrow pairing={decoded.pairing} onJump={onJumpTo} />
            )}
          </div>
          {decoded.content_blocks && decoded.content_blocks.length > 0 && (
            <div className="row-detail-decoded-blocks">
              <h4 className="body-label">content blocks</h4>
              <ContentBlockTags blocks={decoded.content_blocks} />
            </div>
          )}
          {decoded.usage && (decoded.usage.tokens || decoded.usage.latency) && (
            <div className="row-detail-decoded-usage stats-card">
              <h4 className="stats-card-title">Usage Statistics</h4>
              <UsagePanel usage={decoded.usage} mode="full" />
            </div>
          )}
          <EnvelopeInspector envelope={decoded.envelope} />
        </section>
      )}

      {/* ADR 056: per-round-trip context weight — what it costs to
          carry the fixed context (system + tools + history) vs the new
          prompt. Keyed by event_id, so it renders independent of the
          decoded feed. */}
      {contextWeight && <ContextWeightPanel weight={contextWeight} />}

      <Section
        title="Request"
        kind="request"
        exchange={request}
        open={reqOpen}
        onToggle={toggleReq}
      />
      <Section
        title="Response"
        kind="response"
        exchange={response}
        open={resOpen}
        onToggle={toggleRes}
      />
    </aside>
  );
}

function Section({
  title,
  kind,
  exchange,
  open,
  onToggle,
}: {
  title: string;
  kind: "request" | "response";
  exchange: Exchange | undefined;
  open: boolean;
  onToggle: () => void;
}) {
  return (
    <section className={`row-detail-section ${kind}${open ? " open" : ""}`}>
      <button
        type="button"
        className="row-detail-section-head"
        onClick={onToggle}
        aria-expanded={open}
      >
        <span className="row-detail-section-chev" aria-hidden="true">
          {open ? "▾" : "▸"}
        </span>
        <span className="row-detail-section-title">{title}</span>
      </button>
      {open && !exchange && (
        <div className="body-empty">(no event captured)</div>
      )}
      {open && exchange && (
        <>
          <HeadersTable headers={exchange.headers ?? {}} />
          {exchange.body_out !== undefined ? (
            <BeforeAfterPanel
              kind={kind}
              before={exchange.body}
              after={exchange.body_out}
            />
          ) : (
            <>
              <h4 className="body-label">Body</h4>
              <BodyView body={exchange.body} label={`${kind} body`} />
            </>
          )}
        </>
      )}
    </section>
  );
}

function HeadersTable({ headers }: { headers: Record<string, string[]> }) {
  const entries = Object.entries(headers);
  if (entries.length === 0) {
    return <div className="body-empty">(no headers)</div>;
  }
  return (
    <table className="headers-table">
      <tbody>
        {entries.map(([name, values]) => (
          <tr key={name}>
            <th>{name}</th>
            <td>{values.join(", ")}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function statusClass(s: number): string {
  if (s >= 500) return "err";
  if (s >= 400) return "warn";
  if (s >= 200) return "ok";
  return "pending";
}

// ADR 056: per-round-trip context weight. Token counts are vendor
// facts; *_bytes are request-side structural sizes. The overhead ratio
// (carried ÷ presented input) is derived here, never stored — it's the
// share of input you pay to carry context rather than prompt.
function ContextWeightPanel({ weight: w }: { weight: ContextWeight }) {
  const carried = w.cache_read_tokens + w.cache_creation_tokens;
  const presented = w.input_tokens + carried;
  const overheadPct = presented > 0 ? Math.round((carried / presented) * 100) : 0;
  const tok = (n: number) => `${n.toLocaleString()} tok`;
  const kb = (n: number) => `${(n / 1024).toFixed(1)} KB`;
  return (
    <section className="row-detail-context-weight stats-card">
      <h4 className="stats-card-title">Context Weight</h4>
      <table className="usage-table stats-table">
        <tbody>
          <tr className="stats-row-strong">
            <th>overhead ratio</th>
            <td>{overheadPct}% carried</td>
          </tr>
          <tr>
            <th>carried (cache read)</th>
            <td>{tok(w.cache_read_tokens)}</td>
          </tr>
          <tr>
            <th>created (cache write)</th>
            <td>{tok(w.cache_creation_tokens)}</td>
          </tr>
          <tr>
            <th>new input</th>
            <td>{tok(w.input_tokens)}</td>
          </tr>
          <tr>
            <th>output</th>
            <td>{tok(w.output_tokens)}</td>
          </tr>
          <tr>
            <th>system</th>
            <td>{kb(w.system_bytes)}</td>
          </tr>
          <tr>
            <th>tools</th>
            <td>
              {w.tools_count} · {kb(w.tools_bytes)}
            </td>
          </tr>
          <tr>
            <th>preamble</th>
            <td>{kb(w.preamble_bytes)}</td>
          </tr>
        </tbody>
      </table>
    </section>
  );
}


