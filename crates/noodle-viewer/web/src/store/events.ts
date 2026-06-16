// Event store — receives ServerMsg from the WebSocket, builds
// ExchangePairs on demand, exposes a useSyncExternalStore-friendly
// API.
//
// Design: append-only log + cached pair index. Mode views read either
// the raw log or the pair index depending on what they need. Re-derive
// in the view layer with `useMemo` so this store stays tiny.

import type { ParsedResponse, ParseCache } from "./derived/ooda";
import type {
  BrainObservation,
  ContextWeight,
  DecodedExchange,
  Exchange,
  ExchangePair,
  Frame,
  ServerMsg,
  SideEffectEvent,
  CaptureState,
} from "../types";

/**
 * Item 4 viewer-panel slice (ADR 020 §7): one row in the
 * attribution feed. Captures arrival order + the wire event.
 *
 * Stamped at the store boundary (`Date.now()`) rather than
 * extracted from the side-effect payload because not every
 * variant carries a wall-clock stamp — `Hint` lacks one entirely.
 */
export interface AttributionRow {
  /** Monotonic sequence number assigned by the store on
   *  ingest. Useful for stable React `key` props. */
  seq: number;
  /** Wall-clock millis at ingest. */
  received_unix_ms: number;
  event: SideEffectEvent;
}

// ─── LEARNED (ADR 051) ────────────────────────────────────────────
// The per-round-trip knowledge noodle extracted from a round-trip's
// bytes, keyed by `event_id`. Assembled from the side-effect feed
// (attribution + evidence) joined with the decoded exchange (context
// tokens + lineage + pairing). This is the debugger's right column:
// traffic in, knowledge out, one round-trip at a time.

/** One contributing signal behind an attribution value. */
export interface LearnedEvidence {
  /** `hint.category` or `artifact.name`. */
  category: string;
  value: string;
  /** `hint.source` (`marker`/`user_agent`) or `artifact.source_transform`. */
  source: string;
  /** Present for hints; artifacts carry no confidence. */
  confidence?: number;
  kind: "hint" | "artifact";
}

export interface LearnedAttribution {
  /** Resolved `category → value` for this round-trip. */
  values: Record<string, string>;
  /** `category → previous value` for categories that changed from the
   *  prior round-trip in the same turn. Empty for the first
   *  round-trip of a turn (invariant 4). */
  delta: Record<string, string | null>;
}

export interface LearnedContext {
  input_tokens?: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
  /** `input_tokens` change vs the prior round-trip in the turn;
   *  `null` when there is no predecessor. */
  input_delta?: number | null;
}

/** Lineage line for a round-trip, re-bound to the §5 marks (ADR 052
 *  §11). The agent-run identity is the `frame_id` (the spawning
 *  `tool_use.id`; `"ROOT"` for the main agent); the parent is a
 *  single `parent_frame_id`. The four legacy `parent_*` fields and
 *  `agent_run_id` are gone. */
export interface LearnedLineage {
  frame_id?: string | null;
  parent_frame_id?: string | null;
}

export interface LearnedPairing {
  /** This round-trip's `tool_result` closes a `tool_use` emitted in
   *  this prior request's round-trip. */
  resolves_tool_use_in_request_id?: string | null;
  /** This round-trip's `tool_use` was answered by this later
   *  request's round-trip. */
  resolved_by_request_id?: string | null;
}

/** Assembled per-round-trip LEARNED record (invariant 1: one
 *  `event_id`). Returned by `EventStore.getLearnedFor`. */
export interface LearnedRecord {
  event_id: string;
  turn_id?: string | null;
  attribution: LearnedAttribution;
  evidence: LearnedEvidence[];
  context: LearnedContext;
  lineage: LearnedLineage;
  pairing: LearnedPairing;
}

/** Mutable per-round-trip accumulator, fed incrementally from the
 *  side-effect stream. Read-time join with the decoded exchange
 *  produces the public {@link LearnedRecord}. */
interface LearnedAccumulator {
  event_id: string;
  turn_id?: string;
  frame_id?: string;
  /** Resolved `category → value`; last write wins (matches the
   *  engine's own resolution semantics). */
  resolved: Record<string, string>;
  evidence: LearnedEvidence[];
}

type Listener = () => void;

/**
 * Per-request summary used by the SSE-mode rail. Computed
 * incrementally on every ingest; the underlying frame list lives in
 * `framesByRequestId`.
 */
export interface FrameRequestSummary {
  request_id: string;
  count: number;
  /** Event of the first frame (`message_start` for Anthropic etc.). */
  first_event: string | null;
  /** ts_unix_ms of the first frame. */
  first_ts: number;
  /** ts_unix_ms of the most recent frame — drives rail sort. */
  last_ts: number;
}

/**
 * Map-backed `ParseCache` co-located on the store. Owns invalidation:
 * cleared in `EventStore.clearLocal()` so cleared exchanges' parses
 * don't linger.
 *
 * Stats are exposed for the dev-perf badge.
 */
export class StoreParseCache implements ParseCache {
  private entries = new Map<string, ParsedResponse>();
  private hits = 0;
  private misses = 0;

  get(eventId: string): ParsedResponse | undefined {
    const v = this.entries.get(eventId);
    if (v) this.hits++;
    else this.misses++;
    return v;
  }
  set(eventId: string, parsed: ParsedResponse): void {
    this.entries.set(eventId, parsed);
  }
  clear(): void {
    this.entries.clear();
    this.hits = 0;
    this.misses = 0;
  }
  size(): number {
    return this.entries.size;
  }
  stats(): { size: number; hits: number; misses: number } {
    return { size: this.entries.size, hits: this.hits, misses: this.misses };
  }
}

export class EventStore {
  private exchanges: Exchange[] = [];
  // event_id → index into exchanges (the request side, if known).
  private byId = new Map<string, ExchangePair>();
  private capture: CaptureState = { enabled: false };
  private connected = false;
  private listeners = new Set<Listener>();
  // Cached snapshots — useSyncExternalStore requires that getSnapshot
  // returns the same reference until state changes, otherwise React
  // 19's StrictMode aborts render with "getSnapshot should be cached".
  private pairsSnapshot: ExchangePair[] = [];
  // Per-request frame index. Each Map value is a *new* array on
  // mutation (functional update) so callers comparing references can
  // detect change without deep-equality.
  private framesByRequestId = new Map<string, Frame[]>();
  // Cached snapshots for SSE mode. Same useSyncExternalStore
  // requirement as pairs.
  private frameSummariesSnapshot: FrameRequestSummary[] = [];
  // Memoized OODA-parse results per event_id. Passed to
  // `buildSessions(pairs, pairsById, parseCache)` so each response
  // body is parsed at most once for the lifetime of the capture.
  readonly parseCache = new StoreParseCache();

  // ─── ADR 047 rung 1 brain observations ───────────────────────
  /** Map keyed by round-trip `event_id`. Populated by `ServerMsg::Brain`
   *  events that arrive after both halves of a pair landed. Joined to
   *  the `ExchangePair` view by `event_id`. */
  private brainsByEventId = new Map<string, BrainObservation>();
  /** Index by `brain.thread_id` → event_ids in arrival order. Drives
   *  the per-thread timeline view (Phase 2). */
  private brainsByThreadId = new Map<string, string[]>();
  /** Cached snapshot ref for `useSyncExternalStore`. Replaced on each
   *  brain ingest so React detects the change. */
  private brainsSnapshot: ReadonlyMap<string, BrainObservation> = new Map();

  // ─── ADR 056 context weight ──────────────────────────────────
  /** Map keyed by round-trip `event_id`. Populated by the
   *  `context_weight` `ServerMsg` the hub emits once a response's
   *  decoded usage pairs with its request body. Joined to the row by
   *  `event_id`, exactly like brain observations. */
  private contextWeightByEventId = new Map<string, ContextWeight>();

  // ─── S22: typed DecodedExchange feed (/api/decoded-exchanges) ─
  /** Map keyed by `exchange.event_id`. Holds the LATEST DecodedExchange
   *  for each event_id; request and response records share the
   *  same id, so the response wins (which is what we want — the
   *  response carries usage / content_blocks / events). */
  private decodedByEventId = new Map<string, DecodedExchange>();
  /** Index by `marks.turn_id` → list of event_ids in arrival order.
   *  Populated as decoded records flow in; used by the OODA view
   *  to group rows by turn directly (LEGACY ooda.ts heuristic
   *  falls back when this index is empty). */
  private eventIdsByTurnId = new Map<string, string[]>();

  // ─── Attribution feed (item 4 viewer-panel slice, ADR 020 §7) ─
  /** Rolling buffer of incoming side-effects, oldest→newest. The
   *  panel renders the tail and the live "incoming" indicator. */
  private attribution: AttributionRow[] = [];
  /** Cached snapshot for `useSyncExternalStore`. */
  private attributionSnapshot: AttributionRow[] = [];
  /** Index for per-exchange lookup: `session_prefix` → latest
   *  Resolved row. The HTTP / OODA mode rows show a one-line
   *  attribution chip when the matching session is present. */
  private resolvedBySession = new Map<string, AttributionRow>();
  /** ADR 051: per-round-trip LEARNED accumulators, keyed by the
   *  side-effect correlation `event_id`. Joined with the decoded
   *  exchange at read time in {@link getLearnedFor}. */
  private learnedByEventId = new Map<string, LearnedAccumulator>();
  private attributionSeq = 0;
  /** Hard cap on the rolling buffer. The earliest rows fall off;
   *  the `resolvedBySession` index keeps the latest per session
   *  even if the row itself ages out (rare but possible at the
   *  cap). */
  private static readonly ATTRIBUTION_CAP = 5_000;

  ingest(msg: ServerMsg): void {
    switch (msg.kind) {
      case "hello":
        this.connected = true;
        break;
      case "exchange": {
        const ex: Exchange = { ...msg };
        this.exchanges.push(ex);
        const existing = this.byId.get(ex.event_id);
        if (existing) {
          if (ex.direction === "request") existing.request = ex;
          else existing.response = ex;
        } else {
          const pair: ExchangePair = { event_id: ex.event_id };
          if (ex.direction === "request") pair.request = ex;
          else pair.response = ex;
          this.byId.set(ex.event_id, pair);
        }
        this.refreshPairsSnapshot();
        break;
      }
      case "frame": {
        const f: Frame = {
          request_id: msg.request_id,
          frame_index: msg.frame_index,
          timestamp: msg.timestamp,
          ts_unix_ms: msg.ts_unix_ms,
          event: msg.event ?? null,
          data: msg.data,
        };
        const prev = this.framesByRequestId.get(f.request_id);
        // Functional update: replace the array reference so React
        // components subscribing via useSyncExternalStore can detect
        // change with Object.is. Inserts append in arrival order;
        // out-of-order arrivals (rare) sort by frame_index lazily on
        // read.
        const next = prev ? [...prev, f] : [f];
        this.framesByRequestId.set(f.request_id, next);
        this.refreshFrameSummariesSnapshot();
        break;
      }
      case "side_effect": {
        const row: AttributionRow = {
          seq: ++this.attributionSeq,
          received_unix_ms: Date.now(),
          event: msg.event,
        };
        this.attribution.push(row);
        if (this.attribution.length > EventStore.ATTRIBUTION_CAP) {
          // Drop from the front. Resolved index entries that no
          // longer have a backing row stay cached — the index is
          // the source of truth for "latest resolved per session"
          // regardless of buffer eviction.
          this.attribution.shift();
        }
        if (msg.event.kind === "resolved") {
          this.resolvedBySession.set(msg.event.session_prefix, row);
        }
        this.ingestLearned(msg.event);
        this.refreshAttributionSnapshot();
        break;
      }
      case "capture":
        this.capture = { enabled: msg.enabled, file: msg.file };
        break;
      case "brain": {
        const observation: BrainObservation = msg.observation;
        this.brainsByEventId.set(msg.event_id, observation);
        const thread = observation.thread_id;
        const ids = this.brainsByThreadId.get(thread);
        if (ids) {
          if (!ids.includes(msg.event_id)) ids.push(msg.event_id);
        } else {
          this.brainsByThreadId.set(thread, [msg.event_id]);
        }
        // New Map ref so useSyncExternalStore detects the change.
        this.brainsSnapshot = new Map(this.brainsByEventId);
        break;
      }
      case "context_weight": {
        // ADR 056 — join to the row by event_id, like brain.
        this.contextWeightByEventId.set(msg.event_id, msg.weight);
        break;
      }
    }
    this.emit();
  }

  /**
   * S22: ingest one [`DecodedExchange`] from the SSE feed.
   *
   * Stored by `exchange.event_id`. Indexed by `marks.turn_id`
   * when present so the OODA view can group rows by the proxy-
   * minted turn id directly (refactor-overview.md §10). Listeners
   * fire so React subscribers re-render — the decoded layer is
   * additive UX, no view-mode mutation here.
   */
  ingestDecoded(dx: DecodedExchange): void {
    const id = dx.exchange.event_id;
    if (!id) return;
    this.decodedByEventId.set(id, dx);
    const turnId = dx.marks?.turn_id;
    if (turnId) {
      const arr = this.eventIdsByTurnId.get(turnId);
      if (arr) {
        if (!arr.includes(id)) arr.push(id);
      } else {
        this.eventIdsByTurnId.set(turnId, [id]);
      }
    }
    this.emit();
  }

  setConnected(v: boolean): void {
    this.connected = v;
    this.emit();
  }

  clearLocal(): void {
    this.exchanges = [];
    this.byId.clear();
    this.framesByRequestId.clear();
    this.parseCache.clear();
    this.attribution = [];
    this.resolvedBySession.clear();
    this.learnedByEventId.clear();
    this.attributionSeq = 0;
    this.decodedByEventId.clear();
    this.eventIdsByTurnId.clear();
    this.refreshPairsSnapshot();
    this.refreshFrameSummariesSnapshot();
    this.refreshAttributionSnapshot();
    this.emit();
  }

  // Snapshot getters — return cached references so `useSyncExternalStore`
  // sees `Object.is(prev, next)` whenever state hasn't changed.
  getPairs(): ExchangePair[] {
    return this.pairsSnapshot;
  }
  getCapture(): CaptureState {
    return this.capture;
  }
  isConnected(): boolean {
    return this.connected;
  }
  /** Per-request summaries (one per request_id with frames). Sort
   *  order is *not* fixed by the store; callers sort for display. */
  getFrameSummaries(): FrameRequestSummary[] {
    return this.frameSummariesSnapshot;
  }
  /** All frames for `requestId` in `frame_index` order. Returns the
   *  current cached array; callers should treat it as immutable. */
  getFramesFor(requestId: string): Frame[] {
    return this.framesByRequestId.get(requestId) ?? EMPTY_FRAMES;
  }
  /** Attribution feed in arrival order, oldest first. Cached
   *  reference; safe for `useSyncExternalStore`. */
  getAttribution(): AttributionRow[] {
    return this.attributionSnapshot;
  }
  /** Map of `event_id` → [`BrainObservation`]. Cached reference;
   *  replaced on each brain ingest. Components join to
   *  `ExchangePair.event_id` to surface brain badges + per-thread
   *  metadata. */
  getBrains(): ReadonlyMap<string, BrainObservation> {
    return this.brainsSnapshot;
  }
  /** Single brain observation for a `event_id`, if one has arrived. */
  getBrainFor(eventId: string): BrainObservation | undefined {
    return this.brainsByEventId.get(eventId);
  }
  /** ADR 056 — context weight for a `event_id`, if it has arrived. */
  getContextWeightFor(eventId: string): ContextWeight | undefined {
    return this.contextWeightByEventId.get(eventId);
  }
  /** List of `event_id`s observed in a given `thread_id`, arrival
   *  order. Drives the per-thread timeline view. */
  getBrainThread(threadId: string): readonly string[] {
    return this.brainsByThreadId.get(threadId) ?? EMPTY_THREAD;
  }
  /** Latest `Resolved` row for a given `session_prefix`, if any.
   *  Used by HTTP / OODA mode to render an inline attribution
   *  chip on rows whose `session_hash` matches. */
  getResolvedForSession(
    sessionPrefix: string | null | undefined,
  ): AttributionRow | undefined {
    if (!sessionPrefix) return undefined;
    return this.resolvedBySession.get(sessionPrefix);
  }

  /**
   * ADR 051: fold one side-effect into the per-round-trip LEARNED
   * accumulator keyed by its correlation `event_id`. Records without
   * an `event_id` (emitted outside an inspectable flow) are skipped —
   * they cannot be tied to a round-trip.
   */
  private ingestLearned(ev: SideEffectEvent): void {
    const eventId = ev.event_id;
    if (!eventId) return;
    let acc = this.learnedByEventId.get(eventId);
    if (!acc) {
      acc = { event_id: eventId, resolved: {}, evidence: [] };
      this.learnedByEventId.set(eventId, acc);
    }
    if (ev.turn_id) acc.turn_id = ev.turn_id;
    if (ev.frame_id) acc.frame_id = ev.frame_id;
    switch (ev.kind) {
      case "resolved":
        for (const [cat, val] of Object.entries(ev.resolved)) {
          acc.resolved[cat] = val; // last write wins
        }
        break;
      case "hint":
        acc.evidence.push({
          category: ev.category,
          value: ev.value,
          source: ev.source,
          confidence: ev.confidence,
          kind: "hint",
        });
        break;
      case "artifact":
        acc.evidence.push({
          category: ev.name,
          value: ev.value,
          source: ev.source_transform,
          kind: "artifact",
        });
        break;
      // audit carries no attribution/evidence value — skipped.
    }
  }

  /**
   * ADR 051: assemble the LEARNED record for one round-trip. Joins
   * the side-effect accumulator (attribution + evidence) with the
   * decoded exchange (context tokens, lineage, pairing), and computes
   * the per-turn delta against the immediately prior round-trip in
   * the same `turn_id`. Returns `undefined` when neither stream has
   * anything for the round-trip.
   */
  getLearnedFor(eventId: string | null | undefined): LearnedRecord | undefined {
    if (!eventId) return undefined;
    const acc = this.learnedByEventId.get(eventId);
    const decoded = this.decodedByEventId.get(eventId);
    if (!acc && !decoded) return undefined;

    const turnId = acc?.turn_id ?? decoded?.marks?.turn_id ?? null;
    const order = turnId ? this.eventIdsByTurnId.get(turnId) ?? [] : [];
    const idx = order.indexOf(eventId);
    const prevId = idx > 0 ? order[idx - 1] : undefined;
    const prevAcc = prevId ? this.learnedByEventId.get(prevId) : undefined;
    const prevDecoded = prevId ? this.decodedByEventId.get(prevId) : undefined;

    const values = acc?.resolved ?? {};
    const delta: Record<string, string | null> = {};
    if (prevAcc) {
      for (const [cat, val] of Object.entries(values)) {
        const prev = prevAcc.resolved[cat];
        if (prev !== val) delta[cat] = prev ?? null;
      }
    }

    const tok = decoded?.usage?.tokens;
    const prevTok = prevDecoded?.usage?.tokens;
    const context: LearnedContext = {
      input_tokens: tok?.input_tokens,
      cache_read_input_tokens: tok?.cache_read_input_tokens ?? undefined,
      cache_creation_input_tokens: tok?.cache_creation_input_tokens ?? undefined,
      input_delta:
        tok && prevTok ? tok.input_tokens - prevTok.input_tokens : null,
    };

    const m = decoded?.marks;
    const lineage: LearnedLineage = {
      frame_id: acc?.frame_id ?? m?.frame_id ?? null,
      parent_frame_id: m?.parent_frame_id ?? null,
    };

    const p = decoded?.pairing;
    const pairing: LearnedPairing = {
      resolves_tool_use_in_request_id:
        p?.resolves_tool_use_in_request_id ?? null,
      resolved_by_request_id: p?.resolved_by_request_id ?? null,
    };

    return {
      event_id: eventId,
      turn_id: turnId,
      attribution: { values, delta },
      evidence: acc?.evidence ?? [],
      context,
      lineage,
      pairing,
    };
  }

  /** S22: typed [`DecodedExchange`] for one event_id, if any.
   *  Returns the latest stored record — typically the response
   *  side (richer than the request side; the request gets
   *  overwritten when the response arrives). */
  getDecodedFor(eventId: string | null | undefined): DecodedExchange | undefined {
    if (!eventId) return undefined;
    return this.decodedByEventId.get(eventId);
  }

  /** S22: list of event_ids that share a `marks.turn_id`, in
   *  arrival order. Empty when the proxy hasn't stamped that
   *  turn yet (OODA view falls back to its heuristic). */
  getEventIdsForTurn(turnId: string | null | undefined): string[] {
    if (!turnId) return EMPTY_IDS;
    return this.eventIdsByTurnId.get(turnId) ?? EMPTY_IDS;
  }

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  private refreshPairsSnapshot(): void {
    this.pairsSnapshot = Array.from(this.byId.values());
  }

  private refreshAttributionSnapshot(): void {
    // Functional update — fresh array reference so React detects
    // change via Object.is. Cap-bounded copy.
    this.attributionSnapshot = this.attribution.slice();
  }

  private refreshFrameSummariesSnapshot(): void {
    const next: FrameRequestSummary[] = [];
    for (const [request_id, frames] of this.framesByRequestId) {
      if (frames.length === 0) continue;
      next.push({
        request_id,
        count: frames.length,
        first_event: frames[0].event ?? null,
        first_ts: frames[0].ts_unix_ms,
        last_ts: frames[frames.length - 1].ts_unix_ms,
      });
    }
    this.frameSummariesSnapshot = next;
  }

  private emit(): void {
    for (const fn of this.listeners) fn();
  }
}

/** Stable empty reference returned by `getFramesFor` for unknown
 *  ids. Avoids allocating a new array on each render so React's
 *  cheap-equality memoization stays cheap. Frozen so accidental
 *  mutation throws in strict mode. */
const EMPTY_FRAMES: Frame[] = Object.freeze([] as Frame[]) as Frame[];
const EMPTY_THREAD: readonly string[] = Object.freeze([]) as readonly string[];

/** Stable empty reference for `getEventIdsForTurn`. */
const EMPTY_IDS: string[] = Object.freeze([] as string[]) as string[];
