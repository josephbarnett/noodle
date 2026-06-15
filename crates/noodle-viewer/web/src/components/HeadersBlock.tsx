// Side-by-side REQUEST / RESPONSE header table for one round-trip,
// keyed off the request_id we already have on the ExchangePair.

import { useMemo } from "react";
import { Block } from "./Block";
import type { ExchangePair } from "../types";

interface Props {
  pair: ExchangePair | undefined;
  turnNum: number;
  rtIndex: number;
  rtTotal: number;
  ts: string;
}

export function HeadersBlock({ pair, turnNum, rtIndex, rtTotal, ts }: Props) {
  const reqHeaders = useMemo(
    () => toEntries(pair?.request?.headers),
    [pair?.request?.headers],
  );
  const respHeaders = useMemo(
    () => toEntries(pair?.response?.headers),
    [pair?.response?.headers],
  );
  const total = reqHeaders.length + respHeaders.length;
  const summary =
    rtTotal > 1
      ? `Turn ${turnNum} · roundtrip ${rtIndex}/${rtTotal}`
      : `Turn ${turnNum}`;
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
