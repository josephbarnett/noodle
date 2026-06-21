// Side-by-side REQUEST / RESPONSE header table for one round-trip,
// keyed off the request_id we already have on the ExchangePair.

import { useMemo } from "react";
import { Block } from "./Block";
import type { ExchangePair } from "../types";
import type { Usage } from "../store/derived/ooda";

interface Props {
  pair: ExchangePair | undefined;
  turnNum: number;
  rtIndex: number;
  rtTotal: number;
  ts: string;
  /** This round-trip's own token usage — shown per-RT, never summed
   *  across the turn (cached context is re-billed every round-trip, so
   *  a turn-level sum double-counts it; ADR 056). */
  usage?: Usage;
}

/** Compact per-round-trip token chip, e.g. `1.2k↑ 340↓ · cache 244k`.
 *  Returns null when no usage was captured. */
export function usageChip(usage: Usage | undefined): string | null {
  if (!usage) return null;
  const parts: string[] = [];
  if (usage.input_tokens != null) parts.push(`${fmtTokens(usage.input_tokens)}↑`);
  if (usage.output_tokens != null) parts.push(`${fmtTokens(usage.output_tokens)}↓`);
  const cache = usage.cache_read_input_tokens;
  if (cache != null && cache > 0) parts.push(`cache ${fmtTokens(cache)}`);
  return parts.length > 0 ? parts.join(" ") : null;
}

/** Short token count: `999`, `1.2k`, `244k`. */
function fmtTokens(n: number): string {
  if (n < 1000) return String(n);
  return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
}

export function HeadersBlock({ pair, turnNum, rtIndex, rtTotal, ts, usage }: Props) {
  const reqHeaders = useMemo(
    () => toEntries(pair?.request?.headers),
    [pair?.request?.headers],
  );
  const respHeaders = useMemo(
    () => toEntries(pair?.response?.headers),
    [pair?.response?.headers],
  );
  const total = reqHeaders.length + respHeaders.length;
  const base =
    rtTotal > 1
      ? `Turn ${turnNum} · roundtrip ${rtIndex}/${rtTotal}`
      : `Turn ${turnNum}`;
  const tokens = usageChip(usage);
  const summary = tokens ? `${base} · ${tokens}` : base;
  return (
    <Block
      role="headers"
      label="HEADERS"
      summary={summary}
      size={`${total} headers`}
      ts={ts}
      defaultOpen={false}
      body={
        <div className="headers-two-col">
          <HeaderColumn title="REQUEST" entries={reqHeaders} />
          <HeaderColumn title="RESPONSE" entries={respHeaders} />
        </div>
      }
    />
  );
}

function HeaderColumn({
  title,
  entries,
}: {
  title: string;
  entries: [string, string[]][];
}) {
  return (
    <div className="headers-col">
      <div className="headers-col-title">
        {title} <span className="headers-col-count">({entries.length})</span>
      </div>
      {entries.length === 0 ? (
        <div className="body-empty">(no headers)</div>
      ) : (
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
      )}
    </div>
  );
}

function toEntries(
  h: Record<string, string[]> | undefined,
): [string, string[]][] {
  if (!h) return [];
  return Object.entries(h).sort((a, b) => a[0].localeCompare(b[0]));
}
