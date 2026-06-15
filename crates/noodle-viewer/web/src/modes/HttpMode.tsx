import { useEffect, useMemo, useRef, useState } from "react";
import type { BrainObservation, DecodedExchange, Exchange, ExchangePair } from "../types";
import { effectiveBody } from "../lib/effectiveBody";
import type { AttributionRow } from "../store/events";
import { TurnIdBadge } from "../components/TurnIdBadge";
import { UsagePanel } from "../components/UsagePanel";
import { ToolPairingArrow } from "../components/ToolPairingArrow";

interface Props {
  pairs: ExchangePair[];
  selected: string | null;
  onSelect: (eventId: string) => void;
  /** Item 4 viewer-panel slice (ADR 020 §7): per-row attribution
   *  lookup. Returns the latest `Resolved` row for the exchange's
   *  `session_hash`, or `undefined` if none yet. Optional — modes
   *  rendered without the attribution feed still work. */
  resolvedFor?: (sessionPrefix: string | null | undefined) => AttributionRow | undefined;
  /** S22 (refactor-overview.md §10): per-row decoded-exchange
   *  lookup. Returns the typed [`DecodedExchange`] (carries
   *  marks.turn_id, usage, pairing, …) for the exchange's
   *  event_id, or `undefined` if none yet. Optional —
   *  legacy/test callers pass nothing and the row renders as
   *  today. */
  decodedFor?: (eventId: string) => DecodedExchange | undefined;
  /** ADR 047 rung 1: per-row brain observation lookup. Returns the
   *  brain observation for the exchange's `event_id`, or `undefined`
   *  when the pair has not yet been observed. Optional — pre-brain
   *  callers pass nothing and the row renders without brain badges. */
  brainFor?: (eventId: string) => BrainObservation | undefined;
}

export function HttpMode({ pairs, selected, onSelect, resolvedFor, decodedFor, brainFor }: Props) {
  const [filter, setFilter] = useState("");
  // Sort by request timestamp (or response if request unknown).
  const sorted = useMemo(() => {
    const ts = (p: ExchangePair) =>
      p.request?.timestamp ?? p.response?.timestamp ?? "";
    return [...pairs].sort((a, b) => ts(a).localeCompare(ts(b)));
  }, [pairs]);

  const filtered = useMemo(
    () => sorted.filter((p) => matchesFilter(p, filter)),
    [sorted, filter],
  );

  // Per-row refs so arrow-key nav can scrollIntoView the focused row.
  // Map cleared on every render via `useMemo`-style reset; refs are
  // re-installed by the callback below as React commits the list.
  const rowRefs = useRef(new Map<string, HTMLDivElement>());

  // Arrow-key navigation. Listens on `window` so it works even when
  // the focused row scrolls off-screen (otherwise the row loses
  // focus and key events stop reaching its onKeyDown). Guards
  // against keys captured while typing in a future search box.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
      const target = e.target as HTMLElement | null;
      if (target?.matches?.("input, textarea, [contenteditable='true']")) return;
      if (filtered.length === 0) return;
      const idx = selected
        ? filtered.findIndex((p) => p.event_id === selected)
        : -1;
      let next: number;
      if (idx === -1) {
        next = e.key === "ArrowDown" ? 0 : filtered.length - 1;
      } else {
        const delta = e.key === "ArrowDown" ? 1 : -1;
        next = Math.max(0, Math.min(filtered.length - 1, idx + delta));
        if (next === idx) return; // clamp — no-op rather than fire onSelect on the same id
      }
      e.preventDefault();
      onSelect(filtered[next].event_id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [filtered, selected, onSelect]);

  // Whenever the selected id changes, scroll the focused row into
  // view if it's not already on screen. `block: "nearest"` keeps
  // the viewport stable when the row IS visible.
  //
  // Optional-chained call: jsdom doesn't implement `scrollIntoView`,
  // and the runtime path doesn't care if a stub-less environment
  // skips the scroll. Real browsers always have it.
  useEffect(() => {
    if (!selected) return;
    const el = rowRefs.current.get(selected);
    el?.scrollIntoView?.({ block: "nearest" });
  }, [selected]);

  if (sorted.length === 0) {
    return (
      <div className="empty">
        Waiting for traffic. Start the proxy and drive a request through it.
      </div>
    );
  }

  return (
    <div className="http-mode">
      <FilterBar
        value={filter}
        onChange={setFilter}
        matchCount={filtered.length}
        total={sorted.length}
      />
      <div className="http-header">
        <span>Time</span>
        <span>Method</span>
        <span>Host</span>
        <span>Provider</span>
        <span>Path</span>
        <span>Status</span>
        <span style={{ textAlign: "right" }}>Size</span>
      </div>
      <div className="http-list">
        {filtered.map((p) => (
          <Row
            key={p.event_id}
            pair={p}
            selected={selected === p.event_id}
            onClick={() => onSelect(p.event_id)}
            resolvedFor={resolvedFor}
            decoded={decodedFor?.(p.event_id)}
            brain={brainFor?.(p.event_id)}
            onJumpTo={onSelect}
            registerRef={(el) => {
              if (el) rowRefs.current.set(p.event_id, el);
              else rowRefs.current.delete(p.event_id);
            }}
          />
        ))}
        {filtered.length === 0 && (
          <div className="empty">
            No rows match the filter. Try a different vendor / path /
            method, or clear the filter to see all {sorted.length}{" "}
            exchanges.
          </div>
        )}
      </div>
    </div>
  );
}

/** Filter input with mitmweb-inspired prefixes:
 *  - `~m GET` / `~m POST` — method
 *  - `~h api.anthropic.com` — host substring
 *  - `~u /v1/messages` — path substring
 *  - `~s 200` / `~s 5xx` — status (exact or class match)
 *  - `~p anthropic` — provider substring
 *  - `~b foo` — body substring (request or response)
 *  - `~hh user-agent` — header name/value substring
 *  - any other text — matches across method, host, path, provider
 *    (substring, case-insensitive). Stack multiple terms with
 *    whitespace; all must match (AND).
 */
function FilterBar({
  value,
  onChange,
  matchCount,
  total,
}: {
  value: string;
  onChange: (v: string) => void;
  matchCount: number;
  total: number;
}) {
  return (
    <div className="http-filter-bar">
      <input
        type="text"
        className="http-filter-input"
        placeholder="Filter… (~m POST  ~h anthropic  ~u /v1/messages  ~s 5xx  ~b token  ~hh user-agent)"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        spellCheck={false}
        autoComplete="off"
        aria-label="Filter HTTP rows"
      />
      <span className="http-filter-count">
        {value.trim() === ""
          ? `${total} exchanges`
          : `${matchCount} / ${total} match`}
      </span>
      {value && (
        <button
          type="button"
          className="http-filter-clear"
          onClick={() => onChange("")}
          title="Clear filter"
        >
          ✕
        </button>
      )}
    </div>
  );
}

/** Parse the filter string into AND-combined predicates and run
 *  them against the pair. Empty filter passes everything. */
function matchesFilter(pair: ExchangePair, filter: string): boolean {
  const q = filter.trim();
  if (q === "") return true;
  const terms = splitTerms(q);
  return terms.every((t) => evalTerm(pair, t));
}

interface Term {
  kind:
    | "method"
    | "host"
    | "url"
    | "status"
    | "provider"
    | "body"
    | "header"
    | "any";
  value: string;
}

/** Split on whitespace but keep `~xx` prefixes attached to their
 *  argument. The full string after the prefix up to the next
 *  whitespace is the value. */
function splitTerms(q: string): Term[] {
  const out: Term[] = [];
  const re = /(~hh|~[mhusupb])\s+(\S+)|(\S+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(q)) !== null) {
    if (m[1] && m[2]) {
      out.push({ kind: prefixKind(m[1]), value: m[2] });
    } else if (m[3]) {
      out.push({ kind: "any", value: m[3] });
    }
  }
  return out;
}

function prefixKind(prefix: string): Term["kind"] {
  switch (prefix) {
    case "~m":
      return "method";
    case "~h":
      return "host";
    case "~u":
      return "url";
    case "~s":
      return "status";
    case "~p":
      return "provider";
    case "~b":
      return "body";
    case "~hh":
      return "header";
    default:
      return "any";
  }
}

function evalTerm(pair: ExchangePair, term: Term): boolean {
  const { request, response } = pair;
  const v = term.value.toLowerCase();
  switch (term.kind) {
    case "method":
      return (request?.method ?? "").toLowerCase() === v
        || (request?.method ?? "").toLowerCase().includes(v);
    case "host":
      return hostFor(request).toLowerCase().includes(v);
    case "url":
      return (
        (request?.url ?? "").toLowerCase().includes(v)
        || pathFor(request).toLowerCase().includes(v)
      );
    case "status":
      return matchesStatus(response?.status ?? null, term.value);
    case "provider": {
      const p = (request?.provider ?? response?.provider ?? "").toLowerCase();
      return p.includes(v);
    }
    case "body":
      return (
        bodyText(request).toLowerCase().includes(v)
        || bodyText(response).toLowerCase().includes(v)
      );
    case "header":
      return headerSearch(request, v) || headerSearch(response, v);
    case "any":
      return (
        (request?.method ?? "").toLowerCase().includes(v)
        || hostFor(request).toLowerCase().includes(v)
        || pathFor(request).toLowerCase().includes(v)
        || (request?.provider ?? response?.provider ?? "")
          .toLowerCase()
          .includes(v)
      );
  }
}

function matchesStatus(s: number | null, q: string): boolean {
  if (s === null) return q === "—" || q.toLowerCase() === "pending";
  // Numeric exact or NNx class match (e.g. "5xx").
  if (/^\d+$/.test(q)) return s === Number(q);
  const m = q.toLowerCase().match(/^(\d)xx$/);
  if (m) return Math.floor(s / 100) === Number(m[1]);
  return String(s).includes(q);
}

function bodyText(r?: Exchange): string {
  const b = effectiveBody(r);
  if (b === null || b === undefined) return "";
  return typeof b === "string" ? b : JSON.stringify(b);
}

function headerSearch(r: Exchange | undefined, q: string): boolean {
  if (!r?.headers) return false;
  for (const [name, values] of Object.entries(r.headers)) {
    if (name.toLowerCase().includes(q)) return true;
    if (Array.isArray(values)) {
      for (const v of values) {
        if (typeof v === "string" && v.toLowerCase().includes(q)) return true;
      }
    }
  }
  return false;
}

function Row({
  pair,
  selected,
  onClick,
  resolvedFor,
  decoded,
  brain,
  onJumpTo,
  registerRef,
}: {
  pair: ExchangePair;
  selected: boolean;
  onClick: () => void;
  resolvedFor?: (sessionPrefix: string | null | undefined) => AttributionRow | undefined;
  decoded?: DecodedExchange;
  brain?: BrainObservation;
  onJumpTo?: (eventId: string) => void;
  registerRef: (el: HTMLDivElement | null) => void;
}) {
  const { request, response } = pair;
  const ts = displayTime(request?.timestamp ?? response?.timestamp);
  const method = request?.method ?? "—";
  const host = hostFor(request);
  const path = pathFor(request);
  const status = response?.status ?? null;
  const provider = (request?.provider ?? response?.provider ?? "unknown").toLowerCase();
  const size = bodySize(response);
  // Item 4 stretch: per-row attribution chip when a Resolved
  // exists for this exchange's session.
  const attribution = resolvedFor?.(
    request?.session_hash ?? response?.session_hash,
  );
  const chipText = attributionSummary(attribution);

  return (
    <div
      ref={registerRef}
      className={`http-row${selected ? " selected" : ""}`}
      onClick={onClick}
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onClick();
        }
      }}
    >
      <span className="ts">{ts}</span>
      <span className="method">{method}</span>
      <span className="host" title={host}>{host}</span>
      <span className={`provider ${provider}`}>{provider}</span>
      <span className="path" title={path}>
        <span className="path-text">{path}</span>
        {/* S22: per-row decoded-layer chips (turn id, usage,
            pairing). Rendered after the path, before status,
            so they fold into the existing row layout without
            shifting columns. */}
        {decoded?.marks?.turn_id && (
          <TurnIdBadge turnId={decoded.marks.turn_id} />
        )}
        {decoded?.usage && <UsagePanel usage={decoded.usage} mode="inline" />}
        {decoded?.pairing && (
          <ToolPairingArrow pairing={decoded.pairing} onJump={onJumpTo} />
        )}
        {decoded?.attribution_markers?.map((m) => (
          <span
            key={`${m.name}=${m.value}`}
            className="attribution-marker-chip"
            title={`${m.name} = ${m.value} (extracted by ${m.source_transform})`}
          >
            {m.name}: {m.value}
          </span>
        ))}
        {brain && <BrainChip brain={brain} />}
        {chipText && (
          <span className="attribution-chip" title={chipText}>
            {chipText}
          </span>
        )}
      </span>
      <span className={`status ${statusClass(status)}`}>
        {status === null ? "—" : status}
      </span>
      <span className="size">{size}</span>
    </div>
  );
}

/** ADR 047 rung 1 brain chip. Three visual tiers reflect the
 *  2×2 (directive × detected) value table:
 *
 *  - **Red** — `compaction_detected=true`: structural shrink confirmed.
 *    This is the moment the agent silently lost history. Headline event.
 *  - **Amber** — `compaction_directive_present=true` without detected:
 *    preventive maintenance (steady state for Claude Code with the
 *    context-management beta on).
 *  - **Grey** — utility-bucket sub-task call (`thread_id="utility"`).
 *
 *  Hovering surfaces the directive kind, turn index, and dropped /
 *  added block counts.
 */
function BrainChip({ brain }: { brain: BrainObservation }) {
  const isCompaction = brain.compaction_detected;
  const isUtility = brain.thread_id === "utility";
  const tier = isCompaction ? "compaction" : isUtility ? "utility" : "directive";
  const label = isCompaction
    ? `🧠 -${brain.blocks_dropped} blocks`
    : isUtility
      ? `🧠 utility`
      : // ADR 047 brain `thread_turn_index` — a per-round-trip counter on the
        // brain's thread, NOT the ADR 052 §5 turn (which is the `turn:` chip).
        // Labelled "rt" so it doesn't read as the depth-0 turn and contradict it.
        `🧠 rt ${brain.thread_turn_index}`;
  const titleLines = [
    `thread_id: ${brain.thread_id}`,
    `thread_turn_index (per-round-trip, not the §5 turn): ${brain.thread_turn_index}`,
    `compaction_detected: ${brain.compaction_detected}`,
    `compaction_directive_present: ${brain.compaction_directive_present}`,
  ];
  if (brain.compaction_directive_kind) {
    titleLines.push(`directive_kind: ${brain.compaction_directive_kind}`);
  }
  titleLines.push(`blocks_dropped: ${brain.blocks_dropped}`);
  titleLines.push(`blocks_added: ${brain.blocks_added}`);
  titleLines.push(`estimated_window_tokens: ${brain.estimated_window_tokens}`);
  titleLines.push(`api_context_management_beta: ${brain.api_context_management_beta}`);
  return (
    <span className={`brain-chip brain-chip-${tier}`} title={titleLines.join("\n")}>
      {label}
    </span>
  );
}

/** Build a compact `tool: X · work_type: Y` string from a
 *  Resolved row. Returns `null` when the row is not a
 *  Resolved or has no resolved categories. */
function attributionSummary(row: AttributionRow | undefined): string | null {
  if (!row || row.event.kind !== "resolved") return null;
  const entries = Object.entries(row.event.resolved);
  if (entries.length === 0) return null;
  return entries.map(([k, v]) => `${k}: ${v}`).join(" · ");
}

function displayTime(ts: string | undefined): string {
  if (!ts) return "—";
  // 2026-05-10T17:08:59.123Z → 17:08:59.123
  const m = ts.match(/T(\d{2}:\d{2}:\d{2}(?:\.\d+)?)/);
  return m?.[1]?.slice(0, 12) ?? ts;
}

function hostFor(r?: Exchange): string {
  if (!r) return "";
  try {
    if (r.url) return new URL(r.url).host;
  } catch {
    /* fall through to header */
  }
  return headerFirst(r, "host") ?? "";
}

function pathFor(r?: Exchange): string {
  if (!r) return "";
  try {
    if (r.url) {
      const u = new URL(r.url);
      return u.pathname + u.search;
    }
  } catch {
    /* fall through */
  }
  return "—";
}

function headerFirst(r: Exchange | undefined, name: string): string | null {
  const v = r?.headers?.[name] ?? r?.headers?.[name.toLowerCase()];
  return Array.isArray(v) ? (v[0] ?? null) : null;
}

function statusClass(s: number | null): string {
  if (s === null) return "pending";
  if (s >= 500) return "err";
  if (s >= 400) return "warn";
  if (s >= 200) return "ok";
  return "pending";
}

function bodySize(r?: Exchange): string {
  const body = effectiveBody(r);
  if (!r || body === undefined || body === null) return "—";
  const s = typeof body === "string" ? body : JSON.stringify(body);
  return formatBytes(s.length);
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n}B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)}K`;
  return `${(n / (1024 * 1024)).toFixed(1)}M`;
}
