//! Read-only ad-hoc SQL over the embellish `rollups.db` (ADR 031).
//!
//! V2 OTLP-query tab — turns the viewer into a Datasette-style query
//! console over the `ai_telemetry_v_0_0_2` schema. The embellisher
//! writes the DB on the shared `emptyDir`; this module opens a
//! `READ_ONLY` connection and never writes.
//!
//! ## Endpoints (rooted at `/api/rollups`)
//!
//! - `GET  /schema` → `{table, columns: [{name, type, notnull, pk}]}`
//! - `POST /query`  → `{sql} → {columns, rows, row_count, truncated, elapsed_ms}`
//!
//! ## Safety
//!
//! - Opened with `SQLITE_OPEN_READ_ONLY` — `INSERT`/`UPDATE`/`DELETE`
//!   error at the `SQLite` layer with `attempt to write a readonly
//!   database`, surfaced as `400 Bad Request` to the client.
//! - Row results capped at `ROW_CAP` (10,000); responses carry a
//!   `truncated: true` flag when the cap was hit.
//! - `SQLite` `busy_timeout = 2s` — `noodle-embellish` writes in WAL
//!   mode so reads don't block, but the bound caps any unusual
//!   schema-pause.
//! - The viewer's manifest mounts the shared `emptyDir`
//!   `readOnly: true` on the viewer container; defense-in-depth even
//!   if a future change ever asked for write access at the kernel
//!   layer.
//!
//! ## Lazy-open
//!
//! The DB may not exist at viewer startup — `noodle-embellish` may
//! still be initialising its schema, or the operator pointed the
//! viewer at a path that doesn't exist yet. [`RollupsState::new`]
//! tries to open and records the outcome; handlers re-attempt on
//! every request when the cached connection is `None`. Once open the
//! connection is reused for the process lifetime.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Maximum number of rows returned by a single `/query` response.
/// Beyond this the result is truncated and the response carries
/// `truncated: true`.
const ROW_CAP: usize = 10_000;

/// `SQLite` `busy_timeout`. The embellisher writes via WAL so reads
/// do not normally block; this bounds the worst-case stall.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// Read-only handle to the rollups DB. `Clone`-friendly — the inner
/// `Connection` is held behind an `Arc<Mutex<_>>`, so handlers serialise
/// `SQLite` access without blocking the tokio runtime.
#[derive(Clone)]
pub struct RollupsState {
    inner: Arc<RollupsInner>,
}

struct RollupsInner {
    conn: Mutex<Option<Connection>>,
    db_path: PathBuf,
}

impl RollupsState {
    /// Try to open the rollups DB at startup. Always returns a
    /// state — handlers re-attempt the open lazily if the DB wasn't
    /// available yet (typical right after a fresh deploy where the
    /// embellisher hasn't created the DB yet).
    #[must_use]
    pub fn new(db_path: PathBuf) -> Self {
        let conn = Self::try_open(&db_path).ok();
        if conn.is_some() {
            tracing::info!(path = %db_path.display(), "rollups db opened (read-only)");
        } else {
            tracing::warn!(
                path = %db_path.display(),
                "rollups db not yet available — handlers will retry on first /api/rollups request",
            );
        }
        Self {
            inner: Arc::new(RollupsInner {
                conn: Mutex::new(conn),
                db_path,
            }),
        }
    }

    fn try_open(path: &PathBuf) -> Result<Connection, rusqlite::Error> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(conn)
    }

    /// Run `f` with the held connection, re-opening lazily if the
    /// startup attempt failed.
    async fn with_conn<F, R>(&self, f: F) -> Result<R, RollupsError>
    where
        F: FnOnce(&Connection) -> Result<R, RollupsError>,
    {
        let mut guard = self.inner.conn.lock().await;
        if guard.is_none() {
            match Self::try_open(&self.inner.db_path) {
                Ok(c) => {
                    tracing::info!(
                        path = %self.inner.db_path.display(),
                        "rollups db opened on retry (read-only)",
                    );
                    *guard = Some(c);
                }
                Err(e) => return Err(RollupsError::Unavailable(e.to_string())),
            }
        }
        let conn = guard.as_ref().expect("connection populated above");
        f(conn)
    }
}

/// One column from `PRAGMA table_info(ai_telemetry_v_0_0_2)`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub notnull: bool,
    pub pk: bool,
}

/// Response for `GET /api/rollups/schema`.
#[derive(Debug, Serialize)]
pub struct SchemaResponse {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

/// `GET /api/rollups/schema` — returns the column list of the
/// `ai_telemetry_v_0_0_2` table. Drives the saved-queries / schema
/// hint UI in the OTLP tab.
pub async fn schema(
    State(state): State<RollupsState>,
) -> Result<Json<SchemaResponse>, (StatusCode, String)> {
    state
        .with_conn(|conn| {
            let mut stmt = conn
                .prepare("PRAGMA table_info(ai_telemetry_v_0_0_2)")
                .map_err(|e| RollupsError::Sql(e.to_string()))?;
            let cols = stmt
                .query_map([], |row| {
                    Ok(ColumnInfo {
                        name: row.get::<_, String>(1)?,
                        ty: row.get::<_, String>(2)?,
                        notnull: row.get::<_, i64>(3)? != 0,
                        pk: row.get::<_, i64>(5)? != 0,
                    })
                })
                .map_err(|e| RollupsError::Sql(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| RollupsError::Sql(e.to_string()))?;
            Ok(SchemaResponse {
                table: "ai_telemetry_v_0_0_2".to_owned(),
                columns: cols,
            })
        })
        .await
        .map(Json)
        .map_err(error_status)
}

/// Request body for `POST /api/rollups/query`.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    pub sql: String,
}

/// Response for `POST /api/rollups/query`.
#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub row_count: usize,
    pub truncated: bool,
    pub elapsed_ms: u128,
}

/// `POST /api/rollups/query` — runs `sql` against the read-only
/// rollups DB. Returns the column names + rows + an `elapsed_ms`
/// stamp the UI surfaces in the footer.
pub async fn query(
    State(state): State<RollupsState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, (StatusCode, String)> {
    state
        .with_conn(|conn| {
            let start = Instant::now();
            let mut stmt = conn
                .prepare(&req.sql)
                .map_err(|e| RollupsError::Sql(format!("prepare: {e}")))?;
            let col_count = stmt.column_count();
            let columns: Vec<String> = (0..col_count)
                .map(|i| stmt.column_name(i).unwrap_or("").to_owned())
                .collect();
            let mut out: Vec<Vec<serde_json::Value>> = Vec::with_capacity(64);
            let mut truncated = false;
            let mut rows = stmt
                .query([])
                .map_err(|e| RollupsError::Sql(format!("query: {e}")))?;
            while let Some(r) = rows.next().map_err(|e| RollupsError::Sql(e.to_string()))? {
                if out.len() >= ROW_CAP {
                    truncated = true;
                    break;
                }
                let mut row = Vec::with_capacity(col_count);
                for i in 0..col_count {
                    let v = r.get_ref(i).map_err(|e| RollupsError::Sql(e.to_string()))?;
                    row.push(value_ref_to_json(v));
                }
                out.push(row);
            }
            let elapsed_ms = start.elapsed().as_millis();
            let row_count = out.len();
            Ok(QueryResponse {
                columns,
                rows: out,
                row_count,
                truncated,
                elapsed_ms,
            })
        })
        .await
        .map(Json)
        .map_err(error_status)
}

fn value_ref_to_json(v: ValueRef<'_>) -> serde_json::Value {
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => serde_json::Value::String(s.to_owned()),
            Err(_) => serde_json::Value::String(format!("<{} bytes invalid utf-8>", t.len())),
        },
        ValueRef::Blob(b) => serde_json::Value::String(format!("<{} bytes blob>", b.len())),
    }
}

#[derive(Debug)]
enum RollupsError {
    Sql(String),
    Unavailable(String),
}

fn error_status(e: RollupsError) -> (StatusCode, String) {
    match e {
        RollupsError::Sql(s) => (StatusCode::BAD_REQUEST, s),
        RollupsError::Unavailable(s) => (StatusCode::SERVICE_UNAVAILABLE, s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::NamedTempFile;

    fn seed_db(rows: &[(&str, &str, i64, i64)]) -> NamedTempFile {
        let f = NamedTempFile::new().unwrap();
        let conn = Connection::open(f.path()).unwrap();
        conn.execute_batch(
            r"
            CREATE TABLE ai_telemetry_v_0_0_2 (
                event_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                brain_thread_id TEXT,
                brain_thread_turn_index INTEGER,
                brain_compaction_detected INTEGER
            );
            ",
        )
        .unwrap();
        for (event_id, thread_id, turn, compaction) in rows {
            conn.execute(
                "INSERT INTO ai_telemetry_v_0_0_2
                    (event_id, provider, model, brain_thread_id,
                     brain_thread_turn_index, brain_compaction_detected)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    event_id,
                    "anthropic",
                    "claude-3-5-sonnet",
                    thread_id,
                    turn,
                    compaction,
                ],
            )
            .unwrap();
        }
        drop(conn);
        f
    }

    #[tokio::test]
    async fn schema_returns_brain_columns() {
        let db = seed_db(&[]);
        let state = RollupsState::new(db.path().to_path_buf());
        let resp = schema(State(state)).await.expect("schema ok").0;
        let names: Vec<&str> = resp.columns.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"event_id"));
        assert!(names.contains(&"brain_thread_id"));
        assert!(names.contains(&"brain_compaction_detected"));
        assert_eq!(resp.table, "ai_telemetry_v_0_0_2");
    }

    #[tokio::test]
    async fn query_returns_rows_and_columns_in_order() {
        let db = seed_db(&[("a", "t1", 1, 0), ("b", "t1", 2, 1), ("c", "t2", 1, 0)]);
        let state = RollupsState::new(db.path().to_path_buf());
        let resp = query(
            State(state),
            Json(QueryRequest {
                sql: "SELECT brain_thread_id, COUNT(*) AS turns, \
                             SUM(brain_compaction_detected) AS comps \
                      FROM ai_telemetry_v_0_0_2 \
                      GROUP BY brain_thread_id ORDER BY brain_thread_id"
                    .to_owned(),
            }),
        )
        .await
        .expect("query ok")
        .0;
        assert_eq!(resp.columns, vec!["brain_thread_id", "turns", "comps"]);
        assert_eq!(resp.row_count, 2);
        assert!(!resp.truncated);
        assert_eq!(resp.rows[0][0], serde_json::Value::String("t1".to_owned()));
        assert_eq!(resp.rows[0][1], serde_json::Value::from(2_i64));
        assert_eq!(resp.rows[0][2], serde_json::Value::from(1_i64));
    }

    #[tokio::test]
    async fn invalid_sql_returns_400() {
        let db = seed_db(&[]);
        let state = RollupsState::new(db.path().to_path_buf());
        let err = query(
            State(state),
            Json(QueryRequest {
                sql: "SELECT bogus_col FROM bogus_table".to_owned(),
            }),
        )
        .await
        .expect_err("expected bad request");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn write_attempt_blocked_by_read_only_flag() {
        let db = seed_db(&[("a", "t1", 1, 0)]);
        let state = RollupsState::new(db.path().to_path_buf());
        let err = query(
            State(state),
            Json(QueryRequest {
                sql: "DELETE FROM ai_telemetry_v_0_0_2".to_owned(),
            }),
        )
        .await
        .expect_err("expected sql error on read-only db");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            err.1.to_ascii_lowercase().contains("readonly")
                || err.1.to_ascii_lowercase().contains("read-only")
                || err.1.to_ascii_lowercase().contains("read only"),
            "expected readonly-related message, got: {}",
            err.1,
        );
    }

    #[tokio::test]
    async fn missing_db_returns_503() {
        let path = std::path::PathBuf::from("/nonexistent/noodle-test/rollups.db");
        let state = RollupsState::new(path);
        let err = query(
            State(state),
            Json(QueryRequest {
                sql: "SELECT 1".to_owned(),
            }),
        )
        .await
        .expect_err("expected 503 when DB missing");
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn row_cap_truncates_large_results() {
        let db = NamedTempFile::new().unwrap();
        let conn = Connection::open(db.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE ai_telemetry_v_0_0_2 (event_id TEXT PRIMARY KEY, n INTEGER);",
        )
        .unwrap();
        // Seed > ROW_CAP rows. 10,001 is enough to exercise the truncation path.
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..=ROW_CAP {
            tx.execute(
                "INSERT INTO ai_telemetry_v_0_0_2 (event_id, n) VALUES (?1, ?2)",
                params![format!("evt-{i}"), i64::try_from(i).unwrap()],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        drop(conn);
        let state = RollupsState::new(db.path().to_path_buf());
        let resp = query(
            State(state),
            Json(QueryRequest {
                sql: "SELECT event_id FROM ai_telemetry_v_0_0_2".to_owned(),
            }),
        )
        .await
        .expect("query ok")
        .0;
        assert_eq!(resp.row_count, ROW_CAP);
        assert!(resp.truncated);
    }
}
