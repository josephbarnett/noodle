// OTLP mode — ad-hoc SQL over the embellisher's rollups.db, exposed
// via the viewer's `/api/rollups/{schema,query}` endpoints (V2.1
// backend).
//
// Layout: left sidebar of saved queries + schema column list, right
// pane with a monospace SQL editor and the results table below. Six
// baked-in saved queries demonstrate brain.* / gen_ai.-derivable
// signals; users can edit + re-run any of them.
//
// State: last-edited SQL persists in sessionStorage so a refresh
// (or a port-forward bounce) doesn't lose the query in progress.

import { useEffect, useMemo, useRef, useState } from "react";

interface ColumnInfo {
  name: string;
  type: string;
  notnull: boolean;
  pk: boolean;
}

interface SchemaResponse {
  table: string;
  columns: ColumnInfo[];
}

interface QueryResponse {
  columns: string[];
  rows: unknown[][];
  row_count: number;
  truncated: boolean;
  elapsed_ms: number;
}

interface SavedQuery {
  id: string;
  title: string;
  description: string;
  sql: string;
}

// Ten saved queries demonstrate the brain.* + policy.* substrates
// end-to-end. Edit one and you've got your own — sessionStorage
// carries the last run between refreshes.
const SAVED_QUERIES: SavedQuery[] = [
  {
    id: "compaction-events",
    title: "Compaction events",
    description:
      "Every turn where brain_compaction_detected = 1, newest first. " +
      "The headline brain signal — each row is a moment an agent silently lost history.",
    sql: `SELECT
  timestamp,
  brain_thread_id,
  brain_thread_turn_index AS turn,
  brain_blocks_dropped     AS dropped,
  brain_blocks_added       AS added,
  brain_compaction_directive_kind AS directive_kind,
  brain_estimated_window_tokens   AS peak_tokens,
  context_json
FROM ai_telemetry_v_0_0_2
WHERE brain_compaction_detected = 1
ORDER BY timestamp DESC
LIMIT 50;`,
  },
  {
    id: "brain-thread-leaderboard",
    title: "Brain thread leaderboard",
    description:
      "Threads ranked by silent-context-loss: total compaction events × turns × peak window. " +
      "Use this to spot the longest-running conversations most affected by drift.",
    sql: `SELECT
  brain_thread_id,
  COUNT(*)                              AS turns,
  SUM(brain_compaction_detected)        AS compactions,
  SUM(brain_blocks_dropped)             AS total_dropped,
  MAX(brain_estimated_window_tokens)    AS peak_tokens
FROM ai_telemetry_v_0_0_2
WHERE brain_thread_id IS NOT NULL
GROUP BY brain_thread_id
ORDER BY compactions DESC, turns DESC
LIMIT 20;`,
  },
  {
    id: "directive-vs-detected",
    title: "Compaction directive × detected matrix",
    description:
      "The brain's 2×2 value table as one query. " +
      "Most rows land in 'directive only' (steady-state Claude Code); the rare 'detected' rows are the events that matter.",
    sql: `SELECT
  CASE WHEN brain_compaction_directive_present = 1 THEN 'directive'    ELSE 'no directive' END AS directive,
  CASE WHEN brain_compaction_detected          = 1 THEN 'detected'     ELSE 'no shrink'    END AS detected,
  COUNT(*) AS rows
FROM ai_telemetry_v_0_0_2
WHERE brain_thread_id IS NOT NULL
GROUP BY directive, detected
ORDER BY rows DESC;`,
  },
  {
    id: "provider-mix",
    title: "Provider × model mix",
    description:
      "Where the agent traffic actually goes. Cross-check 'context.tool' (what client) against 'model' (which Anthropic / OpenAI / etc. model).",
    sql: `SELECT
  provider,
  model,
  COUNT(*)                  AS calls,
  SUM(input_tokens)         AS input_tokens,
  SUM(output_tokens)        AS output_tokens,
  ROUND(AVG(latency_ms), 0) AS avg_latency_ms
FROM ai_telemetry_v_0_0_2
GROUP BY provider, model
ORDER BY calls DESC;`,
  },
  {
    id: "anthropic-beta-exposure",
    title: "Anthropic context-management beta exposure",
    description:
      "Which sessions are on the new context-management-2025-06-27 beta? Per-session adoption metric no platform vendor surfaces.",
    sql: `SELECT
  brain_api_context_management_beta AS on_beta,
  COUNT(*)                          AS turns,
  COUNT(DISTINCT session_hash)      AS sessions
FROM ai_telemetry_v_0_0_2
WHERE provider = 'anthropic'
GROUP BY on_beta;`,
  },
  {
    id: "policy-flag-leaderboard",
    title: "Policy flag leaderboard (Watchtower D2)",
    description:
      "Rules ranked by how often they fired. ADR 045 §2.4 observe-first — measure precision before promoting any rule to enforcement (D7). " +
      "If a rule lights up on every turn, it's noisy and not ready to block.",
    sql: `SELECT
  policy_rule,
  COUNT(*)              AS flags,
  ROUND(AVG(policy_risk), 2) AS avg_risk,
  MAX(policy_risk)      AS peak_risk
FROM ai_telemetry_v_0_0_2
WHERE policy_decision = 'flag'
GROUP BY policy_rule
ORDER BY flags DESC;`,
  },
  {
    id: "recent-flagged-turns",
    title: "Recent flagged turns",
    description:
      "Latest 50 flag verdicts. Each row is a moment Watchtower would have surfaced (and at D7 promotion, could have blocked). " +
      "The rationale column names the pattern the classifier matched.",
    sql: `SELECT
  timestamp,
  policy_rule,
  policy_risk,
  policy_rationale,
  policy_surface,
  brain_thread_id,
  brain_thread_turn_index AS turn,
  context_json
FROM ai_telemetry_v_0_0_2
WHERE policy_decision = 'flag'
ORDER BY timestamp DESC
LIMIT 50;`,
  },
  {
    id: "policy-allow-vs-flag",
    title: "Policy decision distribution (allow vs flag)",
    description:
      "Allow rate is the inverse of false-positive risk — high allow + low flag = quiet substrate. " +
      "Use this as the precision proxy before recommending any rule for enforcement promotion.",
    sql: `SELECT
  policy_decision,
  COUNT(*) AS rows,
  ROUND(100.0 * COUNT(*) / SUM(COUNT(*)) OVER (), 1) AS pct
FROM ai_telemetry_v_0_0_2
WHERE policy_decision IS NOT NULL
GROUP BY policy_decision
ORDER BY rows DESC;`,
  },
  {
    id: "policy-threads-with-flags",
    title: "Threads with flag verdicts",
    description:
      "Brain threads that triggered at least one policy flag. Cross-references ADR 045 §brain-as-watchtower-input — " +
      "a thread that's both compacting AND triggering destructive-tool flags is the highest-signal review target.",
    sql: `SELECT
  brain_thread_id,
  COUNT(*)                                  AS total_turns,
  SUM(CASE WHEN policy_decision = 'flag' THEN 1 ELSE 0 END) AS flag_turns,
  SUM(brain_compaction_detected)            AS compactions,
  MAX(policy_risk)                          AS peak_risk
FROM ai_telemetry_v_0_0_2
WHERE brain_thread_id IS NOT NULL
GROUP BY brain_thread_id
HAVING flag_turns > 0
ORDER BY flag_turns DESC, peak_risk DESC
LIMIT 20;`,
  },
  {
    id: "recent-turns",
    title: "Recent turns (last 50)",
    description:
      "The latest 50 rows across every thread. Good first query to confirm the viewer + embellisher are flowing in real time.",
    sql: `SELECT
  timestamp,
  provider,
  model,
  endpoint_path,
  status_code,
  latency_ms,
  input_tokens,
  output_tokens,
  brain_thread_id,
  brain_thread_turn_index,
  brain_compaction_detected,
  policy_decision,
  policy_rule
FROM ai_telemetry_v_0_0_2
ORDER BY timestamp DESC
LIMIT 50;`,
  },
];

const SQL_STATE_KEY = "noodle-otlp-last-sql";
const QUERY_TITLE_KEY = "noodle-otlp-last-title";

export function OtlpMode() {
  const [schema, setSchema] = useState<SchemaResponse | null>(null);
  const [schemaError, setSchemaError] = useState<string | null>(null);
  const [sql, setSql] = useState<string>(() =>
    sessionStorage.getItem(SQL_STATE_KEY) ?? SAVED_QUERIES[0].sql,
  );
  const [currentTitle, setCurrentTitle] = useState<string | null>(() =>
    sessionStorage.getItem(QUERY_TITLE_KEY) ?? SAVED_QUERIES[0].title,
  );
  const [result, setResult] = useState<QueryResponse | null>(null);
  const [queryError, setQueryError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);
  const editorRef = useRef<HTMLTextAreaElement | null>(null);

  // Persist SQL across refreshes — drop-port-forward UX is brittle
  // enough; losing the in-progress query on top would be cruel.
  useEffect(() => {
    sessionStorage.setItem(SQL_STATE_KEY, sql);
  }, [sql]);
  useEffect(() => {
    if (currentTitle !== null) sessionStorage.setItem(QUERY_TITLE_KEY, currentTitle);
  }, [currentTitle]);

  // Load the schema once at mount.
  useEffect(() => {
    let cancelled = false;
    fetch("/api/rollups/schema")
      .then(async (r) => {
        if (!r.ok) throw new Error(`schema: ${r.status} ${await r.text()}`);
        return r.json() as Promise<SchemaResponse>;
      })
      .then((s) => {
        if (!cancelled) setSchema(s);
      })
      .catch((e) => {
        if (!cancelled) setSchemaError(String(e));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const run = async (overrideSql?: string) => {
    const toRun = overrideSql ?? sql;
    setRunning(true);
    setQueryError(null);
    try {
      const r = await fetch("/api/rollups/query", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ sql: toRun }),
      });
      if (!r.ok) {
        const txt = await r.text();
        setQueryError(`${r.status}: ${txt}`);
        setResult(null);
      } else {
        const j = (await r.json()) as QueryResponse;
        setResult(j);
      }
    } catch (e) {
      setQueryError(String(e));
      setResult(null);
    } finally {
      setRunning(false);
    }
  };

  const pickSaved = (q: SavedQuery) => {
    setSql(q.sql);
    setCurrentTitle(q.title);
    setResult(null);
    setQueryError(null);
    editorRef.current?.focus();
  };

  // Cmd/Ctrl+Enter → run query (Honeycomb / Datadog convention).
  const onEditorKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault();
      run();
    } else if (e.key === "Tab") {
      e.preventDefault();
      const ta = e.currentTarget;
      const start = ta.selectionStart;
      const end = ta.selectionEnd;
      const next = `${sql.slice(0, start)}  ${sql.slice(end)}`;
      setSql(next);
      // queueMicrotask keeps the caret in the right spot once React commits.
      queueMicrotask(() => {
        ta.selectionStart = ta.selectionEnd = start + 2;
      });
    }
  };

  const groupedColumns = useMemo(() => groupByFamily(schema?.columns ?? []), [schema]);

  return (
    <div className="otlp-mode">
      <aside className="otlp-sidebar">
        <h2>Saved queries</h2>
        <ul className="otlp-saved">
          {SAVED_QUERIES.map((q) => (
            <li
              key={q.id}
              className={currentTitle === q.title ? "active" : ""}
              onClick={() => pickSaved(q)}
              title={q.description}
            >
              <div className="otlp-saved-title">{q.title}</div>
              <div className="otlp-saved-desc">{q.description}</div>
            </li>
          ))}
        </ul>
        <h2>Schema</h2>
        {schemaError && <div className="otlp-error">{schemaError}</div>}
        {!schemaError && !schema && <div className="otlp-loading">loading…</div>}
        {schema && (
          <div className="otlp-schema">
            <div className="otlp-schema-table">{schema.table}</div>
            {Object.entries(groupedColumns).map(([family, cols]) => (
              <details key={family} open={family === "brain.*" || family === "envelope"}>
                <summary>
                  {family} <span className="otlp-col-count">({cols.length})</span>
                </summary>
                <ul>
                  {cols.map((c) => (
                    <li key={c.name}>
                      <span className="otlp-col-name">{c.name}</span>{" "}
                      <span className="otlp-col-type">{c.type}</span>
                    </li>
                  ))}
                </ul>
              </details>
            ))}
          </div>
        )}
      </aside>
      <section className="otlp-main">
        <div className="otlp-toolbar">
          {currentTitle && <span className="otlp-current-title">{currentTitle}</span>}
          <span className="otlp-spacer" />
          <button
            className="otlp-run"
            onClick={() => run()}
            disabled={running}
            title="Cmd/Ctrl+Enter"
          >
            {running ? "Running…" : "Run"}
          </button>
        </div>
        <textarea
          ref={editorRef}
          className="otlp-editor"
          value={sql}
          spellCheck={false}
          onChange={(e) => setSql(e.target.value)}
          onKeyDown={onEditorKeyDown}
        />
        <div className="otlp-results">
          {queryError && (
            <div className="otlp-error">
              <b>error:</b> {queryError}
            </div>
          )}
          {result && !queryError && <ResultsTable result={result} />}
          {!result && !queryError && (
            <div className="otlp-empty">
              Pick a saved query on the left or write your own — Cmd/Ctrl+Enter runs it.
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

function ResultsTable({ result }: { result: QueryResponse }) {
  return (
    <>
      <div className="otlp-results-meta">
        {result.row_count.toLocaleString()} row{result.row_count === 1 ? "" : "s"}{" "}
        {result.truncated && (
          <span className="otlp-truncated">(truncated at 10,000)</span>
        )}{" "}
        · {result.elapsed_ms} ms
      </div>
      <div className="otlp-table-wrap">
        <table className="otlp-table">
          <thead>
            <tr>
              {result.columns.map((c) => (
                <th key={c}>{c}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {result.rows.map((row, ri) => (
              <tr key={ri}>
                {row.map((cell, ci) => (
                  <td key={ci}>{renderCell(cell)}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}

function renderCell(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

/** Group schema columns into families so the schema panel stays
 *  scannable. `brain_thread_id` → `brain.*`; `client_user_agent` →
 *  `client.*`; etc. Columns without a recognised prefix fall into
 *  `envelope`. */
function groupByFamily(cols: ColumnInfo[]): Record<string, ColumnInfo[]> {
  const families: Record<string, ColumnInfo[]> = {};
  const familyOf = (name: string): string => {
    if (name.startsWith("brain_")) return "brain.*";
    if (name.startsWith("policy_")) return "policy.*";
    if (name.startsWith("client_")) return "client.*";
    if (name.startsWith("agent_")) return "agent.*";
    if (name.startsWith("rate_limit_")) return "rate_limit.*";
    if (name.startsWith("processor_")) return "processor.*";
    if (name === "context_json" || name === "provider_metadata_json") return "context.*";
    if (
      name === "input_tokens" ||
      name === "output_tokens" ||
      name === "estimated_cost_usd" ||
      name === "cost_model_version"
    )
      return "usage.*";
    return "envelope";
  };
  for (const c of cols) {
    const fam = familyOf(c.name);
    if (!families[fam]) families[fam] = [];
    families[fam].push(c);
  }
  return families;
}
