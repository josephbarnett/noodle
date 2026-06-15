// Before/After comparison for a single direction's body. Surfaces the
// exact bytes noodle touched:
//
//   Request  → "Injection · Before" (client) / "After" (forwarded upstream)
//   Response → "Extraction · Before" (upstream raw) / "After" (forwarded to client)
//
// View modes:
//   - Side: line-aligned side-by-side. Both columns share ONE vertical
//     scroll; diff highlights inline (added rows green on the right,
//     removed rows red on the left, unchanged rows neutral).
//   - Diff: unified line diff.
//   - Before: just the pre-mutation body (raw, no diff).
//   - After: just the post-mutation body (raw, no diff).
//
// When `before` and `after` are byte-equal we show "no change" instead
// of empty panels — the row didn't actually exercise injection/extraction.

import { useMemo, useState } from "react";
import { BodyView } from "./BodyView";

type ViewMode = "side" | "diff" | "before" | "after";

interface Props {
  kind: "request" | "response";
  before: unknown;
  after: unknown;
}

export function BeforeAfterPanel({ kind, before, after }: Props) {
  const [view, setView] = useState<ViewMode>("side");
  const action = kind === "request" ? "Injection" : "Extraction";
  const beforeLabel =
    kind === "request" ? "Before (client sent)" : "Before (upstream raw)";
  const afterLabel =
    kind === "request"
      ? "After (forwarded upstream)"
      : "After (forwarded to client)";

  const beforeText = toText(before);
  const afterText = toText(after);
  const unchanged = beforeText !== null && beforeText === afterText;

  return (
    <div className={`before-after-panel ${kind}`}>
      <header className="before-after-head">
        <h4 className="body-label">
          {action} · Before / After
          {unchanged && (
            <span className="before-after-unchanged" title="bytes are byte-identical">
              · no change
            </span>
          )}
        </h4>
        <div className="before-after-toggle" role="tablist">
          <ToggleButton current={view} mode="side" set={setView} label="Side" />
          <ToggleButton current={view} mode="diff" set={setView} label="Diff" />
          <ToggleButton current={view} mode="before" set={setView} label="Before" />
          <ToggleButton current={view} mode="after" set={setView} label="After" />
        </div>
      </header>

      {view === "side" && (
        <AlignedSideView
          beforeLabel={beforeLabel}
          afterLabel={afterLabel}
          beforeText={beforeText ?? ""}
          afterText={afterText ?? ""}
          unchanged={unchanged}
        />
      )}
      {view === "diff" && (
        <DiffView
          beforeText={beforeText ?? ""}
          afterText={afterText ?? ""}
          unchanged={unchanged}
        />
      )}
      {view === "before" && <BodyView body={before} label={`${action.toLowerCase()} before`} />}
      {view === "after" && <BodyView body={after} label={`${action.toLowerCase()} after`} />}
    </div>
  );
}

function ToggleButton({
  current,
  mode,
  set,
  label,
}: {
  current: ViewMode;
  mode: ViewMode;
  set: (m: ViewMode) => void;
  label: string;
}) {
  const active = current === mode;
  return (
    <button
      type="button"
      className={`before-after-tab${active ? " active" : ""}`}
      onClick={() => set(mode)}
      aria-pressed={active}
    >
      {label}
    </button>
  );
}

/** Line-aligned side-by-side view: every diff op produces one
 *  row across BOTH columns. `del` ops fill the after column with a
 *  visually-grayed blank; `add` ops fill the before column with the
 *  same. `ctx` rows show identical text on both sides. The whole
 *  table sits inside one scroll container so both columns scroll
 *  in lock-step. */
function AlignedSideView({
  beforeLabel,
  afterLabel,
  beforeText,
  afterText,
  unchanged,
}: {
  beforeLabel: string;
  afterLabel: string;
  beforeText: string;
  afterText: string;
  unchanged: boolean;
}) {
  const rows = useMemo(
    () => alignDiff(diffLines(beforeText, afterText)),
    [beforeText, afterText],
  );

  return (
    <div className="before-after-aligned">
      <div className="before-after-aligned-heads">
        <div className="before-after-col-label">{beforeLabel}</div>
        <div className="before-after-col-label">{afterLabel}</div>
      </div>
      {unchanged ? (
        <div className="body-empty">no change — proxy did not mutate this body</div>
      ) : (
        <div className="before-after-aligned-scroll">
          <table className="before-after-aligned-table">
            <tbody>
              {rows.map((r, i) => (
                <tr key={i}>
                  <td className={`bal-cell bal-${r.beforeKind}`}>{r.beforeText}</td>
                  <td className={`bal-cell bal-${r.afterKind}`}>{r.afterText}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function DiffView({
  beforeText,
  afterText,
  unchanged,
}: {
  beforeText: string;
  afterText: string;
  unchanged: boolean;
}) {
  if (unchanged) {
    return (
      <div className="before-after-diff body-empty">
        no change — proxy did not mutate this body
      </div>
    );
  }
  const ops = diffLines(beforeText, afterText);
  return (
    <pre className="before-after-diff">
      {ops.map((op, i) => (
        <span key={i} className={`diff-line diff-${op.kind}`}>
          {op.kind === "add" ? "+ " : op.kind === "del" ? "- " : "  "}
          {op.line}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}

interface DiffOp {
  kind: "add" | "del" | "ctx";
  line: string;
}

interface AlignedRow {
  beforeText: string;
  beforeKind: "ctx" | "del" | "blank";
  afterText: string;
  afterKind: "ctx" | "add" | "blank";
}

/** Turn a list of diff ops into row-aligned (before, after) pairs.
 *  Adjacent `del`+`add` ops pair up so a single-line change shows
 *  removed on the left and added on the right — the classic
 *  side-by-side diff alignment. */
function alignDiff(ops: DiffOp[]): AlignedRow[] {
  const out: AlignedRow[] = [];
  let i = 0;
  while (i < ops.length) {
    const op = ops[i];
    if (op.kind === "ctx") {
      out.push({
        beforeText: op.line,
        beforeKind: "ctx",
        afterText: op.line,
        afterKind: "ctx",
      });
      i++;
      continue;
    }
    // Pair adjacent del→add (or add→del) into one row so the changed
    // line shows side-by-side. Multiple deletes / adds in a row still
    // pair greedily; any tail fills with blank on the other side.
    if (op.kind === "del") {
      const next = ops[i + 1];
      if (next && next.kind === "add") {
        out.push({
          beforeText: op.line,
          beforeKind: "del",
          afterText: next.line,
          afterKind: "add",
        });
        i += 2;
        continue;
      }
      out.push({
        beforeText: op.line,
        beforeKind: "del",
        afterText: "",
        afterKind: "blank",
      });
      i++;
      continue;
    }
    // op.kind === "add"
    out.push({
      beforeText: "",
      beforeKind: "blank",
      afterText: op.line,
      afterKind: "add",
    });
    i++;
  }
  return out;
}

/** Plain LCS line diff. Performance is fine for body sizes the viewer
 *  shows; if a body is megabytes the React render will struggle
 *  before the diff does. */
function diffLines(a: string, b: string): DiffOp[] {
  const aLines = a.split("\n");
  const bLines = b.split("\n");
  const n = aLines.length;
  const m = bLines.length;
  if (n * m > 1_000_000) {
    return [
      { kind: "del", line: `(diff too large — ${n} vs ${m} lines, showing tails)` },
      ...aLines.slice(-20).map((l) => ({ kind: "del" as const, line: l })),
      { kind: "ctx", line: "" },
      ...bLines.slice(-20).map((l) => ({ kind: "add" as const, line: l })),
    ];
  }
  const dp: number[][] = Array.from({ length: n + 1 }, () =>
    new Array(m + 1).fill(0),
  );
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = aLines[i] === bLines[j]
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out: DiffOp[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (aLines[i] === bLines[j]) {
      out.push({ kind: "ctx", line: aLines[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ kind: "del", line: aLines[i] });
      i++;
    } else {
      out.push({ kind: "add", line: bLines[j] });
      j++;
    }
  }
  while (i < n) {
    out.push({ kind: "del", line: aLines[i++] });
  }
  while (j < m) {
    out.push({ kind: "add", line: bLines[j++] });
  }
  return out;
}

function toText(body: unknown): string | null {
  if (body === null || body === undefined) return null;
  if (typeof body === "string") return body;
  try {
    return JSON.stringify(body, null, 2);
  } catch {
    return null;
  }
}
