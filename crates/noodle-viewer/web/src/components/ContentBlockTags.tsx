// S22 (refactor-overview.md §10): renders the typed
// `content_blocks[]` decoded from a `tap.jsonl` record. Each block
// gets a small kind tag (`text`, `thinking`, `tool_use`,
// `vendor_specific`). Click to reveal the block's specific detail
// (text body, tool name + args, etc.).
//
// `turn_start` / `turn_end` are surfaced as turn-boundary chips
// rather than expandable blocks — they don't carry user-visible
// content.

import { useState } from "react";
import type { DecodedContentBlock } from "../types";

interface Props {
  blocks: DecodedContentBlock[] | null | undefined;
}

export function ContentBlockTags({ blocks }: Props) {
  if (!blocks || blocks.length === 0) return null;
  return (
    <div className="content-block-tags">
      {blocks.map((b, i) => (
        <BlockTag key={i} block={b} />
      ))}
    </div>
  );
}

function BlockTag({ block }: { block: DecodedContentBlock }) {
  const [open, setOpen] = useState(false);

  const label = labelFor(block);
  const className = `block-tag block-tag-${classFor(block)}${open ? " open" : ""}`;
  const expandable = isExpandable(block);

  return (
    <div className={className}>
      <button
        type="button"
        className="block-tag-head"
        onClick={() => expandable && setOpen((v) => !v)}
        title={describe(block)}
        aria-expanded={open}
        disabled={!expandable}
      >
        <span className="block-tag-kind">{label}</span>
        {summaryFor(block) && <span className="block-tag-summary">{summaryFor(block)}</span>}
        {expandable && (
          <span className="block-tag-chev" aria-hidden="true">
            {open ? "▾" : "▸"}
          </span>
        )}
      </button>
      {open && expandable && (
        <div className="block-tag-detail">{detailFor(block)}</div>
      )}
    </div>
  );
}

function labelFor(b: DecodedContentBlock): string {
  switch (b.kind) {
    case "turn_start":
      return "turn start";
    case "turn_end":
      return "turn end";
    case "content":
      return b.category === "reasoning" ? "thinking" : "text";
    case "tool_use":
      return "tool_use";
    case "vendor_specific":
      return `vendor:${b.vendor_kind}`;
  }
}

function classFor(b: DecodedContentBlock): string {
  switch (b.kind) {
    case "turn_start":
    case "turn_end":
      return "turn";
    case "content":
      return b.category === "reasoning" ? "thinking" : "text";
    case "tool_use":
      return "tool";
    case "vendor_specific":
      return "vendor";
  }
}

function summaryFor(b: DecodedContentBlock): string | null {
  switch (b.kind) {
    case "content":
      return b.text.slice(0, 60).replace(/\s+/g, " ");
    case "tool_use":
      return b.tool_name;
    case "turn_end":
      return b.status != null ? `${b.status}` : null;
    default:
      return null;
  }
}

function isExpandable(b: DecodedContentBlock): boolean {
  return (
    b.kind === "content" || b.kind === "tool_use" || b.kind === "vendor_specific"
  );
}

function detailFor(b: DecodedContentBlock): React.ReactNode {
  switch (b.kind) {
    case "content":
      return (
        <pre className="block-tag-text">
          {b.text}
          {b.thinking_signature && (
            <span className="block-tag-note">
              {"\n— signature: " + b.thinking_signature}
            </span>
          )}
        </pre>
      );
    case "tool_use":
      return (
        <div className="block-tag-tool">
          <div className="block-tag-tool-id">
            id: <span className="mono">{b.tool_use_id}</span>
          </div>
          <pre className="block-tag-text">{JSON.stringify(b.input, null, 2)}</pre>
        </div>
      );
    case "vendor_specific":
      return (
        <pre className="block-tag-text">{JSON.stringify(b.payload, null, 2)}</pre>
      );
    default:
      return null;
  }
}

function describe(b: DecodedContentBlock): string {
  switch (b.kind) {
    case "turn_start":
      return `turn_start · request_id=${b.request_id} · method=${b.method ?? "—"}`;
    case "turn_end":
      return `turn_end · request_id=${b.request_id} · status=${b.status ?? "—"}`;
    case "content":
      return `content[${b.block_index}] · category=${b.category}`;
    case "tool_use":
      return `tool_use[${b.block_index}] · ${b.tool_name} · ${b.tool_use_id}`;
    case "vendor_specific":
      return `vendor_specific · ${b.vendor_kind}`;
  }
}
