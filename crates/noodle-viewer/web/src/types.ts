// Wire shape mirrors `crates/noodle-viewer/src/model.rs`. Drift is
// caught by Rust tests on the contract module + a `vitest` parse
// test on the client side.

export type Direction = "request" | "response";

export interface Exchange {
  direction: Direction;
  timestamp: string;
  event_id: string;
  provider: string;
  /** HTTP method (request only). */
  method?: string | null;
  /** Full request URL (request only). */
  url?: string | null;
  /** HTTP status (response only). */
  status?: number | null;
  session_hash?: string | null;
  headers?: Record<string, string[]>;
  /**
   * Bytes noodle received on this direction (pre-mutation).
   * Request: what the client sent. Response: what upstream sent.
   * On mutating paths this is the "pre-noodle" view.
   */
  body?: unknown;
  /**
   * Bytes noodle forwarded on this direction (post-mutation).
   * Present only when noodle injected (request) or stripped
   * (response) bytes. Absent (passthrough) means body_out == body.
   * The viewer renders body_out by default so the user sees what
   * actually crossed the wire after our modifications.
   */
  body_out?: unknown;
}

export interface CaptureState {
  enabled: boolean;
  file?: string | null;
}

/** One captured SSE frame. Mirrors `crates/noodle-viewer/src/model.rs::Frame`. */
export interface Frame {
  /** `request_id` pairs to `Exchange.event_id` of the same response. */
  request_id: string;
  /** 0-based, monotonic within a response. */
  frame_index: number;
  /** RFC 3339 / ISO-8601 UTC of arrival. */
  timestamp: string;
  /** Same instant in epoch-ms; used for `+Δms` per-frame deltas. */
  ts_unix_ms: number;
  /** The SSE `event:` field if present. */
  event?: string | null;
  /** Parsed JSON when the bytes were valid JSON; the raw string otherwise. */
  data: unknown;
}

/**
 * Item 4 viewer-panel slice (ADR 020 §7): attribution side-effect
 * events from `side_effects.jsonl`. Wire shape mirrors
 * `crates/noodle-viewer/src/model.rs::SideEffectEvent` and
 * ultimately `noodle-adapters::sink::JsonlEntry`.
 *
 * Discriminated union on `kind`. `Resolved` is the primary
 * attribution unit; `Hint`/`Artifact`/`Audit` are the contributing
 * records the resolver consumed.
 */
/**
 * ADR 051: round-trip correlation, flattened onto every side-effect
 * record by `noodle-sinks` `CorrelationFields`. `event_id` is the
 * round-trip id (proxy `nl-N`) — it keys a side-effect to the
 * round-trip that produced it. Optional: records emitted outside an
 * inspectable flow omit them.
 */
export interface SideEffectCorrelation {
  event_id?: string;
  /** The depth-0 turn this side-effect's round-trip belongs to
   *  (ADR 052 §5 `turn_id`); `null`/absent for side-calls. */
  turn_id?: string;
  /** The frame (agent run) this side-effect's round-trip belongs to
   *  (ADR 052 §5 `frame_id`; `"ROOT"` for the main agent). Replaces
   *  the removed `agent_run_id`. */
  frame_id?: string;
}

export type SideEffectEvent =
  | ({
      kind: "hint";
      category: string;
      value: string;
      confidence: number;
      source: string;
    } & SideEffectCorrelation)
  | ({
      kind: "artifact";
      name: string;
      value: string;
      source_transform: string;
      flow_id: number;
      captured_at_unix_ms: number;
    } & SideEffectCorrelation)
  | ({
      kind: "audit";
      kind_inner: string;
      transform: string;
      flow_id: number;
      at_unix_ms: number;
      detail?: unknown;
    } & SideEffectCorrelation)
  | ({
      kind: "resolved";
      session_prefix: string;
      flow_id: number;
      at_unix_ms: number;
      resolved: Record<string, string>;
    } & SideEffectCorrelation);

export type ServerMsg =
  | { kind: "hello"; version: string }
  | ({ kind: "exchange" } & Exchange)
  | ({ kind: "frame" } & Frame)
  | { kind: "side_effect"; event: SideEffectEvent }
  | { kind: "brain"; event_id: string; observation: BrainObservation }
  | { kind: "capture"; enabled: boolean; file?: string | null };

/**
 * ADR 047 rung 1 brain observation for one completed round-trip.
 * Wire shape mirrors `noodle_embellish_core::BrainObservation`. Joined
 * to an `Exchange` row by the matching `event_id` on the
 * `ServerMsg::Brain` event.
 *
 * The two compaction signals — `compaction_directive_present` (the
 * client's stated intent, lifted from the request body's
 * `context_management.edits[]`) and `compaction_detected` (the
 * structural diff confirming a real shrink) — are independent. A
 * row with `compaction_directive_present=true` and
 * `compaction_detected=false` is preventive maintenance (steady
 * state for Claude Code); the rare row with `compaction_detected=true`
 * is the moment the agent silently lost history.
 */
export interface BrainObservation {
  /** Stable id for the conversation thread; `"utility"` for sub-task calls. */
  thread_id: string;
  /** 1-based monotonic turn counter within this thread. */
  thread_turn_index: number;
  /** Structural shrink confirmed: messages[] is smaller than the prior turn. */
  compaction_detected: boolean;
  /** Client opted into compaction this turn via the `context_management` field. */
  compaction_directive_present: boolean;
  /** When `compaction_directive_present`, the edit kind (e.g. `clear_thinking_20251015`). */
  compaction_directive_kind?: string | null;
  /** Count of message signatures present prior turn but absent now. */
  blocks_dropped: number;
  /** Count of message signatures present now but absent prior. */
  blocks_added: number;
  /** Running high-water mark of provider-reported `input_tokens` for this thread. */
  estimated_window_tokens: number;
  /** `anthropic-beta` header lists `context-management-*`. */
  api_context_management_beta: boolean;
}

// Client-derived: a paired request + response for one event_id.
export interface ExchangePair {
  event_id: string;
  request?: Exchange;
  response?: Exchange;
}

// ─── S22: DecodedExchange ─────────────────────────────────────
//
// Mirrors `crates/noodle-viewer/src/model.rs::DecodedExchange` and
// the on-disk `tap.jsonl` shape (snake_case keys, on-disk token
// names like `input_tokens` rather than the internal `input`).
//
// The frontend consumes these over the `GET /api/decoded-exchanges`
// SSE stream. They ride alongside the legacy `Exchange` feed on
// `/ws`; views/components blend the two streams by `event_id`.

/**
 * Typed marks block — the per-round-trip §5 contract (ADR 052
 * `docs/adrs/052-turn-run-lineage-frame-tree.md`). The proxy
 * computes the turn/run/lineage tree on the wire and stamps these
 * authoritative marks; the viewer **renders** them and never
 * re-derives the tree (FR6, invariant 6).
 */
export interface DecodedMarks {
  /** The stack container — one session's whole frame tree. */
  session_id: string;
  /** Where this round-trip sits in the tree. `main` = the ROOT
   *  frame (the main agent); `sub_agent` = a child frame; `side_call`
   *  = off-tree harness call (quota / title-gen / security-monitor /
   *  suggestion / compactor), no turn, no place in the tree. */
  role: "main" | "sub_agent" | "side_call";
  /** The frame's identity — the spawning `tool_use.id`. `"ROOT"` for
   *  the main agent; `null` for a side-call. */
  frame_id: string | null;
  /** The frame that spawned this one; `null` for ROOT and side-calls. */
  parent_frame_id?: string | null;
  /** Sub-agent nesting level: 0 = main, 1+ = nested; `null` for a
   *  side-call. */
  depth?: number | null;
  /** The depth-0 turn this round-trip belongs to, stable across the
   *  entire recursion of one user prompt; `null` for a side-call. */
  turn_id?: string | null;
}

/** Token-usage shape on the wire — mirrors `usage.tokens.*` on
 *  `tap.jsonl` (ADR 030 / S8). */
export interface DecodedTokenUsage {
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens?: number | null;
  cache_creation_input_tokens?: number | null;
  reasoning_tokens?: number | null;
  vendor_extras?: Record<string, unknown>;
}

/** Latency shape on the wire. */
export interface DecodedLatency {
  time_to_first_byte_ms?: number | null;
  total_ms?: number | null;
}

/** Typed usage block (response-side only). */
export interface DecodedUsage {
  tokens?: DecodedTokenUsage | null;
  latency?: DecodedLatency | null;
}

/** Agent harness the proxy observed (Claude Code, Cursor, …). */
export interface DecodedAgentApp {
  name: string;
  version?: string | null;
  build_hash?: string | null;
  build_date?: string | null;
  source: string;
}

/** Host the agent ran on. */
export interface DecodedMachine {
  hostname?: string | null;
  os_family: string;
  os_version?: string | null;
  architecture: string;
  locale?: string | null;
  timezone?: string | null;
}

/** The noodle build that observed this round-trip. */
export interface DecodedCollectorApp {
  name: string;
  version: string;
  build_hash: string;
  build_date: string;
  features: string[];
}

/** API key fingerprint (prefix only — sensitive bytes redacted). */
export interface DecodedApiKey {
  prefix: string;
  kind: string;
  source: string;
}

/** Organization / account context. */
export interface DecodedOrganization {
  organization_id?: string | null;
  parent_organization_id?: string | null;
  account_type: string;
}

/** Subscription tier label / source. */
export interface DecodedTier {
  tier?: string | null;
  source: string;
}

/** Subscription / api-key / org context (ADR 029 family 13). */
export interface DecodedSubscription {
  api_key?: DecodedApiKey | null;
  organization?: DecodedOrganization | null;
  tier?: DecodedTier | null;
}

/** Typed envelope block. Each inner field individually optional. */
export interface DecodedEnvelope {
  agent_app?: DecodedAgentApp | null;
  machine?: DecodedMachine | null;
  collector_app?: DecodedCollectorApp | null;
  subscription?: DecodedSubscription | null;
}

/** Cross-record tool-use pairing (ADR 030 §4). */
export interface DecodedPairing {
  /** On a request record carrying a `tool_result`: event_id of the
   *  prior response record that emitted the originating `tool_use`. */
  resolves_tool_use_in_request_id?: string | null;
  /** On a response record carrying a `tool_use`: event_id of the
   *  subsequent request record that resolved the call. Populated
   *  only via patch records. */
  resolved_by_request_id?: string | null;
}

/**
 * One typed content block in the decoded layer (ADR 030 §2).
 * Discriminated on `kind`. Mirrors
 * `noodle_domain::decoders::DecodedEvent` on the Rust side.
 */
export type DecodedContentBlock =
  | {
      kind: "turn_start";
      request_id: string;
      provider: string;
      method?: string | null;
      url?: string | null;
    }
  | {
      kind: "turn_end";
      request_id: string;
      provider: string;
      status?: number | null;
      turn_end?: unknown;
      usage?: unknown;
    }
  | {
      kind: "content";
      request_id: string;
      provider: string;
      block_index: number;
      category: string;
      text: string;
      thinking_signature?: string | null;
    }
  | {
      kind: "tool_use";
      request_id: string;
      provider: string;
      block_index: number;
      tool_use_id: string;
      tool_name: string;
      input: unknown;
      capability: unknown;
    }
  | {
      kind: "vendor_specific";
      request_id: string;
      provider: string;
      direction: string;
      block_kind: string;
      vendor_kind: string;
      payload: unknown;
    };

/**
 * The typed wrapper riding alongside one `tap.jsonl` record. The
 * frontend pairs decoded records with the slim `Exchange` (from
 * `/ws`) by `exchange.event_id`.
 */
export interface DecodedExchange {
  exchange: Exchange;
  marks?: DecodedMarks | null;
  envelope?: DecodedEnvelope | null;
  usage?: DecodedUsage | null;
  content_blocks?: DecodedContentBlock[];
  events?: unknown[];
  pairing?: DecodedPairing | null;
  /** Attribution markers extracted from this record's content by
   *  the proxy's L5 transforms (e.g. MarkerStripTransform captures
   *  `<noodle:NAME>VALUE</noodle:NAME>` tags). Empty when the
   *  record carries no markers. */
  attribution_markers?: DecodedAttributionMarker[];
}

export interface DecodedAttributionMarker {
  name: string;
  value: string;
  source_transform: string;
}
