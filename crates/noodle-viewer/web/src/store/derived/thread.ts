// Flatten an OodaSession into a linear conversational thread of
// role-tagged blocks, matching the TAP-viewer visual model.
//
// One Session → many Turns → many RoundTrips → many Blocks.
// The thread interleaves the blocks in chronological order with
// `turn-divider` and `headers` markers so the structure is legible
// without nesting.

import type { AgentRun, ContentBlock, OodaTurn, RoundTrip } from "./ooda";

export type ThreadItem =
  | { kind: "turn-divider"; turnNum: number; ts: string; roundtrips: number; turnId?: string }
  | {
      kind: "system";
      ts: string;
      /** Each text block in the request's top-level `system` array,
       *  rendered as its own purple block. Anthropic's API accepts
       *  either a bare string or an array of `{type:"text",text}`
       *  blocks; the viewer normalises both to this list. */
      blocks: ContentBlock[];
      /** True when noodle injected at least one of these blocks
       *  (i.e. the post-mutation system array is longer than the
       *  client-as-received one). Used to mark the injected block
       *  visually so the operator can audit attribution at a glance. */
      mutated: boolean;
    }
  | { kind: "user"; ts: string; blocks: ContentBlock[]; variant: "input" | "tool-loop" }
  | { kind: "headers"; ts: string; turnNum: number; rtIndex: number; rtTotal: number; requestId: string }
  | { kind: "thinking"; ts: string; text: string }
  | { kind: "agent-text"; ts: string; text: string }
  | { kind: "tool-use"; ts: string; toolUseId: string; name: string; input: unknown; result: ContentBlock | null; isError: boolean }
  | {
      kind: "tool-cluster";
      ts: string;
      /** The clustered tool-use items in arrival order. Always
       *  `length >= 2` — singletons aren't wrapped. */
      items: ToolUseItem[];
      /** Human-readable summary like `"Read ×2, Bash, Edit, +1 more"`.
       *  Pre-computed at derivation time so render stays cheap. */
      summary: string;
    }
  | { kind: "agent-unknown"; ts: string; raw: unknown }
  | { kind: "turn-end"; ts: string; stopReason?: string; turnNum: number };

/** Convenience alias — the tool-use ThreadItem variant. Exported
 *  because `tool-cluster.items` is a list of these and call sites
 *  need to type-narrow when rendering cluster bodies. */
export type ToolUseItem = Extract<ThreadItem, { kind: "tool-use" }>;

/** True for the `<system-reminder>`-style content blocks that
 *  swamp Claude Code requests — auto-collapsed in the UI. */
export function isLikelySystemReminder(block: ContentBlock): boolean {
  if (block.type !== "text") return false;
  return /^<system-reminder>/.test(block.text);
}

/**
 * Flatten one agent run's turns into the linear conversational thread.
 * The session-level view shows one agent run at a time; the rail
 * provides navigation across runs (and sub-agent links inside the
 * parent's Agent tool blocks navigate too).
 *
 * A post-pass groups runs of N≥2 consecutive `tool-use` items into a
 * single `tool-cluster` so tool-heavy turns don't drown the view.
 */
export function flattenAgentRun(run: AgentRun): ThreadItem[] {
  const items: ThreadItem[] = [];
  for (const turn of run.turns) {
    items.push({
      kind: "turn-divider",
      turnNum: turn.turnNum,
      ts: turn.startedAt,
      roundtrips: turn.roundtrips.length,
      turnId: turn.turnId,
    });
    turn.roundtrips.forEach((rt, idx) => {
      pushRoundTrip(items, turn, rt, idx);
    });
    const last = turn.roundtrips[turn.roundtrips.length - 1];
    items.push({
      kind: "turn-end",
      ts: last?.timestamp ?? turn.startedAt,
      stopReason: last?.stopReason,
      turnNum: turn.turnNum,
    });
  }
  return clusterConsecutiveTools(items);
}

/** Threshold above which consecutive tool-use items get bundled
 *  into a `tool-cluster`. Single calls render as standalone rows so
 *  one-off tools don't get an extra click. */
const CLUSTER_MIN = 2;

/** Walks `items` in order and replaces every contiguous run of
 *  `tool-use` items of length `>= CLUSTER_MIN` with one
 *  `tool-cluster` item. Any non-tool item (thinking, agent-text,
 *  headers, user, etc.) breaks the run — those interruptions are
 *  semantically meaningful (the agent reasoned between calls). */
export function clusterConsecutiveTools(items: ThreadItem[]): ThreadItem[] {
  const out: ThreadItem[] = [];
  let i = 0;
  while (i < items.length) {
    const it = items[i];
    if (it.kind !== "tool-use") {
      out.push(it);
      i++;
      continue;
    }
    // Scan forward to the end of the contiguous tool-use run.
    let j = i + 1;
    while (j < items.length && items[j].kind === "tool-use") j++;
    const run = items.slice(i, j) as ToolUseItem[];
    if (run.length >= CLUSTER_MIN) {
      out.push({
        kind: "tool-cluster",
        ts: run[0].ts,
        items: run,
        summary: clusterSummary(run),
      });
    } else {
      out.push(run[0]);
    }
    i = j;
  }
  return out;
}

/** Build the per-cluster summary: insertion-ordered, deduplicated
 *  list of tool names with a `×N` suffix when a name repeats, capped
 *  at SHOW distinct names then `+M more`. Pure function so render
 *  stays a stable string lookup. */
const SHOW_DISTINCT = 3;
export function clusterSummary(items: ToolUseItem[]): string {
  const order: string[] = [];
  const counts = new Map<string, number>();
  for (const it of items) {
    if (!counts.has(it.name)) order.push(it.name);
    counts.set(it.name, (counts.get(it.name) ?? 0) + 1);
  }
  const parts: string[] = [];
  for (let i = 0; i < Math.min(order.length, SHOW_DISTINCT); i++) {
    const n = order[i];
    const c = counts.get(n) ?? 1;
    parts.push(c > 1 ? `${n} ×${c}` : n);
  }
  if (order.length > SHOW_DISTINCT) {
    parts.push(`+${order.length - SHOW_DISTINCT} more`);
  }
  return parts.join(", ");
}

function pushRoundTrip(
  items: ThreadItem[],
  turn: OodaTurn,
  rt: RoundTrip,
  idx: number,
): void {
  // The user side of THIS roundtrip. Two cases:
  //   - Initial user input (idx 0): render its blocks (text, etc.)
  //   - Tool-loop continuation (idx ≥ 1): the user message is
  //     entirely tool_results that are already paired with their
  //     tool_use blocks in the prior round-trip's TOOL renderings.
  //     Rendering them again as standalone user blocks duplicates
  //     content and disconnects them visually from their tool_use.
  //     So we skip pure-tool_result user messages here; the result
  //     is shown inline in the TOOL block.
  // Render the request's top-level `system` field BEFORE the user
  // message. This is where noodle's attribution directive lands
  // (appended as a new text block on top of whatever Claude Code
  // sent us). The `mutated` flag is true when noodle modified the
  // array — the renderer marks the injected block visually.
  if (rt.systemBlocks.length > 0) {
    items.push({
      kind: "system",
      ts: rt.timestamp,
      blocks: rt.systemBlocks,
      mutated: rt.systemMutated,
    });
  }
  if (rt.userMessage.length > 0) {
    const allToolResults = rt.userMessage.every((b) => b.type === "tool_result");
    if (!(idx > 0 && allToolResults)) {
      items.push({
        kind: "user",
        ts: rt.timestamp,
        blocks: rt.userMessage,
        variant: idx === 0 ? "input" : "tool-loop",
      });
    }
  }
  items.push({
    kind: "headers",
    ts: rt.timestamp,
    turnNum: turn.turnNum,
    rtIndex: idx + 1,
    rtTotal: turn.roundtrips.length,
    requestId: rt.exchangeId,
  });
  for (const ab of rt.assistant) {
    switch (ab.type) {
      case "thinking":
        items.push({ kind: "thinking", ts: rt.timestamp, text: ab.thinking });
        break;
      case "text":
        items.push({ kind: "agent-text", ts: rt.timestamp, text: ab.text });
        break;
      case "tool_use":
        items.push({
          kind: "tool-use",
          ts: rt.timestamp,
          toolUseId: ab.id,
          name: ab.name,
          input: ab.input,
          result: ab.result ?? null,
          isError:
            ab.result?.type === "tool_result" ? ab.result.is_error === true : false,
        });
        break;
      case "tool_result":
        // Rare: assistant side carrying a tool_result. Keep it as a
        // user-tool-loop block so it remains visible.
        items.push({
          kind: "user",
          ts: rt.timestamp,
          blocks: [ab],
          variant: "tool-loop",
        });
        break;
      default:
        items.push({ kind: "agent-unknown", ts: rt.timestamp, raw: ab });
    }
  }
}
