// S22 (refactor-overview.md §10): token-usage + latency panel
// rendered alongside response rows. Reads `usage.tokens.*` and
// `usage.latency.*` from a [`DecodedExchange`] (on-disk wire
// shape, ADR 030 / S8).
//
// Inline summary by default (compact for rail/row use); the
// `mode="full"` variant renders an expandable detail block.

import type { DecodedUsage } from "../types";

interface Props {
  usage: DecodedUsage | null | undefined;
  /** `inline` — compact single-line chip (default). `full` —
   *  expanded labeled table (used in RowDetail). */
  mode?: "inline" | "full";
}

export function UsagePanel({ usage, mode = "inline" }: Props) {
  if (!usage || (!usage.tokens && !usage.latency)) return null;

  if (mode === "inline") {
    const parts: string[] = [];
    const t = usage.tokens;
    if (t) {
      parts.push(`${t.input_tokens}↑ ${t.output_tokens}↓`);
      if (t.cache_read_input_tokens) parts.push(`cache:${t.cache_read_input_tokens}`);
      if (t.reasoning_tokens) parts.push(`reason:${t.reasoning_tokens}`);
    }
    const l = usage.latency;
    if (l?.total_ms !== null && l?.total_ms !== undefined) {
      parts.push(`${formatMs(l.total_ms)}`);
    }
    if (parts.length === 0) return null;
    const title = describe(usage);
    return (
      <span className="usage-chip" title={title}>
        {parts.join(" · ")}
      </span>
    );
  }

  // Full mode — labeled table.
  return (
    <div className="usage-panel">
      {usage.tokens && (
        <table className="usage-table">
          <tbody>
            <Row label="input_tokens" value={usage.tokens.input_tokens} />
            <Row label="output_tokens" value={usage.tokens.output_tokens} />
            {usage.tokens.cache_read_input_tokens != null && (
              <Row
                label="cache_read_input_tokens"
                value={usage.tokens.cache_read_input_tokens}
              />
            )}
            {usage.tokens.cache_creation_input_tokens != null && (
              <Row
                label="cache_creation_input_tokens"
                value={usage.tokens.cache_creation_input_tokens}
              />
            )}
            {usage.tokens.reasoning_tokens != null && (
              <Row
                label="reasoning_tokens"
                value={usage.tokens.reasoning_tokens}
              />
            )}
          </tbody>
        </table>
      )}
      {usage.latency && (
        <table className="usage-table">
          <tbody>
            {usage.latency.time_to_first_byte_ms != null && (
              <Row
                label="time_to_first_byte_ms"
                value={usage.latency.time_to_first_byte_ms}
              />
            )}
            {usage.latency.total_ms != null && (
              <Row label="total_ms" value={usage.latency.total_ms} />
            )}
          </tbody>
        </table>
      )}
    </div>
  );
}

function Row({ label, value }: { label: string; value: number }) {
  return (
    <tr>
      <th>{label}</th>
      <td>{value.toLocaleString()}</td>
    </tr>
  );
}

function formatMs(n: number | null | undefined): string {
  if (n == null) return "";
  if (n < 1000) return `${n}ms`;
  return `${(n / 1000).toFixed(2)}s`;
}

function describe(u: DecodedUsage): string {
  const lines: string[] = [];
  if (u.tokens) {
    lines.push(`input_tokens=${u.tokens.input_tokens}`);
    lines.push(`output_tokens=${u.tokens.output_tokens}`);
    if (u.tokens.cache_read_input_tokens != null)
      lines.push(`cache_read=${u.tokens.cache_read_input_tokens}`);
  }
  if (u.latency?.time_to_first_byte_ms != null)
    lines.push(`ttfb=${u.latency.time_to_first_byte_ms}ms`);
  if (u.latency?.total_ms != null) lines.push(`total=${u.latency.total_ms}ms`);
  return lines.join("\n");
}
