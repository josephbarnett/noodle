// Anthropic streaming-SSE response reconstruction.
//
// Single responsibility: take the raw SSE stream (as a string) that
// noodle captures in the TAP entry's body field and reconstitute the
// equivalent of a non-streaming response's `content[]` + `stop_reason`
// + `usage` so the OODA derivation can treat both shapes uniformly.
//
// Anthropic event types we walk:
//   - message_start         → initial message envelope (model, role)
//   - content_block_start   → open a block at `index`
//   - content_block_delta   → append `text` (text_delta)
//                             or accumulate `partial_json` (input_json_delta)
//                             or accumulate `thinking` (thinking_delta)
//   - content_block_stop    → finalize the block at `index`
//                             (parse accumulated `partial_json` if present)
//   - message_delta         → final stop_reason + usage updates
//   - message_stop          → end
//
// Everything else is ignored — including `ping`.

import type { ContentBlock, Usage } from "./ooda";

export interface ParsedSse {
  contentBlocks: ContentBlock[];
  stopReason?: string;
  usage?: Usage;
  model?: string;
}

interface AnyObj {
  [k: string]: unknown;
}

/**
 * Heuristic: does this string look like an Anthropic SSE stream we
 * can parse? Caller should use this before paying the full split-and-
 * parse cost on bodies that may be plain text.
 */
export function looksLikeAnthropicSse(s: string): boolean {
  // Any `event: message_start` line in the first 2 KB is a strong tell.
  return /(^|\n)event:\s*message_start\b/.test(s.slice(0, 2048));
}

export function parseAnthropicSse(raw: string): ParsedSse {
  const blocks: ContentBlock[] = [];
  // Track partial state by `index` (Anthropic emits blocks in order
  // but the index is the source of truth across deltas).
  const partials: Map<
    number,
    {
      type: string;
      text?: string;
      thinking?: string;
      tool_use?: { id: string; name: string; rawInput: string };
      finalized?: ContentBlock;
    }
  > = new Map();

  let stopReason: string | undefined;
  let usage: Usage | undefined;
  let model: string | undefined;

  for (const ev of splitSseEvents(raw)) {
    const data = ev.data;
    if (!data) continue;
    let parsed: AnyObj;
    try {
      parsed = JSON.parse(data) as AnyObj;
    } catch {
      continue; // malformed event payload — skip
    }
    const t = typeof parsed.type === "string" ? parsed.type : "";

    switch (t) {
      case "message_start": {
        const m = parsed.message as AnyObj | undefined;
        if (m) {
          if (typeof m.model === "string") model = m.model;
          if (m.usage && typeof m.usage === "object") {
            usage = { ...(usage ?? {}), ...(m.usage as Usage) };
          }
          if (typeof m.stop_reason === "string") stopReason = m.stop_reason;
        }
        break;
      }
      case "content_block_start": {
        const index = numericIndex(parsed.index);
        if (index === null) break;
        const block = parsed.content_block as AnyObj | undefined;
        if (!block) break;
        const bt = typeof block.type === "string" ? block.type : "unknown";
        if (bt === "text") {
          partials.set(index, { type: "text", text: "" });
        } else if (bt === "thinking") {
          partials.set(index, { type: "thinking", thinking: "" });
        } else if (bt === "tool_use") {
          partials.set(index, {
            type: "tool_use",
            tool_use: {
              id: typeof block.id === "string" ? block.id : "",
              name: typeof block.name === "string" ? block.name : "",
              rawInput: "",
            },
          });
        } else {
          partials.set(index, { type: bt });
        }
        break;
      }
      case "content_block_delta": {
        const index = numericIndex(parsed.index);
        if (index === null) break;
        const delta = parsed.delta as AnyObj | undefined;
        if (!delta) break;
        const dt = typeof delta.type === "string" ? delta.type : "";
        const p = partials.get(index);
        if (!p) break;
        if (dt === "text_delta" && typeof delta.text === "string") {
          p.text = (p.text ?? "") + delta.text;
        } else if (dt === "thinking_delta" && typeof delta.thinking === "string") {
          p.thinking = (p.thinking ?? "") + delta.thinking;
        } else if (dt === "input_json_delta" && typeof delta.partial_json === "string") {
          if (p.tool_use) p.tool_use.rawInput += delta.partial_json;
        }
        break;
      }
      case "content_block_stop": {
        const index = numericIndex(parsed.index);
        if (index === null) break;
        const p = partials.get(index);
        if (!p) break;
        // Finalize.
        if (p.type === "text") {
          p.finalized = { type: "text", text: p.text ?? "" };
        } else if (p.type === "thinking") {
          p.finalized = { type: "thinking", thinking: p.thinking ?? "" };
        } else if (p.type === "tool_use" && p.tool_use) {
          let input: unknown = {};
          if (p.tool_use.rawInput) {
            try {
              input = JSON.parse(p.tool_use.rawInput);
            } catch {
              input = p.tool_use.rawInput;
            }
          }
          p.finalized = {
            type: "tool_use",
            id: p.tool_use.id,
            name: p.tool_use.name,
            input,
            result: null,
          };
        } else {
          p.finalized = { type: "unknown", raw: { type: p.type, index } };
        }
        break;
      }
      case "message_delta": {
        const delta = parsed.delta as AnyObj | undefined;
        if (delta && typeof delta.stop_reason === "string") {
          stopReason = delta.stop_reason;
        }
        const u = parsed.usage as AnyObj | undefined;
        if (u && typeof u === "object") {
          usage = { ...(usage ?? {}), ...(u as Usage) };
        }
        break;
      }
      case "message_stop":
      case "ping":
      default:
        break;
    }
  }

  // Emit finalized blocks in index order.
  const indexes = Array.from(partials.keys()).sort((a, b) => a - b);
  for (const i of indexes) {
    const p = partials.get(i);
    if (p?.finalized) blocks.push(p.finalized);
  }

  return { contentBlocks: blocks, stopReason, usage, model };
}

interface SseEvent {
  event?: string;
  data?: string;
}

function splitSseEvents(raw: string): SseEvent[] {
  // SSE event boundary is a blank line. We're forgiving about line
  // endings and trailing whitespace.
  const events: SseEvent[] = [];
  const chunks = raw.split(/\r?\n\r?\n/);
  for (const chunk of chunks) {
    if (!chunk.trim()) continue;
    let ev: SseEvent = {};
    let dataLines: string[] = [];
    for (const line of chunk.split(/\r?\n/)) {
      if (!line || line.startsWith(":")) continue; // comment
      const colon = line.indexOf(":");
      if (colon < 0) continue;
      const field = line.slice(0, colon).trim();
      // SSE spec: optional single space after colon.
      let value = line.slice(colon + 1);
      if (value.startsWith(" ")) value = value.slice(1);
      if (field === "event") ev.event = value;
      else if (field === "data") dataLines.push(value);
    }
    if (dataLines.length) ev.data = dataLines.join("\n");
    if (ev.data || ev.event) events.push(ev);
  }
  return events;
}

function numericIndex(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  return null;
}
