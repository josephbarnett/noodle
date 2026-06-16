//! Cursor-on-flag state machine over the `ai_telemetry_v_0_0_2` table.
//!
//! The shipper claims a batch of `'pending'` rows by atomically
//! flipping them to `'in_flight'`, exports each, then commits the
//! result back: `'delivered'` on success, `'retry'` (or `'poison'`
//! past the cap) on failure. A `recover_in_flight` call at startup
//! resets any rows left `'in_flight'` by a crashed prior process.
//!
//! All mutations are wrapped in a `BEGIN EXCLUSIVE` transaction so a
//! concurrent shipper process (rare but allowed by ADR 022) cannot
//! double-claim the same rows.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum CursorError {
    #[error("opening rollups db: {0}")]
    Open(#[source] rusqlite::Error),

    #[error("claiming batch: {0}")]
    Claim(#[source] rusqlite::Error),

    #[error("recovering in_flight rows: {0}")]
    Recover(#[source] rusqlite::Error),

    #[error("committing delivery: {0}")]
    Commit(#[source] rusqlite::Error),
}

/// `delivery_status` values per the slice 043 schema additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Pending,
    InFlight,
    Delivered,
    Retry,
    Poison,
}

impl DeliveryStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InFlight => "in_flight",
            Self::Delivered => "delivered",
            Self::Retry => "retry",
            Self::Poison => "poison",
        }
    }
}

/// One row's worth of payload the shipper needs to build an OTLP
/// record. The cursor selects every column the mapper consumes; the
/// shipper passes this struct to [`crate::mapping::row_to_otlp_log`].
#[derive(Debug, Clone)]
pub struct RollupsRow {
    pub event_id: String,
    pub schema_id: String,
    pub schema_version: String,
    pub event_type: String,
    pub timestamp: i64,

    pub provider: String,
    pub model: String,
    pub endpoint_path: String,
    pub streaming: bool,
    pub status_code: i64,
    pub error_type: Option<String>,
    pub latency_ms: i64,

    pub input_tokens: i64,
    pub output_tokens: i64,

    pub api_key_prefix: Option<String>,
    pub api_key_type: Option<String>,
    pub session_id: Option<String>,
    pub session_hash: Option<String>,

    pub client_user_agent: Option<String>,

    pub agent_version: String,
    pub agent_arch: String,

    pub context_json: Option<String>,
    pub provider_metadata_json: Option<String>,
    pub correlation_quality: String,

    pub retry_count: i64,

    // ADR 047 rung 1 brain observation columns. All `Option` —
    // populated only for pairs the brain observed (typically
    // Anthropic `/v1/messages`).
    pub brain_thread_id: Option<String>,
    pub brain_thread_turn_index: Option<i64>,
    pub brain_compaction_detected: Option<bool>,
    pub brain_compaction_directive_present: Option<bool>,
    pub brain_compaction_directive_kind: Option<String>,
    pub brain_blocks_dropped: Option<i64>,
    pub brain_blocks_added: Option<i64>,
    pub brain_estimated_window_tokens: Option<i64>,
    pub brain_api_context_management_beta: Option<bool>,

    // ADR 056 context-weight columns. All `Option` — populated only
    // when the response carried a usage block. Facts only; cost ratios
    // and dollars are derived at the surface from these.
    pub context_input_tokens: Option<i64>,
    pub context_cache_read_tokens: Option<i64>,
    pub context_cache_creation_tokens: Option<i64>,
    pub context_output_tokens: Option<i64>,
    pub context_system_bytes: Option<i64>,
    pub context_tools_bytes: Option<i64>,
    pub context_tools_count: Option<i64>,
    pub context_preamble_bytes: Option<i64>,

    // ADR 045 §2.5 Watchtower D2 observe-mode verdict columns. All
    // `Option` — populated only when the embellisher's classifier
    // scored the pair.
    pub policy_decision: Option<String>,
    pub policy_mode: Option<String>,
    pub policy_risk: Option<f64>,
    pub policy_rule: Option<String>,
    pub policy_rationale: Option<String>,
    pub policy_surface: Option<String>,

    // ADR 052 §5 — marking-detector ids + frame-tree lineage. All
    // `Option` — populated when the marking detector ran.
    // `parent_frame_id`/`depth` describe the node's place in the run
    // frame tree; `role` is `main` | `sub_agent` | `side_call`.
    pub turn_id: Option<String>,
    pub role: Option<String>,
    pub frame_id: Option<String>,
    pub parent_frame_id: Option<String>,
    pub depth: Option<i64>,
}

/// A claimed batch of rows now in `'in_flight'`. Returned by
/// [`RollupsCursor::claim_batch`] so the caller can map + export each
/// row, then call [`RollupsCursor::ack_delivered`] /
/// [`RollupsCursor::ack_failed`] with the per-row outcome.
#[derive(Debug, Clone)]
pub struct ClaimedBatch {
    pub rows: Vec<RollupsRow>,
}

/// Cursor over the rollups table. Owns the connection; not `Send`
/// because `rusqlite::Connection` isn't `Sync` and we don't want to
/// promise multi-threaded use we can't deliver.
pub struct RollupsCursor {
    conn: Connection,
    max_retries: u32,
}

impl RollupsCursor {
    /// Open the rollups database read-write. The shipper expects
    /// `noodle-embellish` has already created the schema; opening a
    /// non-existent file is an error.
    pub fn open(path: &Path, max_retries: u32) -> Result<Self, CursorError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(CursorError::Open)?;
        // WAL mode so the shipper and `noodle-embellish` can write
        // concurrently — `embellish` appends pending rows; the
        // shipper updates delivery_status on existing rows.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(CursorError::Open)?;
        conn.pragma_update(None, "busy_timeout", 5_000_i64)
            .map_err(CursorError::Open)?;
        Ok(Self { conn, max_retries })
    }

    #[cfg(test)]
    #[must_use]
    pub fn from_connection(conn: Connection, max_retries: u32) -> Self {
        Self { conn, max_retries }
    }

    /// Reset any rows left `'in_flight'` by a prior process back to
    /// `'pending'`. Called at startup; the next claim cycle picks
    /// them up again. At-least-once semantics: a row whose export
    /// crashed mid-flight will be re-sent; the collector dedupes on
    /// `event_id`.
    pub fn recover_in_flight(&mut self) -> Result<usize, CursorError> {
        let n = self
            .conn
            .execute(
                "UPDATE ai_telemetry_v_0_0_2
                 SET delivery_status = 'pending'
                 WHERE delivery_status = 'in_flight'",
                [],
            )
            .map_err(CursorError::Recover)?;
        if n > 0 {
            warn!(
                target: "noodle::shipper::cursor",
                recovered = n,
                "reset stale in_flight rows to pending (prior shipper crash?)"
            );
        }
        Ok(n)
    }

    /// Count rows currently in each delivery state. Used by the
    /// binary's status output and by tests.
    pub fn counts(&self) -> Result<DeliveryCounts, CursorError> {
        let row = self
            .conn
            .query_row(
                "SELECT
                    SUM(CASE WHEN delivery_status='pending'   THEN 1 ELSE 0 END),
                    SUM(CASE WHEN delivery_status='in_flight' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN delivery_status='delivered' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN delivery_status='retry'     THEN 1 ELSE 0 END),
                    SUM(CASE WHEN delivery_status='poison'    THEN 1 ELSE 0 END)
                 FROM ai_telemetry_v_0_0_2",
                [],
                |r| {
                    Ok(DeliveryCounts {
                        pending: u64::try_from(r.get::<_, Option<i64>>(0)?.unwrap_or(0))
                            .unwrap_or(0),
                        in_flight: u64::try_from(r.get::<_, Option<i64>>(1)?.unwrap_or(0))
                            .unwrap_or(0),
                        delivered: u64::try_from(r.get::<_, Option<i64>>(2)?.unwrap_or(0))
                            .unwrap_or(0),
                        retry: u64::try_from(r.get::<_, Option<i64>>(3)?.unwrap_or(0)).unwrap_or(0),
                        poison: u64::try_from(r.get::<_, Option<i64>>(4)?.unwrap_or(0))
                            .unwrap_or(0),
                    })
                },
            )
            .map_err(CursorError::Claim)?;
        Ok(row)
    }

    /// Claim up to `limit` pending/retry rows, flipping them to
    /// `'in_flight'` atomically and returning the row payload to the
    /// caller. An empty batch is normal (table fully drained).
    #[allow(clippy::too_many_lines)] // SELECT + row mapper grew with the brain_* columns (ADR 047 §2.4)
    pub fn claim_batch(&mut self, limit: usize) -> Result<ClaimedBatch, CursorError> {
        let tx = self.conn.transaction().map_err(CursorError::Claim)?;
        // First select the event_ids to claim.
        let mut stmt = tx
            .prepare(
                "SELECT event_id FROM ai_telemetry_v_0_0_2
                 WHERE delivery_status IN ('pending', 'retry')
                 ORDER BY event_id
                 LIMIT ?1",
            )
            .map_err(CursorError::Claim)?;
        let mut ids: Vec<String> = Vec::new();
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        for row in stmt
            .query_map(params![limit_i64], |r| r.get::<_, String>(0))
            .map_err(CursorError::Claim)?
        {
            ids.push(row.map_err(CursorError::Claim)?);
        }
        drop(stmt);
        if ids.is_empty() {
            tx.commit().map_err(CursorError::Claim)?;
            return Ok(ClaimedBatch { rows: Vec::new() });
        }

        // Flip to in_flight in one statement.
        let placeholders = ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let update_sql = format!(
            "UPDATE ai_telemetry_v_0_0_2
             SET delivery_status = 'in_flight'
             WHERE event_id IN ({placeholders})"
        );
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        tx.execute(&update_sql, params_vec.as_slice())
            .map_err(CursorError::Claim)?;

        // Read the full row data for each claimed id.
        let select_sql = format!(
            "SELECT event_id, schema_id, schema_version, event_type, timestamp,
                    provider, model, endpoint_path, streaming, status_code, error_type, latency_ms,
                    input_tokens, output_tokens,
                    api_key_prefix, api_key_type, session_id, session_hash,
                    client_user_agent,
                    agent_version, agent_arch,
                    context_json, provider_metadata_json, correlation_quality,
                    retry_count,
                    brain_thread_id, brain_thread_turn_index, brain_compaction_detected,
                    brain_compaction_directive_present, brain_compaction_directive_kind,
                    brain_blocks_dropped, brain_blocks_added,
                    brain_estimated_window_tokens, brain_api_context_management_beta,
                    policy_decision, policy_mode, policy_risk,
                    policy_rule, policy_rationale, policy_surface,
                    turn_id, role, frame_id, parent_frame_id, depth,
                    context_input_tokens, context_cache_read_tokens,
                    context_cache_creation_tokens, context_output_tokens,
                    context_system_bytes, context_tools_bytes,
                    context_tools_count, context_preamble_bytes
             FROM ai_telemetry_v_0_0_2
             WHERE event_id IN ({placeholders})
             ORDER BY event_id"
        );
        let mut stmt = tx.prepare(&select_sql).map_err(CursorError::Claim)?;
        let mut rows = Vec::with_capacity(ids.len());
        let params_vec: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let iter = stmt
            .query_map(params_vec.as_slice(), |r| {
                Ok(RollupsRow {
                    event_id: r.get(0)?,
                    schema_id: r.get(1)?,
                    schema_version: r.get(2)?,
                    event_type: r.get(3)?,
                    timestamp: r.get(4)?,
                    provider: r.get(5)?,
                    model: r.get(6)?,
                    endpoint_path: r.get(7)?,
                    streaming: r.get::<_, i64>(8)? != 0,
                    status_code: r.get(9)?,
                    error_type: r.get(10)?,
                    latency_ms: r.get(11)?,
                    input_tokens: r.get(12)?,
                    output_tokens: r.get(13)?,
                    api_key_prefix: r.get(14)?,
                    api_key_type: r.get(15)?,
                    session_id: r.get(16)?,
                    session_hash: r.get(17)?,
                    client_user_agent: r.get(18)?,
                    agent_version: r.get(19)?,
                    agent_arch: r.get(20)?,
                    context_json: r.get(21)?,
                    provider_metadata_json: r.get(22)?,
                    correlation_quality: r.get(23)?,
                    retry_count: r.get(24)?,
                    brain_thread_id: r.get(25)?,
                    brain_thread_turn_index: r.get(26)?,
                    brain_compaction_detected: r.get::<_, Option<i64>>(27)?.map(|n| n != 0),
                    brain_compaction_directive_present: r
                        .get::<_, Option<i64>>(28)?
                        .map(|n| n != 0),
                    brain_compaction_directive_kind: r.get(29)?,
                    brain_blocks_dropped: r.get(30)?,
                    brain_blocks_added: r.get(31)?,
                    brain_estimated_window_tokens: r.get(32)?,
                    brain_api_context_management_beta: r.get::<_, Option<i64>>(33)?.map(|n| n != 0),
                    policy_decision: r.get(34)?,
                    policy_mode: r.get(35)?,
                    policy_risk: r.get(36)?,
                    policy_rule: r.get(37)?,
                    policy_rationale: r.get(38)?,
                    policy_surface: r.get(39)?,
                    turn_id: r.get(40)?,
                    role: r.get(41)?,
                    frame_id: r.get(42)?,
                    parent_frame_id: r.get(43)?,
                    depth: r.get(44)?,
                    context_input_tokens: r.get(45)?,
                    context_cache_read_tokens: r.get(46)?,
                    context_cache_creation_tokens: r.get(47)?,
                    context_output_tokens: r.get(48)?,
                    context_system_bytes: r.get(49)?,
                    context_tools_bytes: r.get(50)?,
                    context_tools_count: r.get(51)?,
                    context_preamble_bytes: r.get(52)?,
                })
            })
            .map_err(CursorError::Claim)?;
        for row in iter {
            rows.push(row.map_err(CursorError::Claim)?);
        }
        drop(stmt);

        tx.commit().map_err(CursorError::Claim)?;
        debug!(target: "noodle::shipper::cursor", claimed = rows.len(), "claimed batch");
        Ok(ClaimedBatch { rows })
    }

    /// Mark a set of `event_id`s as `'delivered'` and stamp
    /// `shipped_at = now`.
    pub fn ack_delivered(&mut self, event_ids: &[String]) -> Result<(), CursorError> {
        if event_ids.is_empty() {
            return Ok(());
        }
        let now = current_unix_ms();
        let tx = self.conn.transaction().map_err(CursorError::Commit)?;
        let placeholders = (0..event_ids.len())
            .map(|i| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE ai_telemetry_v_0_0_2
             SET delivery_status = 'delivered',
                 shipped_at = ?1
             WHERE event_id IN ({placeholders})"
        );
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = vec![&now as &dyn rusqlite::ToSql];
        for id in event_ids {
            params_vec.push(id as &dyn rusqlite::ToSql);
        }
        tx.execute(&sql, params_vec.as_slice())
            .map_err(CursorError::Commit)?;
        tx.commit().map_err(CursorError::Commit)?;
        Ok(())
    }

    /// Mark a set of `event_id`s as failed — increment `retry_count`,
    /// record the error, transition to `'retry'` or `'poison'`
    /// depending on the cap.
    pub fn ack_failed(&mut self, event_ids: &[String], error: &str) -> Result<(), CursorError> {
        if event_ids.is_empty() {
            return Ok(());
        }
        let now = current_unix_ms();
        let cap = i64::from(self.max_retries);
        let tx = self.conn.transaction().map_err(CursorError::Commit)?;
        let placeholders = (0..event_ids.len())
            .map(|i| format!("?{}", i + 4))
            .collect::<Vec<_>>()
            .join(",");
        // Single statement: increment retry_count + flip to retry or
        // poison based on the new value vs the cap.
        let sql = format!(
            "UPDATE ai_telemetry_v_0_0_2
             SET retry_count = retry_count + 1,
                 last_attempt_at = ?1,
                 last_attempt_error = ?2,
                 delivery_status = CASE
                     WHEN retry_count + 1 >= ?3 THEN 'poison'
                     ELSE 'retry'
                 END
             WHERE event_id IN ({placeholders})"
        );
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = vec![
            &now as &dyn rusqlite::ToSql,
            &error as &dyn rusqlite::ToSql,
            &cap as &dyn rusqlite::ToSql,
        ];
        for id in event_ids {
            params_vec.push(id as &dyn rusqlite::ToSql);
        }
        tx.execute(&sql, params_vec.as_slice())
            .map_err(CursorError::Commit)?;
        tx.commit().map_err(CursorError::Commit)?;
        Ok(())
    }
}

/// Per-state row counts surfaced by [`RollupsCursor::counts`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryCounts {
    pub pending: u64,
    pub in_flight: u64,
    pub delivered: u64,
    pub retry: u64,
    pub poison: u64,
}

fn current_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_test_db_with_rows(rows: &[(&str, &str, i64)]) -> Connection {
        // rows = (event_id, delivery_status, retry_count)
        let conn = Connection::open_in_memory().unwrap();
        // Minimal schema — same shape as noodle-embellish's table,
        // restricted to the columns the cursor reads.
        conn.execute_batch(
            "CREATE TABLE ai_telemetry_v_0_0_2 (
                event_id              TEXT     PRIMARY KEY,
                schema_id             TEXT     NOT NULL DEFAULT 'ai-telemetry',
                schema_version        TEXT     NOT NULL DEFAULT '0.0.2',
                event_type            TEXT     NOT NULL DEFAULT 'api_call',
                timestamp             INTEGER  NOT NULL DEFAULT 0,
                provider              TEXT     NOT NULL DEFAULT 'anthropic',
                model                 TEXT     NOT NULL DEFAULT 'claude',
                endpoint_path         TEXT     NOT NULL DEFAULT '/v1/messages',
                streaming             INTEGER  NOT NULL DEFAULT 1,
                status_code           INTEGER  NOT NULL DEFAULT 200,
                error_type            TEXT,
                latency_ms            INTEGER  NOT NULL DEFAULT 0,
                input_tokens          INTEGER  NOT NULL DEFAULT 0,
                output_tokens         INTEGER  NOT NULL DEFAULT 0,
                api_key_prefix        TEXT,
                api_key_type          TEXT,
                session_id            TEXT,
                session_hash          TEXT,
                client_user_agent     TEXT,
                agent_version         TEXT     NOT NULL DEFAULT '0.0.1',
                agent_arch            TEXT     NOT NULL DEFAULT 'aarch64',
                context_json          TEXT,
                provider_metadata_json TEXT,
                correlation_quality   TEXT     NOT NULL DEFAULT 'minimal',
                shipped_at            INTEGER,
                delivery_status       TEXT     NOT NULL DEFAULT 'pending',
                retry_count           INTEGER  NOT NULL DEFAULT 0,
                last_attempt_at       INTEGER,
                last_attempt_error    TEXT,
                brain_thread_id                     TEXT,
                brain_thread_turn_index             INTEGER,
                brain_compaction_detected           INTEGER,
                brain_compaction_directive_present  INTEGER,
                brain_compaction_directive_kind     TEXT,
                brain_blocks_dropped                INTEGER,
                brain_blocks_added                  INTEGER,
                brain_estimated_window_tokens       INTEGER,
                brain_api_context_management_beta   INTEGER,
                policy_decision                     TEXT,
                policy_mode                         TEXT,
                policy_risk                         REAL,
                policy_rule                         TEXT,
                policy_rationale                    TEXT,
                policy_surface                      TEXT,
                turn_id                             TEXT,
                role                                TEXT,
                frame_id                            TEXT,
                parent_frame_id                     TEXT,
                depth                               INTEGER,
                context_input_tokens                INTEGER,
                context_cache_read_tokens           INTEGER,
                context_cache_creation_tokens       INTEGER,
                context_output_tokens               INTEGER,
                context_system_bytes                INTEGER,
                context_tools_bytes                 INTEGER,
                context_tools_count                 INTEGER,
                context_preamble_bytes              INTEGER
            );",
        )
        .unwrap();
        for (id, status, retry) in rows {
            conn.execute(
                "INSERT INTO ai_telemetry_v_0_0_2 (event_id, delivery_status, retry_count) VALUES (?1, ?2, ?3)",
                params![id, status, retry],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn claim_batch_flips_pending_to_in_flight_and_returns_rows() {
        let conn = open_test_db_with_rows(&[
            ("nl-1", "pending", 0),
            ("nl-2", "pending", 0),
            ("nl-3", "delivered", 0),
        ]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        assert_eq!(batch.rows.len(), 2);
        assert!(batch.rows.iter().any(|r| r.event_id == "nl-1"));
        assert!(batch.rows.iter().any(|r| r.event_id == "nl-2"));
        // Already-delivered row is left alone.
        let counts = cursor.counts().unwrap();
        assert_eq!(counts.in_flight, 2);
        assert_eq!(counts.delivered, 1);
        assert_eq!(counts.pending, 0);
    }

    #[test]
    fn claim_batch_picks_retry_rows_too() {
        let conn = open_test_db_with_rows(&[("nl-1", "retry", 2), ("nl-2", "poison", 5)]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.rows[0].event_id, "nl-1");
        assert_eq!(batch.rows[0].retry_count, 2);
    }

    #[test]
    fn ack_delivered_transitions_in_flight_to_delivered() {
        let conn = open_test_db_with_rows(&[("nl-1", "pending", 0), ("nl-2", "pending", 0)]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        let ids: Vec<String> = batch.rows.iter().map(|r| r.event_id.clone()).collect();
        cursor.ack_delivered(&ids).unwrap();
        let counts = cursor.counts().unwrap();
        assert_eq!(counts.delivered, 2);
        assert_eq!(counts.in_flight, 0);
    }

    #[test]
    fn ack_failed_increments_retry_and_transitions_to_retry() {
        let conn = open_test_db_with_rows(&[("nl-1", "pending", 0)]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        cursor
            .ack_failed(&[batch.rows[0].event_id.clone()], "connection refused")
            .unwrap();
        let counts = cursor.counts().unwrap();
        assert_eq!(counts.retry, 1);
        assert_eq!(counts.in_flight, 0);
        let retry: i64 = cursor
            .conn
            .query_row(
                "SELECT retry_count FROM ai_telemetry_v_0_0_2 WHERE event_id = 'nl-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retry, 1);
    }

    #[test]
    fn ack_failed_at_cap_transitions_to_poison() {
        let conn = open_test_db_with_rows(&[("nl-1", "pending", 4)]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        cursor
            .ack_failed(&[batch.rows[0].event_id.clone()], "still down")
            .unwrap();
        let counts = cursor.counts().unwrap();
        assert_eq!(counts.poison, 1);
        assert_eq!(counts.retry, 0);
    }

    #[test]
    fn recover_in_flight_resets_to_pending() {
        let conn = open_test_db_with_rows(&[
            ("nl-1", "in_flight", 0),
            ("nl-2", "in_flight", 1),
            ("nl-3", "delivered", 0),
        ]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let n = cursor.recover_in_flight().unwrap();
        assert_eq!(n, 2);
        let counts = cursor.counts().unwrap();
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.in_flight, 0);
        assert_eq!(counts.delivered, 1);
    }

    #[test]
    fn empty_table_yields_empty_batch() {
        let conn = open_test_db_with_rows(&[]);
        let mut cursor = RollupsCursor::from_connection(conn, 5);
        let batch = cursor.claim_batch(10).unwrap();
        assert!(batch.rows.is_empty());
    }
}
