// Generic color-banded block component used by the OODA thread view.
// Single responsibility: render one role-tagged block with a header
// (role chip + summary + timestamp + size + collapse toggle) and
// optional body that expands on click.

import { useState, type ReactNode } from "react";

export type BlockRole = "user" | "user-loop" | "agent" | "thinking" | "tool" | "headers" | "system" | "unknown";

interface Props {
  role: BlockRole;
  label: string;
  /** Optional accessory rendered immediately after the label —
   *  used by `ToolBlock` for the bucket badge (built-in / MCP /
   *  skill). Kept as `ReactNode` so callers can ship styled pills
   *  without forcing the badge concept into every block role. */
  badge?: ReactNode;
  /** Short summary shown in the collapsed header. */
  summary?: string;
  /** Optional size badge (bytes, header count, etc.). */
  size?: string;
  ts?: string;
  /** Optional render-on-expand body. Omit for header-only blocks. */
  body?: ReactNode;
  /** Initial expand state. Loud blocks (user input, agent text) default
   *  open; noisy ones (system-reminders, thinking, headers) default
   *  collapsed. */
  defaultOpen?: boolean;
}

export function Block({
  role,
  label,
  badge,
  summary,
  size,
  ts,
  body,
  defaultOpen = true,
}: Props) {
  const collapsible = body !== undefined && body !== null;
  const [open, setOpen] = useState(defaultOpen);
  const headerCls = collapsible ? "block-head clickable" : "block-head";

  return (
    <div className={`block block-${role}${open ? " open" : ""}`}>
      <button
        type="button"
        className={headerCls}
        onClick={collapsible ? () => setOpen(!open) : undefined}
        disabled={!collapsible}
        aria-expanded={open}
      >
        {collapsible && <span className="block-chev">{open ? "▾" : "▸"}</span>}
        <span className={`block-role role-${role}`}>{label}</span>
        {badge}
        {summary && <span className="block-summary">{summary}</span>}
        {size && <span className="block-size">{size}</span>}
        {ts && <span className="block-ts">{formatTs(ts)}</span>}
      </button>
      {collapsible && open && <div className="block-body">{body}</div>}
    </div>
  );
}

function formatTs(ts: string): string {
  const m = ts.match(/T(\d{2}:\d{2}:\d{2}(?:\.\d+)?)/);
  return m?.[1]?.slice(0, 12) ?? ts;
}
