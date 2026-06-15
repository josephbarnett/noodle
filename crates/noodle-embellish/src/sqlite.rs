//! `SQLite` writer for `ai-telemetry` v0.0.2 events.
//!
//! Schema verbatim from ADR 031 §3.1 — every column there exists
//! here, in the same order. A small bookkeeping table
//! `schema_version` records the pinned schema (`ai-telemetry`,
//! version `0.0.2`) so a downstream shipper can refuse to start on
//! drift per ADR 031 §7.
//!
//! ## Concurrency
//!
//! Single-writer model (`refactor-noodle-embellish.md` §7). The
//! writer owns the `rusqlite::Connection` and is `!Send`-only in
//! the sense that callers should not share it across threads; a
//! shipper reads concurrently via a separate connection.

use std::path::Path;

use rusqlite::{Connection, params};
use thiserror::Error;
use ulid::Ulid;

use crate::mapper::TelemetryRow;

/// Schema family this writer ships rows for. Recorded in the
/// `schema_version` table and on every emitted row.
pub const SCHEMA_ID: &str = "ai-telemetry";

/// Pinned schema version. ADR 031 §5 fixes this at v0.0.2; bumps
/// require a migration tool (ADR 031 §8 open question #2 — deferred).
pub const SCHEMA_VERSION: &str = "0.0.2";

/// Table name for the reference target. ADR 031 §3.3 calls for one
/// table per target; future targets land alongside this one.
pub const TARGET_TABLE: &str = "ai_telemetry_v_0_0_2";

#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("opening sqlite database: {0}")]
    Open(#[source] rusqlite::Error),

    #[error("creating schema: {0}")]
    Schema(#[source] rusqlite::Error),

    #[error("inserting row: {0}")]
    Insert(#[source] rusqlite::Error),

    #[error("recording schema version: {0}")]
    Bookkeeping(#[source] rusqlite::Error),

    #[error(
        "schema drift: existing database carries {existing_schema_id} v{existing_schema_version}, \
         this build expects {SCHEMA_ID} v{SCHEMA_VERSION}"
    )]
    SchemaDrift {
        existing_schema_id: String,
        existing_schema_version: String,
    },
}

/// `SQLite` writer for the `ai-telemetry` v0.0.2 target.
pub struct SqliteWriter {
    conn: Connection,
}

impl SqliteWriter {
    /// Open (or create) the `SQLite` database at `path` and ensure the
    /// `ai_telemetry_v_0_0_2` + `schema_version` tables exist.
    ///
    /// Idempotent — calling `open` on an existing valid database is a
    /// no-op on the schema side. Returns [`SqliteError::SchemaDrift`]
    /// when the database already carries a different schema version.
    pub fn open(path: &Path) -> Result<Self, SqliteError> {
        let conn = Connection::open(path).map_err(SqliteError::Open)?;
        Self::from_connection(conn)
    }

    /// Open an in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Self, SqliteError> {
        let conn = Connection::open_in_memory().map_err(SqliteError::Open)?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, SqliteError> {
        let mut writer = Self { conn };
        writer.ensure_schema()?;
        Ok(writer)
    }

    fn ensure_schema(&mut self) -> Result<(), SqliteError> {
        // Bookkeeping table for schema version. Pinned per ADR 031 §7.
        self.conn
            .execute_batch(SCHEMA_VERSION_DDL)
            .map_err(SqliteError::Bookkeeping)?;

        // Drift check before we touch the target table — an existing
        // database with a different schema version means the operator
        // pointed us at a database from an incompatible processor
        // build, and the safe answer is to bail rather than co-mingle.
        let drift: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT schema_id, schema_version FROM schema_version WHERE schema_id = ?1",
                params![SCHEMA_ID],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok();
        if let Some((sid, sver)) = drift {
            if sid != SCHEMA_ID || sver != SCHEMA_VERSION {
                return Err(SqliteError::SchemaDrift {
                    existing_schema_id: sid,
                    existing_schema_version: sver,
                });
            }
        } else {
            // First-time open: record the pinned version.
            self.conn
                .execute(
                    "INSERT INTO schema_version(schema_id, schema_version, recorded_at) \
                     VALUES (?1, ?2, ?3)",
                    params![SCHEMA_ID, SCHEMA_VERSION, current_unix_ms()],
                )
                .map_err(SqliteError::Bookkeeping)?;
        }

        // Target table — CREATE TABLE IF NOT EXISTS keeps the call
        // idempotent across re-opens.
        self.conn
            .execute_batch(AI_TELEMETRY_V_0_0_2_DDL)
            .map_err(SqliteError::Schema)?;

        // ADR 047 rung 1: in-place ADD COLUMN migration for existing
        // dbs that pre-date the brain.* columns. The schema family
        // stays `ai-telemetry` v0.0.2 because these additions are
        // strictly additive: old shippers ignore the new columns,
        // new shippers tolerate NULL.
        ensure_brain_columns(&self.conn)?;
        // ADR 045 §2.5 — same idempotent ALTER TABLE pattern for
        // Watchtower D2's policy.* columns. Additive over v0.0.2.
        ensure_policy_columns(&self.conn)?;
        // ADR 052 §5 — turn_id / role / frame_id / parent_frame_id /
        // depth. Same additive pattern.
        ensure_lineage_columns(&self.conn)?;
        Ok(())
    }

    /// Insert one row into `ai_telemetry_v_0_0_2`.
    ///
    /// **Slice 042 contract:** rows are idempotent on `event_id`.
    /// When the row carries an `event_id` (the proxy's per-round-trip
    /// ULID-ish identifier from `tap.jsonl`), it is used as the
    /// primary key directly — re-running the embellisher over the
    /// same `tap.jsonl` produces zero duplicate rows. When the row
    /// has no `event_id` (synthetic / unit-test paths), a fresh
    /// ULID is minted as a fallback.
    ///
    /// Returns the `event_id` actually written. The underlying
    /// statement is `INSERT OR IGNORE`, so a second call with the
    /// same id silently no-ops at the DB level — the row count
    /// stays correct on re-runs.
    #[allow(clippy::too_many_lines)]
    pub fn insert(&mut self, mut row: TelemetryRow) -> Result<String, SqliteError> {
        if row.event_id.is_empty() {
            row.event_id = Ulid::new().to_string();
        }
        let emitted_at = current_unix_ms();
        let processor_version = crate::PROCESSOR_VERSION;

        self.conn
            .execute(
                INSERT_SQL,
                params![
                    row.event_id,
                    row.schema_id,
                    row.schema_version,
                    row.event_type,
                    row.timestamp,
                    row.request_id,
                    row.provider,
                    row.model,
                    row.endpoint_path,
                    row.endpoint_params_json,
                    i64::from(row.streaming),
                    row.status_code,
                    row.error_type,
                    row.latency_ms,
                    row.input_tokens,
                    row.output_tokens,
                    row.estimated_cost_usd,
                    row.cost_model_version,
                    row.api_key_prefix,
                    row.api_key_type,
                    row.user_id,
                    row.session_id,
                    row.session_hash,
                    row.client_user_agent,
                    row.client_username,
                    row.client_hostname,
                    row.client_app,
                    row.client_lang,
                    row.client_runtime,
                    row.client_runtime_version,
                    row.client_os,
                    row.client_arch,
                    row.client_sdk_name,
                    row.client_sdk_version,
                    row.client_retry_count,
                    row.client_timeout_seconds,
                    row.client_user_name,
                    row.client_department,
                    row.agent_version,
                    row.agent_arch,
                    row.agent_build_date,
                    row.agent_git_sha,
                    row.rate_limit_utilization,
                    row.rate_limit_window_seconds,
                    row.context_json,
                    row.provider_metadata_json,
                    row.correlation_quality.as_str(),
                    emitted_at,
                    processor_version,
                    Option::<i64>::None, // shipped_at — null until shipper marks it
                    row.brain.as_ref().map(|b| b.thread_id.clone()),
                    row.brain.as_ref().map(|b| b.thread_turn_index),
                    row.brain.as_ref().map(|b| i64::from(b.compaction_detected)),
                    row.brain
                        .as_ref()
                        .map(|b| i64::from(b.compaction_directive_present)),
                    row.brain
                        .as_ref()
                        .and_then(|b| b.compaction_directive_kind.clone()),
                    row.brain.as_ref().map(|b| b.blocks_dropped),
                    row.brain.as_ref().map(|b| b.blocks_added),
                    row.brain.as_ref().map(|b| b.estimated_window_tokens),
                    row.brain
                        .as_ref()
                        .map(|b| i64::from(b.api_context_management_beta)),
                    row.policy.as_ref().map(|p| p.decision.as_str().to_owned()),
                    row.policy
                        .as_ref()
                        .and_then(|p| p.mode.map(|m| m.as_str().to_owned())),
                    row.policy.as_ref().map(|p| p.risk),
                    row.policy.as_ref().map(|p| p.rule.clone()),
                    row.policy.as_ref().map(|p| p.rationale.clone()),
                    row.policy.as_ref().map(|p| p.surface.as_str().to_owned()),
                    // ADR 052 §5 frame-tree lineage block (?66-?70).
                    row.turn_id,
                    row.role,
                    row.frame_id,
                    row.parent_frame_id,
                    row.depth,
                ],
            )
            .map_err(SqliteError::Insert)?;
        Ok(row.event_id)
    }

    /// Count rows currently in `ai_telemetry_v_0_0_2`. Used by tests
    /// and the binary's `--verify` mode.
    pub fn row_count(&self) -> Result<i64, SqliteError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM ai_telemetry_v_0_0_2", [], |r| {
                r.get(0)
            })
            .map_err(SqliteError::Insert)
    }

    /// FR5 token rollup **by turn** (ADR 052 §9 invariant 5): Σ tokens for each
    /// turn, summing every sub-agent frame inside it. Every round-trip of a turn
    /// — main agent and all its sub-agents — carries the same `turn_id` (FR3),
    /// so `GROUP BY turn_id` already sums the whole recursion. Side-calls
    /// (`role = 'side_call'`, no turn) are excluded so they never inflate a
    /// turn; their cost is surfaced apart by [`Self::side_call_token_total`].
    pub fn token_rollup_by_turn(&self) -> Result<Vec<(String, i64)>, SqliteError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT turn_id, SUM(input_tokens + output_tokens) AS total \
                 FROM ai_telemetry_v_0_0_2 \
                 WHERE turn_id IS NOT NULL AND role IS NOT 'side_call' \
                 GROUP BY turn_id ORDER BY turn_id",
            )
            .map_err(SqliteError::Insert)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(SqliteError::Insert)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteError::Insert)
    }

    /// FR5 token rollup **by frame** — cost per agent run (`GROUP BY frame_id`).
    pub fn token_rollup_by_frame(&self) -> Result<Vec<(String, i64)>, SqliteError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT frame_id, SUM(input_tokens + output_tokens) AS total \
                 FROM ai_telemetry_v_0_0_2 \
                 WHERE frame_id IS NOT NULL \
                 GROUP BY frame_id ORDER BY frame_id",
            )
            .map_err(SqliteError::Insert)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(SqliteError::Insert)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(SqliteError::Insert)
    }

    /// FR5 side-call token bucket — Σ tokens for `role = 'side_call'` rows,
    /// surfaced separately so they never fold into a turn total.
    pub fn side_call_token_total(&self) -> Result<i64, SqliteError> {
        self.conn
            .query_row(
                "SELECT COALESCE(SUM(input_tokens + output_tokens), 0) \
                 FROM ai_telemetry_v_0_0_2 WHERE role = 'side_call'",
                [],
                |r| r.get(0),
            )
            .map_err(SqliteError::Insert)
    }

    /// Borrow the underlying connection for read-only callers (tests,
    /// the binary's verify path, downstream shippers in-process).
    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Returns the current unix epoch in milliseconds, or `0` if the
/// system clock is before the epoch (which would imply a deeper
/// problem than a `SQLite` write).
fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

const SCHEMA_VERSION_DDL: &str = r"
CREATE TABLE IF NOT EXISTS schema_version (
    schema_id      TEXT     PRIMARY KEY,
    schema_version TEXT     NOT NULL,
    recorded_at    INTEGER  NOT NULL
);
";

// Verbatim from ADR 031 §3.1. Index DDLs appended.
const AI_TELEMETRY_V_0_0_2_DDL: &str = r"
CREATE TABLE IF NOT EXISTS ai_telemetry_v_0_0_2 (
    event_id              TEXT     PRIMARY KEY,
    schema_id             TEXT     NOT NULL,
    schema_version        TEXT     NOT NULL,
    event_type            TEXT     NOT NULL,
    timestamp             INTEGER  NOT NULL,

    request_id            TEXT,
    provider              TEXT     NOT NULL,
    model                 TEXT     NOT NULL,
    endpoint_path         TEXT     NOT NULL,
    endpoint_params_json  TEXT,
    streaming             INTEGER  NOT NULL,
    status_code           INTEGER  NOT NULL,
    error_type            TEXT,
    latency_ms            INTEGER  NOT NULL,

    input_tokens          INTEGER  NOT NULL,
    output_tokens         INTEGER  NOT NULL,
    estimated_cost_usd    REAL,
    cost_model_version    TEXT,

    api_key_prefix        TEXT,
    api_key_type          TEXT,
    user_id               TEXT,
    session_id            TEXT,
    session_hash          TEXT,

    -- ADR 052 §5 marking-detector ids + frame-tree lineage. All
    -- nullable so legacy rows (pre-marking-detector or non-Anthropic
    -- cells without a detector) continue to round-trip cleanly.
    turn_id               TEXT,
    role                  TEXT,
    frame_id              TEXT,
    parent_frame_id       TEXT,
    depth                 INTEGER,

    client_user_agent     TEXT,
    client_username       TEXT,
    client_hostname       TEXT,
    client_app            TEXT,
    client_lang           TEXT,
    client_runtime        TEXT,
    client_runtime_version TEXT,
    client_os             TEXT,
    client_arch           TEXT,
    client_sdk_name       TEXT,
    client_sdk_version    TEXT,
    client_retry_count    INTEGER,
    client_timeout_seconds INTEGER,
    client_user_name      TEXT,
    client_department     TEXT,

    agent_version         TEXT     NOT NULL,
    agent_arch            TEXT     NOT NULL,
    agent_build_date      TEXT,
    agent_git_sha         TEXT,

    rate_limit_utilization     REAL,
    rate_limit_window_seconds  INTEGER,

    context_json          TEXT,
    provider_metadata_json TEXT,

    -- Slice 042 AC #5: per-row correlation provenance stamp.
    correlation_quality   TEXT     NOT NULL,

    processor_emitted_at  INTEGER  NOT NULL,
    processor_version     TEXT     NOT NULL,
    shipped_at            INTEGER,

    -- Slice 043 cursor-on-flag state machine (ADR 022 §3 / story 043).
    -- 'pending'    — fresh row, not yet claimed by a shipper
    -- 'in_flight'  — shipper claimed it; OTLP request in progress
    -- 'delivered'  — collector accepted; shipped_at also set
    -- 'failed'     — last attempt failed; retry_count incremented
    -- 'retry'      — next claim cycle will retry (functionally same as
    --                'pending', kept distinct for ops alerting)
    -- 'poison'     — retry_count >= cap; row parked for manual review
    delivery_status       TEXT     NOT NULL DEFAULT 'pending',
    retry_count           INTEGER  NOT NULL DEFAULT 0,
    last_attempt_at       INTEGER,
    last_attempt_error    TEXT,

    -- ADR 047 rung 1 brain observation columns. All nullable —
    -- populated only when the brain produced an observation for the
    -- pair (typically Anthropic /v1/messages). Booleans stored as
    -- 0/1 integers for SQLite compatibility.
    brain_thread_id                     TEXT,
    brain_thread_turn_index             INTEGER,
    brain_compaction_detected           INTEGER,
    brain_compaction_directive_present  INTEGER,
    brain_compaction_directive_kind     TEXT,
    brain_blocks_dropped                INTEGER,
    brain_blocks_added                  INTEGER,
    brain_estimated_window_tokens       INTEGER,
    brain_api_context_management_beta   INTEGER,

    -- ADR 045 §2.5 Watchtower D2 observe-mode verdict columns. All
    -- nullable — populated only when a classifier scored the pair.
    -- Mode + risk only meaningful for enforcement verbs (block /
    -- redact); rule + rationale identify the classifier; surface
    -- carries 'request' or 'response.tool_use'.
    policy_decision                     TEXT,
    policy_mode                         TEXT,
    policy_risk                         REAL,
    policy_rule                         TEXT,
    policy_rationale                    TEXT,
    policy_surface                      TEXT
);

CREATE INDEX IF NOT EXISTS idx_timestamp     ON ai_telemetry_v_0_0_2 (timestamp);
CREATE INDEX IF NOT EXISTS idx_session_id    ON ai_telemetry_v_0_0_2 (session_id);
CREATE INDEX IF NOT EXISTS idx_api_key       ON ai_telemetry_v_0_0_2 (api_key_prefix);
CREATE INDEX IF NOT EXISTS idx_unshipped     ON ai_telemetry_v_0_0_2 (shipped_at) WHERE shipped_at IS NULL;
-- Slice 043: powers the shipper's claim query.
CREATE INDEX IF NOT EXISTS idx_pending_event ON ai_telemetry_v_0_0_2 (event_id)
    WHERE delivery_status = 'pending' OR delivery_status = 'retry';
";

// INSERT statement matches the column order in CREATE TABLE above.
// `INSERT OR IGNORE` provides AC #4 idempotency: re-running the
// embellisher over the same tap.jsonl is safe — duplicate
// (event_id) inserts no-op rather than erroring.
const INSERT_SQL: &str = r"
INSERT OR IGNORE INTO ai_telemetry_v_0_0_2 (
    event_id, schema_id, schema_version, event_type, timestamp,
    request_id, provider, model, endpoint_path, endpoint_params_json,
    streaming, status_code, error_type, latency_ms,
    input_tokens, output_tokens, estimated_cost_usd, cost_model_version,
    api_key_prefix, api_key_type, user_id, session_id, session_hash,
    client_user_agent, client_username, client_hostname, client_app,
    client_lang, client_runtime, client_runtime_version, client_os,
    client_arch, client_sdk_name, client_sdk_version,
    client_retry_count, client_timeout_seconds,
    client_user_name, client_department,
    agent_version, agent_arch, agent_build_date, agent_git_sha,
    rate_limit_utilization, rate_limit_window_seconds,
    context_json, provider_metadata_json, correlation_quality,
    processor_emitted_at, processor_version, shipped_at,
    brain_thread_id, brain_thread_turn_index, brain_compaction_detected,
    brain_compaction_directive_present, brain_compaction_directive_kind,
    brain_blocks_dropped, brain_blocks_added,
    brain_estimated_window_tokens, brain_api_context_management_beta,
    policy_decision, policy_mode, policy_risk,
    policy_rule, policy_rationale, policy_surface,
    -- ADR 052 §5 frame-tree lineage; ordered last to keep the
    -- existing numeric param indices stable.
    turn_id, role, frame_id, parent_frame_id, depth
) VALUES (
    ?1, ?2, ?3, ?4, ?5,
    ?6, ?7, ?8, ?9, ?10,
    ?11, ?12, ?13, ?14,
    ?15, ?16, ?17, ?18,
    ?19, ?20, ?21, ?22, ?23,
    ?24, ?25, ?26, ?27,
    ?28, ?29, ?30, ?31,
    ?32, ?33, ?34,
    ?35, ?36,
    ?37, ?38,
    ?39, ?40, ?41, ?42,
    ?43, ?44,
    ?45, ?46, ?47,
    ?48, ?49, ?50,
    ?51, ?52, ?53,
    ?54, ?55,
    ?56, ?57,
    ?58, ?59,
    ?60, ?61, ?62,
    ?63, ?64, ?65,
    ?66, ?67, ?68, ?69, ?70
);
";

/// Brain columns added by ADR 047 rung 1, applied additively over
/// existing v0.0.2 schemas via `ALTER TABLE … ADD COLUMN`. Each
/// entry is `(column_name, type_definition)`.
const BRAIN_COLUMNS: &[(&str, &str)] = &[
    ("brain_thread_id", "TEXT"),
    ("brain_thread_turn_index", "INTEGER"),
    ("brain_compaction_detected", "INTEGER"),
    ("brain_compaction_directive_present", "INTEGER"),
    ("brain_compaction_directive_kind", "TEXT"),
    ("brain_blocks_dropped", "INTEGER"),
    ("brain_blocks_added", "INTEGER"),
    ("brain_estimated_window_tokens", "INTEGER"),
    ("brain_api_context_management_beta", "INTEGER"),
];

/// Idempotent in-place migration. Reads `PRAGMA table_info` to see
/// which brain columns the table already carries; `ALTER TABLE …
/// ADD COLUMN` for the rest. Safe on a fresh CREATE TABLE (which
/// already includes them) — every column matches and nothing is
/// added.
fn ensure_brain_columns(conn: &Connection) -> Result<(), SqliteError> {
    ensure_columns(conn, BRAIN_COLUMNS)
}

/// ADR 045 §2.5 — `policy.*` columns Watchtower D2 stamps onto each
/// row. Applied additively over v0.0.2 the same way `BRAIN_COLUMNS`
/// is.
const POLICY_COLUMNS: &[(&str, &str)] = &[
    ("policy_decision", "TEXT"),
    ("policy_mode", "TEXT"),
    ("policy_risk", "REAL"),
    ("policy_rule", "TEXT"),
    ("policy_rationale", "TEXT"),
    ("policy_surface", "TEXT"),
];

/// Idempotent in-place migration for ADR 045 §2.5 policy.* columns.
/// Same shape and tolerance as [`ensure_brain_columns`].
fn ensure_policy_columns(conn: &Connection) -> Result<(), SqliteError> {
    ensure_columns(conn, POLICY_COLUMNS)
}

/// ADR 052 §5 — marking-detector ids + frame-tree lineage. Same
/// additive-over-v0.0.2 pattern as brain.* and policy.*.
const LINEAGE_COLUMNS: &[(&str, &str)] = &[
    ("turn_id", "TEXT"),
    ("role", "TEXT"),
    ("frame_id", "TEXT"),
    ("parent_frame_id", "TEXT"),
    ("depth", "INTEGER"),
];

/// Idempotent in-place migration for ADR 052 §5 lineage columns.
/// Same shape and tolerance as [`ensure_brain_columns`].
fn ensure_lineage_columns(conn: &Connection) -> Result<(), SqliteError> {
    ensure_columns(conn, LINEAGE_COLUMNS)
}

/// Shared scaffold for "add any of these columns that are missing".
/// Used by both brain and policy migrations; one `PRAGMA table_info`
/// scan + N `ALTER TABLE` only for the columns this version of the
/// embellisher introduced.
fn ensure_columns(conn: &Connection, cols: &[(&str, &str)]) -> Result<(), SqliteError> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(ai_telemetry_v_0_0_2)")
        .map_err(SqliteError::Schema)?;
    let existing: std::collections::HashSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(SqliteError::Schema)?
        .collect::<Result<_, _>>()
        .map_err(SqliteError::Schema)?;
    drop(stmt);
    for &(col, ty) in cols {
        if !existing.contains(col) {
            let sql = format!("ALTER TABLE ai_telemetry_v_0_0_2 ADD COLUMN {col} {ty}");
            conn.execute(&sql, []).map_err(SqliteError::Schema)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::TelemetryRow;

    fn empty_row() -> TelemetryRow {
        TelemetryRow {
            event_id: String::new(),
            schema_id: SCHEMA_ID.to_owned(),
            schema_version: SCHEMA_VERSION.to_owned(),
            event_type: "api_call".to_owned(),
            timestamp: 1_716_657_600_000,
            request_id: None,
            provider: "anthropic".to_owned(),
            model: "claude-3-5-sonnet".to_owned(),
            endpoint_path: "/v1/messages".to_owned(),
            endpoint_params_json: None,
            streaming: true,
            status_code: 200,
            error_type: None,
            latency_ms: 100,
            input_tokens: 10,
            output_tokens: 20,
            estimated_cost_usd: None,
            cost_model_version: None,
            api_key_prefix: Some("sk-ant-api0".to_owned()),
            api_key_type: Some("api_key".to_owned()),
            user_id: None,
            session_id: Some("sess_1".to_owned()),
            session_hash: Some("abc123".to_owned()),
            turn_id: None,
            role: None,
            frame_id: None,
            parent_frame_id: None,
            depth: None,
            client_user_agent: Some("claude-cli/1.0".to_owned()),
            client_username: None,
            client_hostname: Some("h".to_owned()),
            client_app: None,
            client_lang: Some("js".to_owned()),
            client_runtime: None,
            client_runtime_version: None,
            client_os: None,
            client_arch: None,
            client_sdk_name: None,
            client_sdk_version: None,
            client_retry_count: None,
            client_timeout_seconds: None,
            client_user_name: None,
            client_department: None,
            agent_version: "0.0.1".to_owned(),
            agent_arch: "aarch64".to_owned(),
            agent_build_date: None,
            agent_git_sha: None,
            rate_limit_utilization: None,
            rate_limit_window_seconds: None,
            context_json: None,
            provider_metadata_json: Some(r#"{"provider":"anthropic"}"#.to_owned()),
            brain: None,
            policy: None,
            correlation_quality: crate::mapper::CorrelationQuality::WireOnly,
        }
    }

    #[test]
    fn open_in_memory_creates_schema() {
        let writer = SqliteWriter::open_in_memory().unwrap();
        assert_eq!(writer.row_count().unwrap(), 0);
    }

    #[test]
    fn schema_version_row_is_pinned() {
        let writer = SqliteWriter::open_in_memory().unwrap();
        let (sid, ver): (String, String) = writer
            .conn()
            .query_row(
                "SELECT schema_id, schema_version FROM schema_version",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(sid, SCHEMA_ID);
        assert_eq!(ver, SCHEMA_VERSION);
    }

    #[test]
    fn insert_round_trips_a_row() {
        let mut writer = SqliteWriter::open_in_memory().unwrap();
        let event_id = writer.insert(empty_row()).unwrap();
        assert!(!event_id.is_empty());
        assert_eq!(writer.row_count().unwrap(), 1);

        let (provider, status, in_t, out_t): (String, i64, i64, i64) = writer
            .conn()
            .query_row(
                "SELECT provider, status_code, input_tokens, output_tokens FROM ai_telemetry_v_0_0_2 WHERE event_id = ?1",
                params![event_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(provider, "anthropic");
        assert_eq!(status, 200);
        assert_eq!(in_t, 10);
        assert_eq!(out_t, 20);
    }

    #[test]
    fn token_rollup_by_turn_sums_subagents_and_excludes_side_calls() {
        // ADR 052 FR5 / §9 invariant 5, parent-parallel-subagents shape: one
        // turn T1 = ROOT main + 3 sub-agent frames, interleaved with two
        // side-calls (quota, monitor) that carry no turn.
        let mut writer = SqliteWriter::open_in_memory().unwrap();
        let mut frame = |turn: Option<&str>, role: &str, fid: Option<&str>, tokens: i64| {
            let mut r = empty_row();
            r.input_tokens = tokens;
            r.output_tokens = 0;
            r.turn_id = turn.map(str::to_owned);
            r.role = Some(role.to_owned());
            r.frame_id = fid.map(str::to_owned);
            writer.insert(r).unwrap();
        };
        frame(Some("T1"), "main", Some("ROOT"), 110);
        frame(Some("T1"), "sub_agent", Some("F1"), 220);
        frame(Some("T1"), "sub_agent", Some("F2"), 330);
        frame(Some("T1"), "sub_agent", Some("F3"), 440);
        frame(None, "side_call", None, 6); // quota
        frame(None, "side_call", None, 9); // security monitor

        // by-turn sums the whole recursion (110+220+330+440), side-calls out.
        assert_eq!(
            writer.token_rollup_by_turn().unwrap(),
            vec![("T1".to_owned(), 1100)]
        );
        // by-frame buckets each agent run on its own id (side-calls have no
        // frame_id, so they are absent here).
        assert_eq!(
            writer.token_rollup_by_frame().unwrap(),
            vec![
                ("F1".to_owned(), 220),
                ("F2".to_owned(), 330),
                ("F3".to_owned(), 440),
                ("ROOT".to_owned(), 110),
            ]
        );
        // side-call cost is surfaced apart and never folded into the turn.
        assert_eq!(writer.side_call_token_total().unwrap(), 15);
    }

    #[test]
    fn reopen_existing_db_is_idempotent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = SqliteWriter::open(tmp.path()).unwrap();
            writer.insert(empty_row()).unwrap();
        }
        // Reopen — schema already exists, row count survives.
        let writer = SqliteWriter::open(tmp.path()).unwrap();
        assert_eq!(writer.row_count().unwrap(), 1);
    }

    #[test]
    fn schema_drift_is_rejected() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            // Manually create a database carrying a different schema
            // version — the writer should refuse to take it.
            let conn = Connection::open(tmp.path()).unwrap();
            conn.execute_batch(SCHEMA_VERSION_DDL).unwrap();
            conn.execute(
                "INSERT INTO schema_version(schema_id, schema_version, recorded_at) VALUES (?1, ?2, ?3)",
                params![SCHEMA_ID, "9.9.9", 0i64],
            ).unwrap();
        }
        let Err(err) = SqliteWriter::open(tmp.path()) else {
            panic!("expected SchemaDrift, got Ok")
        };
        match err {
            SqliteError::SchemaDrift {
                existing_schema_version,
                ..
            } => {
                assert_eq!(existing_schema_version, "9.9.9");
            }
            other => panic!("expected SchemaDrift, got {other:?}"),
        }
    }

    #[test]
    fn unshipped_index_powers_a_fast_filter_query() {
        let mut writer = SqliteWriter::open_in_memory().unwrap();
        writer.insert(empty_row()).unwrap();
        writer.insert(empty_row()).unwrap();
        let count: i64 = writer
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ai_telemetry_v_0_0_2 WHERE shipped_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
}
