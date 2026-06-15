// Tree rendering from the proxy marks — render, never re-derive.
//
// ADR 052 §6 (FR6, invariant 6): the proxy computes the turn/run/
// lineage tree on the wire and stamps authoritative §5 marks onto
// every `/v1/messages` round-trip. The viewer **renders** those
// marks; it does not re-derive the tree from `systemHash` /
// `stop_reason` / stack-matching. That client-side re-derivation was
// a second, disagreeing source of truth (ADR 052 §2) and is gone.
//
// `buildSessions` takes a `getMarks(exchangeId)` accessor returning
// the §5 `DecodedMarks` for a round-trip:
//
//   marks { session_id, role, frame_id, parent_frame_id, depth, turn_id }
//
// The tree is read straight off those fields:
//
//   - `session_id`      groups round-trips into one stack container.
//   - `role: "main"`    is the ROOT frame (`frame_id == "ROOT"`).
//   - `role: "sub_agent"` is a child frame; `frame_id` is the
//     spawning `tool_use.id`, `parent_frame_id` the frame that
//     spawned it, `depth` its nesting level.
//   - `role: "side_call"` is OFF-TREE — quota / title-gen / security-
//     monitor / suggestion / compactor. No turn, no place in the
//     tree; surfaced in the separate auxiliary lane.
//   - `turn_id`         groups a frame's round-trips into turns. One
//     `turn_id` spans the whole recursion of one user prompt.
//
// Hierarchy (see docs/adrs/052-turn-run-lineage-frame-tree.md):
//
//   Session → Frame (AgentRun) → Turn → RoundTrip
//
// Legacy fallback: captures predating the §5 marks (or cells with no
// marking detector) carry no marks. For those, the old heuristic —
// one main run, `stop_reason` turn folding — still produces a usable
// thread so historical `tap.jsonl` files keep rendering.

import type { DecodedMarks, ExchangePair } from "../../types";
import { effectiveBody } from "../../lib/effectiveBody";
import { looksLikeAnthropicSse, parseAnthropicSse } from "./anthropic_sse";

export interface OodaSession {
  /** Stable id. `marks.session_id` when known; synthesized otherwise. */
  id: string;
  /** What we'll show on the rail — friendly summary. */
  label: string;
  model?: string;
  /** RFC3339 of the most recent round-trip's request (any kind). */
  lastActivity: string;
  /** Agent runs (frames), in chronological first-seen order. The
   *  main agent is the ROOT frame; sub-agent frames link to their
   *  parent via `parentRunIndex`. See ADR 052 §3. */
  agentRuns: AgentRun[];
  /** Side-calls (role `side_call`) — off-tree harness calls (quota,
   *  title-gen, security-monitor, suggestion, compactor). Not part of
   *  any turn. ADR 052 §3/FR4. */
  auxiliary: AuxCall[];
}

export interface AgentRun {
  /** 1-based index within the session, in chronological order. */
  index: number;
  /** The frame's identity — the spawning `tool_use.id`; `"ROOT"` for
   *  the main agent. ADR 052 §5 `frame_id`. */
  frameId: string;
  /** RFC3339 of this run's first round-trip's request. */
  startedAt: string;
  /** Short preview of the system prompt for display (first ~240 chars
   *  of joined text content). Display-only; never an identity. */
  systemPreview: string;
  /** True for the ROOT frame (the main agent), i.e. `role == "main"`. */
  isMain: boolean;
  /** Sub-agent nesting level from `marks.depth` (0 = main). */
  depth: number;
  /** If this is a sub-agent frame, the spawn metadata captured from
   *  the parent's `tool_use(Task|Agent)` block. Null for the main
   *  agent. `toolUseId` equals this frame's `frameId`. */
  spawnedBy: AgentSpawn | null;
  /** Index of the run that spawned this one (1-based, matches another
   *  run's `.index`). `null` for the ROOT frame. Resolved from
   *  `marks.parent_frame_id`. Drives the sidebar tree-nest depth. */
  parentRunIndex: number | null;
  /** Turns within this frame, grouped by `marks.turn_id`. */
  turns: OodaTurn[];
}

export interface AgentSpawn {
  /** The parent's tool_use id that spawned this run (= this frame's id). */
  toolUseId: string;
  /** Anthropic Agent tool input.subagent_type (e.g. "code-reviewer"). */
  subagentType?: string;
  /** Anthropic Agent tool input.description (short title). */
  description?: string;
  /** Whether the parent invoked `run_in_background: true`. */
  runInBackground: boolean;
}

export interface OodaTurn {
  turnNum: number;
  /** RFC3339 of the first round-trip's request. */
  startedAt: string;
  /** `marks.turn_id` from the proxy (ADR 052 §5). Present whenever
   *  the marks-driven path opened the turn; `undefined` for
   *  heuristic-derived turns on cells without a marking detector. */
  turnId?: string;
  /** The genuinely new user input that opens this turn (not a tool_result). */
  userInput: ContentBlock[];
  /** One or more round-trips; later ones are tool-use loop continuations. */
  roundtrips: RoundTrip[];
}

export interface AuxCall {
  kind: "quota" | "title" | "other";
  exchangeId: string;
  timestamp: string;
  /** The latest user message in the request (often a short probe). */
  userMessage: ContentBlock[];
  /** Assistant content (for title-gen this is the JSON it returned). */
  assistant: ContentBlock[];
  stopReason?: string;
  usage?: Usage;
  /** Free-form summary for the rail/section header. */
  summary?: string;
}

export interface RoundTrip {
  exchangeId: string;
  timestamp: string;
  userMessage: ContentBlock[];
  /** The request's top-level `system` field as a list of text
   *  blocks. Anthropic's API accepts either a bare string or an
   *  array of `{type:"text",text}` blocks; the builder normalises
   *  both to this shape. Empty when the request had no system. */
  systemBlocks: ContentBlock[];
  /** True when noodle injected (or otherwise modified) the system
   *  array on this round-trip. Detected by comparing the
   *  pre-mutation (`body`) and post-mutation (`body_out`) system
   *  arrays. Used to render the injected block with a distinct
   *  visual marker in the OODA view. */
  systemMutated: boolean;
  assistant: ContentBlock[];
  /** Anthropic wire `stop_reason` for this response (`end_turn`,
   *  `tool_use`, `max_tokens`, …). Display-only — the turn boundary
   *  comes from `marks.turn_id`, not this field (ADR 052 §6). */
  stopReason?: string;
  usage?: Usage;
  model?: string;
  /** True when the assistant emitted a `tool_use` (`stop_reason:
   *  "tool_use"`) — i.e. the loop continues. Display hint only. */
  continuesLoop: boolean;
}

export interface Usage {
  input_tokens?: number;
  output_tokens?: number;
  cache_creation_input_tokens?: number;
  cache_read_input_tokens?: number;
}

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; thinking: string }
  | {
      type: "tool_use";
      id: string;
      name: string;
      input: unknown;
      result?: ToolResultBlock | null;
    }
  | ToolResultBlock
  | { type: "unknown"; raw: unknown };

export interface ToolResultBlock {
  type: "tool_result";
  tool_use_id: string;
  content: string | ContentBlock[];
  is_error?: boolean;
}

/**
 * Optional memoization surface for `parseResponse`. Lets the caller
 * (the React store) carry the parsed SSE / JSON view across multiple
 * `buildSessions` passes so each response body is parsed at most once
 * per capture, not once per WS ingest tick.
 *
 * The cache must be invalidated when the underlying response body
 * could change. Today response bodies are written once and never
 * mutated, so a simple `Map<event_id, ParsedResponse>` suffices.
 *
 * Optional by design: omitting it keeps `buildSessions` pure for
 * tests and for any caller that doesn't want store-side state.
 */
export interface ParseCache {
  get(eventId: string): ParsedResponse | undefined;
  set(eventId: string, parsed: ParsedResponse): void;
}

/** Accessor for one round-trip's §5 marks. Returns `null`/`undefined`
 *  for round-trips the proxy hasn't stamped (legacy captures). */
export type GetMarks = (
  exchangeId: string,
) => DecodedMarks | null | undefined;

/**
 * Top-level entry point. Filters chat traffic, groups by
 * `marks.session_id`, and **renders** the §5 frame tree:
 *   - `role: "side_call"` round-trips go to the auxiliary lane;
 *   - the rest become frames (`AgentRun`s) keyed by `frame_id`,
 *     nested via `parent_frame_id`, and grouped into turns by
 *     `turn_id`.
 * No client-side turn/run derivation — the marks are authoritative
 * (ADR 052 §6). Captures with no marks fall back to the legacy
 * single-run, `stop_reason`-folded heuristic.
 *
 * `parseCache` is optional. When provided, response bodies are
 * parsed at most once per `event_id` for the lifetime of the cache.
 */
export function buildSessions(
  pairs: ExchangePair[],
  pairsById: Map<string, ExchangePair>,
  parseCache?: ParseCache,
  getMarks?: GetMarks,
): OodaSession[] {
  const chatPairs = pairs.filter(isAnthropicChat);

  // Group by session — `marks.session_id` when stamped, else the
  // wire `session_hash` / host fallback so legacy captures still
  // partition sensibly.
  const groups = new Map<string, ExchangePair[]>();
  for (const p of chatPairs) {
    const id = getMarks?.(p.event_id)?.session_id ?? sessionIdFor(p);
    const list = groups.get(id);
    if (list) list.push(p);
    else groups.set(id, [p]);
  }

  const sessions: OodaSession[] = [];
  for (const [id, ps] of groups) {
    ps.sort((a, b) => timestampOf(a).localeCompare(timestampOf(b)));

    const session = getMarks
      ? buildSessionFromMarks(id, ps, getMarks, parseCache, pairsById)
      : buildSessionLegacy(id, ps, parseCache, pairsById);
    sessions.push(session);
  }

  return sessions;
}

// ── Marks-driven rendering (ADR 052 §3/§5/§6) ─────────────────────

/**
 * Render one session's frame tree from the §5 marks.
 *
 * Each non-`side_call` round-trip is routed to its frame by
 * `frame_id`; frames are created in first-seen order, parented by
 * `parent_frame_id`, and their round-trips grouped into turns by
 * `turn_id`. `side_call` round-trips become auxiliary entries.
 *
 * A round-trip with no marks at all falls back to ROOT (it keeps
 * historical / partially-marked captures legible rather than dropping
 * the row to the aux lane).
 */
function buildSessionFromMarks(
  id: string,
  ps: ExchangePair[],
  getMarks: GetMarks,
  parseCache: ParseCache | undefined,
  _pairsById: Map<string, ExchangePair>,
): OodaSession {
  interface FrameAcc {
    frameId: string;
    parentFrameId: string | null;
    depth: number;
    isMain: boolean;
    firstTs: string;
    systemPreview: string;
    rts: { rt: RoundTrip; turnId: string | null }[];
  }
  const frames = new Map<string, FrameAcc>();
  const frameOrder: string[] = [];
  const aux: AuxCall[] = [];
  const mainRTs: RoundTrip[] = [];

  for (const p of ps) {
    const parsed = parseResponseCached(p, parseCache);
    const marks = getMarks(p.event_id) ?? null;

    if (marks?.role === "side_call") {
      // The marks already say this is a side-call; classify only
      // picks a friendly label, so "main" maps to the generic bucket.
      const kind = classifySideCall(p, parsed.assistant);
      aux.push(buildAuxCall(p, kind === "main" ? "other" : kind, parsed));
      continue;
    }

    const rt = buildRoundTrip(p, parsed);
    mainRTs.push(rt);

    // Default to ROOT when marks are absent or carry no frame_id.
    const frameId = marks?.frame_id ?? "ROOT";
    const parentFrameId = marks?.parent_frame_id ?? null;
    const depth = marks?.depth ?? 0;
    const isMain = marks?.role === "main" || frameId === "ROOT";

    let frame = frames.get(frameId);
    if (!frame) {
      frame = {
        frameId,
        parentFrameId,
        depth,
        isMain,
        firstTs: rt.timestamp,
        systemPreview: systemPromptCanonical(p)?.slice(0, 240) ?? "",
        rts: [],
      };
      frames.set(frameId, frame);
      frameOrder.push(frameId);
    }
    frame.rts.push({ rt, turnId: marks?.turn_id ?? null });
  }

  // Frame id → 1-based run index, in first-seen order.
  const indexOf = new Map<string, number>();
  frameOrder.forEach((fid, i) => indexOf.set(fid, i + 1));

  const runs: AgentRun[] = frameOrder.map((fid) => {
    const f = frames.get(fid)!;
    const turns = groupRtsByTurnId(f.rts);
    return {
      index: indexOf.get(fid)!,
      frameId: fid,
      startedAt: f.firstTs,
      systemPreview: f.systemPreview,
      isMain: f.isMain,
      depth: f.depth,
      spawnedBy: f.isMain
        ? null
        : { toolUseId: fid, runInBackground: false },
      parentRunIndex:
        f.parentFrameId != null ? indexOf.get(f.parentFrameId) ?? null : null,
      turns,
    };
  });

  // Enrich each sub-agent frame's spawn metadata from the parent's
  // `tool_use(Task|Agent)` block carrying its id (= the child frame_id).
  attachSpawnMetadata(runs, mainRTs);
  for (const run of runs) pairToolsAcrossTurn(run.turns);

  return {
    id,
    label: shortLabel(id, ps.length),
    model: firstDefined(mainRTs.map((rt) => rt.model)),
    lastActivity: ps[ps.length - 1] ? timestampOf(ps[ps.length - 1]) : "",
    agentRuns: runs,
    auxiliary: aux,
  };
}

/** Group one frame's round-trips into turns by `marks.turn_id`. A
 *  contiguous block of round-trips sharing a `turn_id` is one turn; a
 *  changed (or absent) id starts a new turn. */
function groupRtsByTurnId(
  rts: { rt: RoundTrip; turnId: string | null }[],
): OodaTurn[] {
  const turns: OodaTurn[] = [];
  let current: OodaTurn | null = null;
  let currentKey: string | null = null;
  for (const { rt, turnId } of rts) {
    if (current && turnId !== null && currentKey === turnId) {
      current.roundtrips.push(rt);
      continue;
    }
    current = {
      turnNum: turns.length + 1,
      startedAt: rt.timestamp,
      turnId: turnId ?? undefined,
      userInput: rt.userMessage,
      roundtrips: [rt],
    };
    currentKey = turnId;
    turns.push(current);
  }
  return turns;
}

/**
 * Walk every main-thread round-trip for `tool_use(Task|Agent)` blocks
 * and, when a block's id matches a sub-agent frame's id, copy the
 * spawn metadata (subagent_type / description / run_in_background)
 * onto that frame. The frame tree itself comes from the marks; this
 * only enriches the display label.
 */
function attachSpawnMetadata(runs: AgentRun[], allRTs: RoundTrip[]): void {
  const byFrameId = new Map<string, AgentRun>();
  for (const r of runs) if (!r.isMain) byFrameId.set(r.frameId, r);
  if (byFrameId.size === 0) return;
  for (const rt of allRTs) {
    for (const block of rt.assistant) {
      if (block.type !== "tool_use") continue;
      if (block.name !== "Agent" && block.name !== "Task") continue;
      const run = byFrameId.get(block.id);
      if (!run) continue;
      const input = (block.input ?? {}) as Record<string, unknown>;
      run.spawnedBy = {
        toolUseId: block.id,
        subagentType:
          typeof input.subagent_type === "string"
            ? input.subagent_type
            : undefined,
        description:
          typeof input.description === "string" ? input.description : undefined,
        runInBackground: input.run_in_background === true,
      };
    }
  }
}

// ── Legacy fallback (no marks) ────────────────────────────────────

/**
 * Render a session with no §5 marks: one main run, round-trips folded
 * into turns by the `stop_reason` heuristic. Kept so captures
 * predating the marking detector (or cells without one) still render
 * a usable thread. New, marked captures never reach this path.
 */
function buildSessionLegacy(
  id: string,
  ps: ExchangePair[],
  parseCache: ParseCache | undefined,
  _pairsById: Map<string, ExchangePair>,
): OodaSession {
  const mainRTs: RoundTrip[] = [];
  const aux: AuxCall[] = [];
  for (const p of ps) {
    const parsed = parseResponseCached(p, parseCache);
    const kind = classifySideCall(p, parsed.assistant);
    if (kind === "main") mainRTs.push(buildRoundTrip(p, parsed));
    else aux.push(buildAuxCall(p, kind, parsed));
  }

  const turns = foldIntoTurnsByStopReason(mainRTs);
  const run: AgentRun | null =
    mainRTs.length > 0
      ? {
          index: 1,
          frameId: "ROOT",
          startedAt: mainRTs[0].timestamp,
          systemPreview: "",
          isMain: true,
          depth: 0,
          spawnedBy: null,
          parentRunIndex: null,
          turns,
        }
      : null;
  if (run) pairToolsAcrossTurn(run.turns);

  return {
    id,
    label: shortLabel(id, ps.length),
    model: firstDefined(mainRTs.map((rt) => rt.model)),
    lastActivity: ps[ps.length - 1] ? timestampOf(ps[ps.length - 1]) : "",
    agentRuns: run ? [run] : [],
    auxiliary: aux,
  };
}

/**
 * Legacy turn folding: a round-trip continues the current turn iff
 * the previous round-trip's `stop_reason` was `tool_use`. Any other
 * stop_reason (or no predecessor) starts a new turn. Used only for
 * unmarked captures (ADR 052 §6 makes the proxy authoritative for
 * marked traffic).
 */
function foldIntoTurnsByStopReason(rts: RoundTrip[]): OodaTurn[] {
  const turns: OodaTurn[] = [];
  let current: OodaTurn | null = null;
  for (const rt of rts) {
    const prev = current?.roundtrips[current.roundtrips.length - 1];
    if (prev?.stopReason === "tool_use" && current) {
      current.roundtrips.push(rt);
    } else {
      current = {
        turnNum: turns.length + 1,
        startedAt: rt.timestamp,
        userInput: rt.userMessage,
        roundtrips: [rt],
      };
      turns.push(current);
    }
  }
  return turns;
}

/**
 * Canonicalize an Anthropic-shape `system` field for a display
 * preview. Returns the concatenated text of all `{type:"text",text}`
 * blocks, stripping the per-request `x-anthropic-billing-header`
 * block which varies with the prompt cache hash.
 */
function systemPromptCanonical(pair: ExchangePair): string | null {
  const body = effectiveBody(pair.request);
  if (!body || typeof body !== "object") return null;
  const sys = (body as Record<string, unknown>).system;
  if (typeof sys === "string") return sys;
  if (Array.isArray(sys)) {
    const parts: string[] = [];
    for (const block of sys) {
      if (!block || typeof block !== "object") continue;
      const b = block as Record<string, unknown>;
      if (typeof b.text === "string") {
        if (b.text.startsWith("x-anthropic-billing-header")) continue;
        parts.push(b.text);
      }
    }
    return parts.join("\n");
  }
  return null;
}

// ── Side-call classification (display bucket only) ────────────────

/**
 * Bucket a side-call (or, in the legacy path, decide main-vs-aux) for
 * display. The §5 marks already say a round-trip *is* a side-call
 * (`role: "side_call"`); this only picks a friendly label.
 *
 * Quota probe: `max_tokens <= 1`. Title generation: a single JSON
 * text block with a `title` field. Everything else: `other`.
 */
function classifySideCall(
  p: ExchangePair,
  assistant: ContentBlock[],
): "main" | "quota" | "title" | "other" {
  const reqBody = effectiveBody(p.request);
  if (reqBody && typeof reqBody === "object") {
    const r = reqBody as Record<string, unknown>;
    if (typeof r.max_tokens === "number" && r.max_tokens <= 1) return "quota";
  }
  if (looksLikeTitleGen(assistant)) return "title";
  return "main";
}

function looksLikeTitleGen(blocks: ContentBlock[]): boolean {
  if (blocks.length !== 1) return false;
  const b = blocks[0];
  if (b.type !== "text") return false;
  const trimmed = b.text.trim();
  if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) return false;
  try {
    const v = JSON.parse(trimmed) as Record<string, unknown>;
    return typeof v === "object" && v !== null && "title" in v;
  } catch {
    return false;
  }
}

// ── Construction ─────────────────────────────────────────────

function isAnthropicChat(p: ExchangePair): boolean {
  const url = p.request?.url ?? "";
  return url.includes("/v1/messages");
}

function sessionIdFor(p: ExchangePair): string {
  const hash = p.request?.session_hash ?? p.response?.session_hash;
  if (hash) return hash;
  const host = hostFrom(p) ?? "anon";
  return `anon-${host}`;
}

function hostFrom(p: ExchangePair): string | null {
  const url = p.request?.url ?? "";
  try {
    return url ? new URL(url).host : null;
  } catch {
    return null;
  }
}

function timestampOf(p: ExchangePair): string {
  return p.request?.timestamp ?? p.response?.timestamp ?? "";
}

function buildRoundTrip(p: ExchangePair, parsed: ParsedResponse): RoundTrip {
  const reqBody = ((effectiveBody(p.request) ?? {})) as Record<string, unknown>;
  const messages = Array.isArray(reqBody.messages) ? reqBody.messages : [];
  const userMessage = extractLatestUserMessage(messages);
  const reqModel =
    typeof reqBody.model === "string" ? reqBody.model : undefined;
  const systemBlocks = extractSystemBlocks(reqBody.system);
  // Mutation detection: compare the system arrays from the
  // pre-mutation (request.body) and post-mutation (request.body_out)
  // request bodies. When they differ in length or content, noodle
  // injected (or otherwise modified) the system field for this
  // round-trip.
  const preMutationSystem = extractSystemBlocks(
    ((p.request?.body ?? {}) as Record<string, unknown>).system,
  );
  const systemMutated =
    preMutationSystem.length !== systemBlocks.length ||
    preMutationSystem.some(
      (b, i) =>
        b.type !== systemBlocks[i]?.type ||
        (b.type === "text" &&
          systemBlocks[i]?.type === "text" &&
          b.text !==
            (systemBlocks[i] as Extract<ContentBlock, { type: "text" }>).text),
    );
  return {
    exchangeId: p.event_id,
    timestamp: p.request?.timestamp ?? p.response?.timestamp ?? "",
    userMessage,
    systemBlocks,
    systemMutated,
    assistant: parsed.assistant,
    stopReason: parsed.stopReason,
    usage: parsed.usage,
    model: reqModel ?? parsed.model,
    continuesLoop: parsed.stopReason === "tool_use",
  };
}

/** Normalise Anthropic's `system` wire shape (string OR array of
 *  blocks) into a list of `ContentBlock`s. Empty list for
 *  unknown shapes. */
function extractSystemBlocks(value: unknown): ContentBlock[] {
  if (typeof value === "string") {
    return value.length > 0 ? [{ type: "text", text: value }] : [];
  }
  if (Array.isArray(value)) {
    return value
      .filter(
        (b): b is { type: string; text: string } =>
          typeof b === "object" &&
          b !== null &&
          (b as { type?: unknown }).type === "text" &&
          typeof (b as { text?: unknown }).text === "string",
      )
      .map((b) => ({ type: "text" as const, text: b.text }));
  }
  return [];
}

function buildAuxCall(
  p: ExchangePair,
  kind: "quota" | "title" | "other",
  parsed: ParsedResponse,
): AuxCall {
  const reqBody = ((effectiveBody(p.request) ?? {})) as Record<string, unknown>;
  const messages = Array.isArray(reqBody.messages) ? reqBody.messages : [];
  const userMessage = extractLatestUserMessage(messages);
  return {
    kind,
    exchangeId: p.event_id,
    timestamp: p.request?.timestamp ?? p.response?.timestamp ?? "",
    userMessage,
    assistant: parsed.assistant,
    stopReason: parsed.stopReason,
    usage: parsed.usage,
    summary: summarizeAux(kind, parsed.assistant, userMessage),
  };
}

function summarizeAux(
  kind: "quota" | "title" | "other",
  assistant: ContentBlock[],
  userMessage: ContentBlock[],
): string {
  if (kind === "quota") return "quota probe";
  if (kind === "title") {
    const first = assistant[0];
    if (first && first.type === "text") {
      try {
        const v = JSON.parse(first.text) as Record<string, unknown>;
        if (typeof v.title === "string") return `title: "${v.title}"`;
      } catch {
        /* fall through */
      }
    }
    return "title generation";
  }
  const first = userMessage[0];
  if (first && first.type === "text") return first.text.slice(0, 80);
  return "side-call";
}

export interface ParsedResponse {
  assistant: ContentBlock[];
  stopReason?: string;
  usage?: Usage;
  model?: string;
}

/**
 * Cache-aware wrapper around `parseResponse`. On cache miss, parses
 * via the underlying pure function and writes the result back to the
 * cache so the next pass over the same `event_id` is a Map lookup.
 */
function parseResponseCached(
  p: ExchangePair,
  cache: ParseCache | undefined,
): ParsedResponse {
  if (!cache) return parseResponse(p);
  // CRITICAL: do not cache when the response Exchange hasn't arrived
  // yet. The slim WS feed delivers request + response as separate
  // messages; when the request lands first, `p.response` is
  // `undefined` and `parseResponse` returns `{assistant: []}`.
  // Caching that empty parse against `event_id` pins it — when the
  // response arrives a moment later and OODA re-derives, the cache
  // returns the stale empty parse and the AGENT content block never
  // renders. Refresh recovers because the history replay delivers
  // both directions in quick succession, increasing the odds that
  // the first parse runs after both are present.
  if (!p.response) return parseResponse(p);
  const hit = cache.get(p.event_id);
  if (hit) return hit;
  const parsed = parseResponse(p);
  cache.set(p.event_id, parsed);
  return parsed;
}

function parseResponse(p: ExchangePair): ParsedResponse {
  const body = effectiveBody(p.response);
  if (typeof body === "string" && looksLikeAnthropicSse(body)) {
    const parsed = parseAnthropicSse(body);
    return {
      assistant: parsed.contentBlocks,
      stopReason: parsed.stopReason,
      usage: parsed.usage,
      model: parsed.model,
    };
  }
  if (body && typeof body === "object") {
    const r = body as Record<string, unknown>;
    return {
      assistant: Array.isArray(r.content) ? r.content.map(toBlock) : [],
      stopReason: typeof r.stop_reason === "string" ? r.stop_reason : undefined,
      usage: r.usage && typeof r.usage === "object" ? (r.usage as Usage) : undefined,
      model: typeof r.model === "string" ? r.model : undefined,
    };
  }
  return { assistant: [] };
}

/** Pair tool_use with tool_result across the round-trips of one turn. */
function pairToolsAcrossTurn(turns: OodaTurn[]): void {
  for (const turn of turns) {
    for (let i = 0; i < turn.roundtrips.length - 1; i++) {
      const cur = turn.roundtrips[i];
      const next = turn.roundtrips[i + 1];
      if (!cur || !next) continue;
      const results = new Map<string, ToolResultBlock>();
      for (const b of next.userMessage) {
        if (b.type === "tool_result") results.set(b.tool_use_id, b);
      }
      for (const b of cur.assistant) {
        if (b.type === "tool_use") {
          const r = results.get(b.id);
          if (r) b.result = r;
        }
      }
    }
  }
}

function extractLatestUserMessage(messages: unknown[]): ContentBlock[] {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i] as Record<string, unknown> | null;
    if (!m || m.role !== "user") continue;
    return normalizeContent(m.content);
  }
  return [];
}

function normalizeContent(content: unknown): ContentBlock[] {
  if (typeof content === "string") {
    return [{ type: "text", text: content }];
  }
  if (Array.isArray(content)) {
    return content.map(toBlock);
  }
  return [];
}

function toBlock(raw: unknown): ContentBlock {
  if (!raw || typeof raw !== "object") {
    return { type: "unknown", raw };
  }
  const r = raw as Record<string, unknown>;
  const t = r.type;
  if (t === "text" && typeof r.text === "string") {
    return { type: "text", text: r.text };
  }
  if (t === "thinking" && typeof r.thinking === "string") {
    return { type: "thinking", thinking: r.thinking };
  }
  if (t === "tool_use" && typeof r.id === "string" && typeof r.name === "string") {
    return {
      type: "tool_use",
      id: r.id,
      name: r.name,
      input: r.input ?? {},
      result: null,
    };
  }
  if (t === "tool_result" && typeof r.tool_use_id === "string") {
    const content = r.content;
    return {
      type: "tool_result",
      tool_use_id: r.tool_use_id,
      content:
        typeof content === "string"
          ? content
          : Array.isArray(content)
            ? content.map(toBlock)
            : "",
      is_error: r.is_error === true ? true : undefined,
    };
  }
  return { type: "unknown", raw };
}

function shortLabel(id: string, callCount: number): string {
  if (id.startsWith("anon-")) return id;
  const head = id.slice(0, 12);
  return `${head} · ${callCount} call${callCount === 1 ? "" : "s"}`;
}

function firstDefined<T>(xs: (T | undefined)[]): T | undefined {
  for (const x of xs) if (x !== undefined) return x;
  return undefined;
}
