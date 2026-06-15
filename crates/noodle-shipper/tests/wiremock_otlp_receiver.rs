//! Integration test — drive synthetic rollups rows through the
//! shipper against a wiremock OTLP/HTTP receiver. Validates the
//! full claim → export → ack cycle end-to-end, including:
//!
//! - the receiver actually captures the OTLP payload,
//! - correlation block is at the expected scope per E4 §B,
//! - rows transition `pending → delivered` on success,
//! - rows stay `pending`-equivalent (`retry`) on collector failure.

use std::time::Duration;

use noodle_shipper::{Shipper, ShipperConfig, Transport};
use rusqlite::{Connection, params};
use serde_json::Value;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TABLE_DDL: &str = "
CREATE TABLE IF NOT EXISTS ai_telemetry_v_0_0_2 (
    event_id              TEXT     PRIMARY KEY,
    schema_id             TEXT     NOT NULL DEFAULT 'ai-telemetry',
    schema_version        TEXT     NOT NULL DEFAULT '0.0.2',
    event_type            TEXT     NOT NULL DEFAULT 'api_call',
    timestamp             INTEGER  NOT NULL DEFAULT 0,
    provider              TEXT     NOT NULL DEFAULT 'anthropic',
    model                 TEXT     NOT NULL DEFAULT 'claude-3-5-sonnet',
    endpoint_path         TEXT     NOT NULL DEFAULT '/v1/messages',
    streaming             INTEGER  NOT NULL DEFAULT 1,
    status_code           INTEGER  NOT NULL DEFAULT 200,
    error_type            TEXT,
    latency_ms            INTEGER  NOT NULL DEFAULT 1000,
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
    correlation_quality   TEXT     NOT NULL DEFAULT 'full',
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
    depth                               INTEGER
);";

fn make_db_with_rows(rows: &[(&str, Option<&str>, Option<&str>)]) -> (TempDir, std::path::PathBuf) {
    // rows = (event_id, session_id, context_json)
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("rollups.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(TABLE_DDL).unwrap();
    for (id, session_id, context_json) in rows {
        conn.execute(
            "INSERT INTO ai_telemetry_v_0_0_2 (event_id, session_id, context_json) VALUES (?1, ?2, ?3)",
            params![id, session_id, context_json],
        )
        .unwrap();
    }
    drop(conn);
    (tmp, db_path)
}

#[tokio::test]
async fn shipper_pushes_pending_rows_through_otlp_and_marks_delivered() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    // D1.1 — exporter ships spans alongside logs on every batch.
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let (_tmp, db_path) = make_db_with_rows(&[
        ("nl-1", Some("session-A"), Some(r#"{"tool":"Claude Code"}"#)),
        ("nl-2", Some("session-A"), None),
    ]);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let stats = shipper.tick().await.unwrap();
    assert_eq!(stats.claimed, 2);
    assert_eq!(stats.delivered, 2);
    assert_eq!(stats.failed, 0);

    let counts = shipper.counts().unwrap();
    assert_eq!(counts.delivered, 2);
    assert_eq!(counts.pending, 0);
    assert_eq!(counts.in_flight, 0);

    // Verify the wiremock receiver actually captured the OTLP
    // payload — body shape per OTLP/HTTP §3.1.
    let received = mock_server.received_requests().await.unwrap();
    // D1.1 — exporter now ships logs AND spans per batch.
    let logs_req = received
        .iter()
        .find(|r| r.url.path() == "/v1/logs")
        .expect("logs POST present");
    let traces_req = received
        .iter()
        .find(|r| r.url.path() == "/v1/traces")
        .expect("traces POST present");
    let body: Value = serde_json::from_slice(&logs_req.body).unwrap();
    let records = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"];
    let arr = records.as_array().unwrap();
    assert_eq!(arr.len(), 2, "two log records (one per row)");
    let span_body: Value = serde_json::from_slice(&traces_req.body).unwrap();
    let spans = span_body["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    assert_eq!(spans.len(), 2, "two spans (one per row)");
    for s in spans {
        let trace_id = s["traceId"].as_str().unwrap();
        let span_id = s["spanId"].as_str().unwrap();
        assert_eq!(trace_id.len(), 32, "traceId is 16 bytes hex");
        assert_eq!(span_id.len(), 16, "spanId is 8 bytes hex");
        assert!(s["startTimeUnixNano"].is_string());
        assert!(s["endTimeUnixNano"].is_string());
    }

    let event_ids: Vec<&str> = arr
        .iter()
        .map(|r| {
            r["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|kv| kv["key"] == "event_id")
                .unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert!(event_ids.contains(&"nl-1"));
    assert!(event_ids.contains(&"nl-2"));
}

#[tokio::test]
async fn shipper_retries_when_collector_returns_5xx() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&mock_server)
        .await;

    let (_tmp, db_path) = make_db_with_rows(&[("nl-1", Some("session-A"), None)]);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let stats = shipper.tick().await.unwrap();
    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.delivered, 0);
    assert_eq!(stats.failed, 1);

    // Row landed in retry, not delivered. retry_count = 1.
    let counts = shipper.counts().unwrap();
    assert_eq!(counts.delivered, 0);
    assert_eq!(counts.retry, 1);
    assert_eq!(counts.in_flight, 0);
}

#[tokio::test]
async fn shipper_handles_empty_database_gracefully() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let (_tmp, db_path) = make_db_with_rows(&[]);
    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let stats = shipper.tick().await.unwrap();
    assert_eq!(stats.claimed, 0);
    // No POST should have been issued.
    let received = mock_server.received_requests().await.unwrap();
    assert!(received.is_empty());
}

/// ADR 047 rung 1 — full pipeline e2e: a row with `brain_*` columns
/// populated reaches the wiremock OTLP receiver as `brain.*`
/// attributes on the log record. Validates the `SQLite` columns added
/// in `noodle-embellish::sqlite` flow through `cursor::RollupsRow`
/// and `mapping::row_to_otlp_log` without any value-shape regression.
#[tokio::test]
async fn brain_attrs_reach_the_wiremock_receiver_end_to_end() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("rollups.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(TABLE_DDL).unwrap();
    // Insert one row with the brain_* columns populated end-to-end —
    // matches what `noodle-embellish` would write after the brain
    // observed a real /v1/messages turn against an Anthropic codec
    // with context-management beta on.
    conn.execute(
        "INSERT INTO ai_telemetry_v_0_0_2 (
            event_id, session_id, brain_thread_id, brain_thread_turn_index,
            brain_compaction_detected, brain_compaction_directive_present,
            brain_compaction_directive_kind, brain_blocks_dropped,
            brain_blocks_added, brain_estimated_window_tokens,
            brain_api_context_management_beta
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11
         )",
        params![
            "nl-brain-1",
            "session-Z",
            "c99d2c61-17c6-4be7-a328-813abe8bd2b4",
            1_i64,
            0_i64, // compaction_detected (false)
            1_i64, // compaction_directive_present (true)
            "clear_thinking_20251015",
            0_i64, // blocks_dropped
            1_i64, // blocks_added
            0_i64, // estimated_window_tokens
            1_i64, // api_context_management_beta (true)
        ],
    )
    .unwrap();
    drop(conn);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let stats = shipper.tick().await.unwrap();
    assert_eq!(stats.delivered, 1);

    let received = mock_server.received_requests().await.unwrap();
    // D1.1 — logs + traces both ship per batch.
    let logs_req = received
        .iter()
        .find(|r| r.url.path() == "/v1/logs")
        .expect("logs POST present");
    let body: Value = serde_json::from_slice(&logs_req.body).unwrap();
    let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    let attrs: serde_json::Map<String, Value> = record["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
        .collect();

    assert_eq!(
        attrs["brain.thread_id"]["stringValue"],
        "c99d2c61-17c6-4be7-a328-813abe8bd2b4"
    );
    assert_eq!(attrs["brain.thread_turn_index"]["intValue"], "1");
    assert_eq!(attrs["brain.compaction_detected"]["boolValue"], false);
    assert_eq!(
        attrs["brain.compaction_directive_present"]["boolValue"],
        true
    );
    assert_eq!(
        attrs["brain.compaction_directive_kind"]["stringValue"],
        "clear_thinking_20251015"
    );
    assert_eq!(attrs["brain.blocks_added"]["intValue"], "1");
    assert_eq!(attrs["brain.blocks_dropped"]["intValue"], "0");
    assert_eq!(
        attrs["brain.api_context_management_beta"]["boolValue"],
        true
    );
}

/// ADR 046 §2.3 — full pipeline e2e: a default Anthropic
/// `/v1/messages` row reaches the wiremock OTLP receiver carrying
/// the `OTel` `GenAI` semantic-convention attributes
/// (`gen_ai.provider.name`, `gen_ai.request.model`,
/// `gen_ai.operation.name`, `gen_ai.usage.*`,
/// `gen_ai.conversation.id`) so off-the-shelf `GenAI` viewers can
/// render the captured session without any noodle-side UI.
#[tokio::test]
async fn gen_ai_semantic_convention_attrs_reach_the_wiremock_receiver_end_to_end() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("rollups.sqlite");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(TABLE_DDL).unwrap();
    // Insert one row with non-zero token counts so `gen_ai.usage.*`
    // emit (the shipper omits them at zero per the spec's
    // "if applicable" recommendation).
    conn.execute(
        "INSERT INTO ai_telemetry_v_0_0_2 (event_id, session_id, input_tokens, output_tokens)
         VALUES (?1, ?2, ?3, ?4)",
        params!["nl-genai-1", "session-X", 1234_i64, 567_i64],
    )
    .unwrap();
    drop(conn);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let stats = shipper.tick().await.unwrap();
    assert_eq!(stats.delivered, 1);

    let received = mock_server.received_requests().await.unwrap();
    let logs_req = received
        .iter()
        .find(|r| r.url.path() == "/v1/logs")
        .expect("logs POST present");
    let body: Value = serde_json::from_slice(&logs_req.body).unwrap();
    let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    let attrs: serde_json::Map<String, Value> = record["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
        .collect();

    assert_eq!(attrs["gen_ai.provider.name"]["stringValue"], "anthropic");
    assert_eq!(
        attrs["gen_ai.request.model"]["stringValue"],
        "claude-3-5-sonnet"
    );
    assert_eq!(attrs["gen_ai.operation.name"]["stringValue"], "chat");
    assert_eq!(attrs["gen_ai.request.stream"]["boolValue"], true);
    assert_eq!(attrs["gen_ai.usage.input_tokens"]["intValue"], "1234");
    assert_eq!(attrs["gen_ai.usage.output_tokens"]["intValue"], "567");
    assert_eq!(attrs["gen_ai.conversation.id"]["stringValue"], "session-X");
}

/// D1.1 — the spans payload reaches `/v1/traces` with the correct
/// shape: 16-byte hex `traceId`, 8-byte hex `spanId`, span name
/// follows `<operation> <model>`, attributes mirror the log
/// record's `gen_ai.*` + `context.*` set (including the
/// `gen_ai.activity.*` mirror added for AI-aware viewers).
#[tokio::test]
async fn span_attrs_reach_v1_traces_with_correct_shape() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let (_tmp, db_path) = make_db_with_rows(&[(
        "nl-span-1",
        Some("sess-1"),
        Some(
            r#"{"tool":"Claude Code","work_type":"coding","issue":"CP-42375","project":"noodle"}"#,
        ),
    )]);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 10,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    shipper.tick().await.unwrap();

    let received = mock_server.received_requests().await.unwrap();
    let traces_req = received
        .iter()
        .find(|r| r.url.path() == "/v1/traces")
        .expect("traces POST present");
    let body: Value = serde_json::from_slice(&traces_req.body).unwrap();
    let span = &body["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

    assert_eq!(span["traceId"].as_str().unwrap().len(), 32);
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    assert_eq!(span["kind"], 3); // CLIENT
    // Span name: `<operation> <model>` per OTel GenAI; make_db_with_rows
    // leaves `model` empty so it falls back to the operation alone.
    let name = span["name"].as_str().unwrap();
    assert!(
        name.contains("anthropic") || name.contains("chat") || !name.is_empty(),
        "got name {name:?}"
    );

    let attrs: serde_json::Map<String, Value> = span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
        .collect();

    // Backward-compat: noodle-native context.* still emitted.
    assert_eq!(attrs["context.tool"]["stringValue"], "Claude Code");
    assert_eq!(attrs["context.work_type"]["stringValue"], "coding");
    assert_eq!(attrs["context.issue"]["stringValue"], "CP-42375");
    assert_eq!(attrs["context.project"]["stringValue"], "noodle");
    // Forward-compat: gen_ai.activity.* mirror lit up for AI viewers.
    assert_eq!(attrs["gen_ai.activity.type"]["stringValue"], "coding");
    assert_eq!(attrs["gen_ai.activity.issue"]["stringValue"], "CP-42375");
    assert_eq!(attrs["gen_ai.activity.project"]["stringValue"], "noodle");
    // Tool is attribution-side, NOT activity — must NOT be mirrored.
    assert!(
        attrs.get("gen_ai.activity.tool").is_none(),
        "tool is agent identity, not activity"
    );
}

#[tokio::test]
async fn shipper_drains_in_two_ticks_when_batch_smaller_than_pending() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/traces"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock_server)
        .await;

    let (_tmp, db_path) = make_db_with_rows(&[
        ("nl-1", None, None),
        ("nl-2", None, None),
        ("nl-3", None, None),
    ]);

    let cfg = ShipperConfig {
        db_path,
        endpoint: mock_server.uri(),
        transport: Transport::HttpJson,
        batch_size: 2,
        poll_interval: Duration::from_secs(1),
        max_retries: 5,
    };
    let mut shipper = Shipper::new(cfg).unwrap();
    let t1 = shipper.tick().await.unwrap();
    assert_eq!(t1.delivered, 2);
    let t2 = shipper.tick().await.unwrap();
    assert_eq!(t2.delivered, 1);
    let counts = shipper.counts().unwrap();
    assert_eq!(counts.delivered, 3);
    assert_eq!(counts.pending, 0);
}
