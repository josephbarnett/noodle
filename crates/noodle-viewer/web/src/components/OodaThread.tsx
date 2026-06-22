// Renders one OodaSession as a flat, chronological thread of
// role-tagged blocks. The visual model matches the TAP-viewer
// reference: USER (blue), AGENT (teal), HEADERS (gray collapsed),
// THINKING (amber collapsed), TOOL (orange), with byte sizes and
// timestamps on every row.

import { Fragment, useEffect, useMemo, useState } from "react";
import { Block } from "./Block";
import { HeadersBlock } from "./HeadersBlock";
import { classifyTool } from "../lib/toolBucket";
import type { AgentRun, OodaSession } from "../store/derived/ooda";
import type { ContentBlock } from "../store/derived/ooda";
import type { ExchangePair } from "../types";
import { flattenAgentRun, isLikelySystemReminder, type ThreadItem } from "../store/derived/thread";

interface Props {
  session: OodaSession;
  /** Which agent run inside the session to render. */
  run: AgentRun;
  /** Lookup of round-trip exchange pairs by event_id for header display. */
  pairsById: Map<string, ExchangePair>;
  /** Switch to a different agent run inside the same session
   *  (e.g., from a parent's Agent tool_use → click → child run). */
  onJumpToRun?: (runIdx: number) => void;
  /** Turn the rail asked us to reveal — scrolled into view and
   *  highlighted (story 057). `null` = no rail-driven selection. */
  activeTurn?: number | null;
}

export function OodaThread({ session, run, pairsById, onJumpToRun, activeTurn }: Props) {
  // Within this session, index sub-agent runs by the tool_use id that
  // spawned them so the parent's Agent tool block can show a link.
  const childRunByToolUseId = useMemo(() => {
    const m = new Map<string, AgentRun>();
    for (const r of session.agentRuns) {
      if (r.spawnedBy) m.set(r.spawnedBy.toolUseId, r);
    }
    return m;
  }, [session.agentRuns]);
  const items = useMemo(() => flattenAgentRun(run), [run]);
  const allTurnNums = useMemo(
    () => run.turns.map((t) => t.turnNum),
    [run.turns],
  );

  // Track which turns are collapsed (closed). Default: everything open.
  // `solo` overrides the set: when set, only that turn renders open.
  const [collapsed, setCollapsed] = useState<Set<number>>(new Set());
  const [solo, setSolo] = useState<number | null>(null);

  const isOpen = (turnNum: number): boolean => {
    if (solo !== null) return turnNum === solo;
    return !collapsed.has(turnNum);
  };

  const toggleTurn = (turnNum: number) => {
    if (solo !== null) {
      // Solo → click a different turn solos it; click the soloed turn
      // again exits solo mode and expands everything.
      if (solo === turnNum) {
        setSolo(null);
        setCollapsed(new Set());
      } else {
        setSolo(turnNum);
      }
      return;
    }
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(turnNum)) next.delete(turnNum);
      else next.add(turnNum);
      return next;
    });
  };

  const collapseAll = () => {
    setSolo(null);
    setCollapsed(new Set(allTurnNums));
  };
  const expandAll = () => {
    setSolo(null);
    setCollapsed(new Set());
  };
  const soloTurn = (turnNum: number) => {
    setSolo(turnNum);
  };
  const exitSolo = () => {
    setSolo(null);
    setCollapsed(new Set());
  };

  // Rail-driven selection (story 057): reveal the chosen turn — expand it
  // if collapsed, drop out of solo if it hides it, and scroll it into view.
  // Keyed on the run/session too so picking the same turn number in a
  // different run re-scrolls, but live SSE ticks (which don't change these)
  // don't.
  useEffect(() => {
    if (activeTurn == null) return;
    setSolo((s) => (s !== null && s !== activeTurn ? null : s));
    setCollapsed((prev) => {
      if (!prev.has(activeTurn)) return prev;
      const next = new Set(prev);
      next.delete(activeTurn);
      return next;
    });
    document
      .getElementById(`ooda-turn-${activeTurn}`)
      ?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, [activeTurn, run.index, session.id]);

  // Partition items into per-turn groups so we can hide a whole
  // turn's body when collapsed. Turn-divider and turn-end items
  // bookend each group; non-turn items (pre-turn, post-session)
  // would go through unchanged but there aren't any in practice.
  const groups = useMemo(() => groupByTurn(items), [items]);

  const anyCollapsed = solo !== null || collapsed.size > 0;

  return (
    <div className="ooda-thread-body">
      <div className="ooda-turn-controls">
        {solo !== null ? (
          <button onClick={exitSolo} title="Exit focus mode">
            ← exit focus (turn {solo})
          </button>
        ) : anyCollapsed ? (
          <button onClick={expandAll}>Expand all</button>
        ) : (
          <button onClick={collapseAll} disabled={allTurnNums.length === 0}>
            Collapse all
          </button>
        )}
      </div>

      {groups.map((g, gi) => {
        if (g.turnNum === null) {
          return (
            <Fragment key={`pre-${gi}`}>
              {g.items.map((it, i) => (
                <Fragment key={i}>{renderItem(it, pairsById)}</Fragment>
              ))}
            </Fragment>
          );
        }
        const open = isOpen(g.turnNum);
        const isActiveTurn = activeTurn != null && g.turnNum === activeTurn;
        return (
          <div
            key={`t-${g.turnNum}`}
            id={`ooda-turn-${g.turnNum}`}
            className={`turn-group${open ? "" : " collapsed"}${
              isActiveTurn ? " active-turn" : ""
            }`}
          >
            {g.items.map((it, i) => {
              // Render the turn-divider as a clickable element.
              if (it.kind === "turn-divider") {
                return (
                  <button
                    key={i}
                    type="button"
                    className="turn-divider clickable"
                    onClick={() => toggleTurn(it.turnNum)}
                    onDoubleClick={() => soloTurn(it.turnNum)}
                    title="Click to collapse/expand · double-click to focus this turn alone"
                  >
                    <span className="turn-divider-chev">{open ? "▾" : "▸"}</span>
                    <span className="turn-divider-label">
                      Turn {it.turnNum}
                      {it.roundtrips > 1 ? ` · ${it.roundtrips} round-trips` : ""}
                    </span>
                    {it.turnId && (
                      <span
                        className="turn-divider-turnid"
                        title={`marks.turn_id = ${it.turnId}`}
                      >
                        turn:{it.turnId.slice(-6)}
                      </span>
                    )}
                    <span className="turn-divider-rule" />
                    {!open && <span className="turn-divider-hint">(click to expand)</span>}
                  </button>
                );
              }
              if (!open && it.kind !== "turn-end") return null;
              return (
                <Fragment key={i}>
                  {renderItem(it, pairsById, childRunByToolUseId, onJumpToRun)}
                </Fragment>
              );
            })}
          </div>
        );
      })}

      {session.auxiliary.length > 0 && (
        <section className="aux-section">
          <h3>Auxiliary calls ({session.auxiliary.length})</h3>
          <p className="aux-help">
            Quota probes and title-generation requests Claude Code makes
            outside the user-visible OODA loop. Surfaced separately so
            they don't appear as fake turns.
          </p>
          {session.auxiliary.map((aux) => (
            <Block
              key={aux.exchangeId}
              role="unknown"
              label={`AUX · ${aux.kind}`}
              summary={aux.summary}
              size={aux.usage?.output_tokens != null ? `${aux.usage.output_tokens} tok out` : undefined}
              ts={aux.timestamp}
              defaultOpen={false}
              body={
                <pre className="block-pre">
                  {JSON.stringify(
                    {
                      kind: aux.kind,
                      stopReason: aux.stopReason,
                      usage: aux.usage,
                      user: aux.userMessage,
                      assistant: aux.assistant,
                    },
                    null,
                    2,
                  )}
                </pre>
              }
            />
          ))}
        </section>
      )}
    </div>
  );
}

/**
 * Partition the flat thread items into groups bounded by turn-dividers.
 * Items before the first turn-divider go into a leading "null-turn"
 * group (rare, but the function should be robust to it).
 */
function groupByTurn(items: ThreadItem[]): { turnNum: number | null; items: ThreadItem[] }[] {
  const out: { turnNum: number | null; items: ThreadItem[] }[] = [];
  let current: { turnNum: number | null; items: ThreadItem[] } | null = null;
  for (const it of items) {
    if (it.kind === "turn-divider") {
      current = { turnNum: it.turnNum, items: [it] };
      out.push(current);
    } else {
      if (!current) {
        current = { turnNum: null, items: [] };
        out.push(current);
      }
      current.items.push(it);
    }
  }
  return out;
}

function renderItem(
  it: ThreadItem,
  pairsById: Map<string, ExchangePair>,
  childRunByToolUseId: Map<string, AgentRun> = new Map(),
  onJumpToRun?: (runIdx: number) => void,
) {
  switch (it.kind) {
    case "turn-divider":
      // Rendered specially in OodaThread for click-to-collapse. This
      // case is a fallback that shouldn't normally fire.
      return (
        <div className="turn-divider">
          <span className="turn-divider-label">
            Turn {it.turnNum}
            {it.roundtrips > 1 ? ` · ${it.roundtrips} round-trips` : ""}
          </span>
          {it.turnId && (
            <span
              className="turn-divider-turnid"
              title={`marks.turn_id = ${it.turnId}`}
            >
              turn:{it.turnId.slice(-6)}
            </span>
          )}
          <span className="turn-divider-rule" />
        </div>
      );
    case "turn-end":
      return (
        <div className={`turn-end-marker ${stopReasonClass(it.stopReason)}`}>
          <span className="turn-end-rule" />
          <span className="turn-end-label">
            ✓ end of turn {it.turnNum}
            {it.stopReason ? ` · ${it.stopReason}` : ""}
          </span>
          <span className="turn-end-rule" />
        </div>
      );
    case "user":
      return <UserBlock it={it} />;
    case "system":
      return <SystemBlock it={it} />;
    case "headers":
      return (
        <HeadersBlock
          pair={pairsById.get(it.requestId)}
          turnNum={it.turnNum}
          rtIndex={it.rtIndex}
          rtTotal={it.rtTotal}
          ts={it.ts}
          usage={it.usage}
        />
      );
    case "thinking":
      return (
        <Block
          role="thinking"
          label="THINKING"
          summary={firstLine(it.text)}
          size={`${byteLen(it.text)} bytes`}
          ts={it.ts}
          defaultOpen={false}
          body={<pre className="block-pre">{it.text}</pre>}
        />
      );
    case "agent-text":
      return (
        <Block
          role="agent"
          label="AGENT"
          size={`${byteLen(it.text)} bytes`}
          ts={it.ts}
          defaultOpen={true}
          body={<div className="block-text">{it.text}</div>}
        />
      );
    case "tool-use":
      return (
        <ToolBlock
          it={it}
          subAgent={childRunByToolUseId.get(it.toolUseId) ?? null}
          onJumpToRun={onJumpToRun}
        />
      );
    case "tool-cluster":
      return (
        <ToolClusterBlock
          it={it}
          childRunByToolUseId={childRunByToolUseId}
          onJumpToRun={onJumpToRun}
        />
      );
    case "agent-unknown":
      return (
        <Block
          role="unknown"
          label="UNKNOWN"
          ts={it.ts}
          defaultOpen={false}
          body={<pre className="block-pre">{JSON.stringify(it.raw, null, 2)}</pre>}
        />
      );
  }
}

function UserBlock({
  it,
}: {
  it: Extract<ThreadItem, { kind: "user" }>;
}) {
  // One USER block per content block, so system-reminders and
  // tool_results each get their own collapsed row (like the TAP
  // viewer reference). For a single text input, we render one row.
  return (
    <>
      {it.blocks.map((block, i) => (
        <Fragment key={i}>{renderUserBlock(block, it.ts, it.variant)}</Fragment>
      ))}
    </>
  );
}

function SystemBlock({
  it,
}: {
  it: Extract<ThreadItem, { kind: "system" }>;
}) {
  // Render each text block in the request's `system` array as its
  // own SYSTEM row. When `mutated` is true, noodle injected (or
  // otherwise modified) the array — the last block is the one most
  // likely to be ours since AttributionInjector appends. We mark
  // it visually with the "injected" label so the operator can audit
  // the directive at a glance. The other blocks are Claude Code's
  // own system prompts (billing header, persona, instructions).
  const lastIdx = it.blocks.length - 1;
  return (
    <>
      {it.blocks.map((block, i) => {
        if (block.type !== "text") return null;
        const isLikelyInjection = it.mutated && i === lastIdx;
        return (
          <Block
            key={i}
            role="system"
            label={isLikelyInjection ? "SYSTEM (injected)" : "SYSTEM"}
            summary={firstLine(block.text)}
            size={`${byteLen(block.text)} bytes`}
            ts={it.ts}
            defaultOpen={isLikelyInjection}
            body={<div className="block-text">{block.text}</div>}
          />
        );
      })}
    </>
  );
}

function renderUserBlock(
  block: ContentBlock,
  ts: string,
  variant: "input" | "tool-loop",
) {
  const role = variant === "tool-loop" ? "user-loop" : "user";
  if (block.type === "text") {
    const sysReminder = isLikelySystemReminder(block);
    const summary = sysReminder ? "<system-reminder>" : firstLine(block.text);
    return (
      <Block
        role={role}
        label="USER"
        summary={summary}
        size={`${byteLen(block.text)} bytes`}
        ts={ts}
        defaultOpen={!sysReminder}
        body={<div className="block-text">{block.text}</div>}
      />
    );
  }
  if (block.type === "tool_result") {
    const text =
      typeof block.content === "string"
        ? block.content
        : block.content
            .map((b) => (b.type === "text" ? b.text : JSON.stringify(b)))
            .join("\n");
    return (
      <Block
        role={role}
        label={block.is_error ? "TOOL ERROR" : "TOOL RESULT"}
        summary={`for ${block.tool_use_id.slice(0, 14)}…`}
        size={`${byteLen(text)} bytes`}
        ts={ts}
        defaultOpen={false}
        body={<pre className="block-pre">{text}</pre>}
      />
    );
  }
  return (
    <Block
      role={role}
      label="USER"
      summary={`(${block.type})`}
      ts={ts}
      defaultOpen={false}
      body={<pre className="block-pre">{JSON.stringify(block, null, 2)}</pre>}
    />
  );
}

function ToolBlock({
  it,
  subAgent,
  onJumpToRun,
}: {
  it: Extract<ThreadItem, { kind: "tool-use" }>;
  subAgent: AgentRun | null;
  onJumpToRun?: (runIdx: number) => void;
}) {
  const inputStr =
    typeof it.input === "string"
      ? it.input
      : JSON.stringify(it.input, null, 2);
  const summary = oneLineSummary(it.input);
  const resultText =
    it.result && it.result.type === "tool_result"
      ? typeof it.result.content === "string"
        ? it.result.content
        : it.result.content
            .map((b) => (b.type === "text" ? b.text : JSON.stringify(b)))
            .join("\n")
      : null;
  // Short form of the tool_use id so the user can scan-match between
  // the tool_use and its inline result (and a future "no result yet"
  // warning if the pair fails to land).
  const shortId = it.toolUseId.slice(0, 14);
  const cls = classifyTool(it.name);
  const badge = (
    <span
      className={`tool-bucket tool-bucket-${cls.bucket}`}
      title={cls.bucket === "mcp" && cls.mcp ? cls.mcp.tool : cls.bucket}
    >
      {cls.display}
    </span>
  );
  return (
    <Block
      role="tool"
      label={`TOOL ${it.name}`}
      badge={badge}
      summary={`${shortId}… · ${summary}`}
      size={`${byteLen(inputStr)} bytes`}
      ts={it.ts}
      defaultOpen={false}
      body={
        <div className="tool-body">
          <div className="tool-section">
            <div className="block-label">input</div>
            <pre className="block-pre">{inputStr}</pre>
          </div>
          {resultText !== null && (
            <div className={`tool-section${it.isError ? " err" : ""}`}>
              <div className="block-label">
                {it.isError ? "error" : "result"} · {byteLen(resultText)} bytes
              </div>
              <pre className="block-pre">{resultText}</pre>
            </div>
          )}
          {resultText === null && (
            <div className="body-empty">(no result captured)</div>
          )}
          {subAgent && (
            <div className="sub-agent-link">
              <div className="block-label">sub-agent run</div>
              <button
                className="sub-agent-link-btn"
                onClick={() => onJumpToRun?.(subAgent.index)}
                title="Jump to the sub-agent run inside this session"
              >
                ↳ run #{subAgent.index} ·{" "}
                {subAgent.turns.reduce((n, t) => n + t.roundtrips.length, 0)}{" "}
                round-trip
                {subAgent.turns.reduce((n, t) => n + t.roundtrips.length, 0) === 1
                  ? ""
                  : "s"}
                {" · view →"}
              </button>
              {subAgent.spawnedBy?.subagentType && (
                <span className="sub-agent-type">
                  {subAgent.spawnedBy.subagentType}
                </span>
              )}
              {subAgent.spawnedBy?.runInBackground && (
                <span className="sub-agent-type">background</span>
              )}
            </div>
          )}
        </div>
      }
    />
  );
}

function ToolClusterBlock({
  it,
  childRunByToolUseId,
  onJumpToRun,
}: {
  it: Extract<ThreadItem, { kind: "tool-cluster" }>;
  childRunByToolUseId: Map<string, AgentRun>;
  onJumpToRun?: (runIdx: number) => void;
}) {
  const countBadge = (
    <span className="tool-cluster-count">×{it.items.length}</span>
  );
  return (
    <Block
      role="tool"
      label="TOOL CLUSTER"
      badge={countBadge}
      summary={it.summary}
      ts={it.ts}
      defaultOpen={false}
      body={
        <div className="tool-cluster-body">
          {it.items.map((child) => (
            <ToolBlock
              key={child.toolUseId}
              it={child}
              subAgent={childRunByToolUseId.get(child.toolUseId) ?? null}
              onJumpToRun={onJumpToRun}
            />
          ))}
        </div>
      }
    />
  );
}

function firstLine(s: string): string {
  const line = s.split("\n", 1)[0] ?? "";
  return truncate(line, 120);
}

function truncate(s: string, n: number): string {
  return s.length > n ? s.slice(0, n - 1) + "…" : s;
}

function byteLen(s: string): number {
  // UTF-8 byte length; cheap enough.
  return new TextEncoder().encode(s).length;
}

function stopReasonClass(stop?: string): string {
  if (!stop) return "ok";
  if (stop === "end_turn") return "ok";
  if (stop === "max_tokens") return "warn";
  return "ok";
}

function oneLineSummary(input: unknown): string {
  if (input === null || input === undefined) return "";
  if (typeof input === "string") return truncate(input, 90);
  if (typeof input !== "object") return String(input);
  const o = input as Record<string, unknown>;
  for (const key of ["file_path", "path", "command", "query", "pattern", "url"]) {
    const v = o[key];
    if (typeof v === "string") return truncate(v, 90);
  }
  const keys = Object.keys(o);
  return truncate(`{${keys.join(", ")}}`, 90);
}
