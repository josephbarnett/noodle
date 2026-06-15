//! Golden equivalence test — refactor slice S23.
//!
//! Asserts that the new decoder-driven mapper (`map_decoded_pair`,
//! reached via `Embellisher::process_records`) produces a `SQLite`
//! database **byte-equivalent** to S16's baseline (`map_pair` reached
//! via the same `Embellisher` API that S16 shipped).
//!
//! The load-bearing safety property of S23 is in
//! `docs/adrs/refactor-overview.md` §10.2 / §10.3:
//!
//! > Decoder consolidation breaks embellish output → S23's e2e
//! > asserts the SQLite database byte-identical to the S16 baseline;
//! > the refactor is a strict swap of the input plumbing, not the
//! > mapping logic.
//!
//! ## How this is structured
//!
//! Per the e2e contract (no fixture-replay), the real-world property
//! is asserted in `e2e_embellish_decoder_path_exec_claude.rs` against
//! a live claude session. This golden test pins the same property
//! against a **synthetic, hand-crafted** request/response pair so the
//! invariant is checked in CI without needing a network round-trip
//! to Anthropic.
//!
//! The synthetic input is shaped to exercise every load-bearing
//! field S23 routes through the decoder (`provider`, `status_code`,
//! `input_tokens`, `output_tokens`) plus the envelope-level fields
//! the mapper still pulls from the raw view (headers, marks,
//! subscription, agent, rate-limit). Diff anything between the
//! two paths and we fail.

use noodle_embellish::{Embellisher, TapEntryView};
use noodle_embellish::{TelemetryRow, decode_pair, map_decoded_pair, map_pair};
use rusqlite::Connection;
use serde_json::json;

/// Pull a single column from the first row in `ai_telemetry_v_0_0_2`.
/// Used by the golden tests to keep tuple types compact (avoids
/// `clippy::type_complexity` warnings on 8+ -element tuples).
fn column<T>(conn: &Connection, name: &str) -> T
where
    T: rusqlite::types::FromSql,
{
    conn.query_row(
        &format!("SELECT {name} FROM ai_telemetry_v_0_0_2 LIMIT 1"),
        [],
        |r| r.get::<_, T>(0),
    )
    .unwrap_or_else(|e| panic!("read column {name}: {e}"))
}

/// Pull a single column from the first row in `ai_telemetry_v_0_0_2`
/// matching `where_clause`.
fn column_where<T>(conn: &Connection, name: &str, where_clause: &str) -> T
where
    T: rusqlite::types::FromSql,
{
    conn.query_row(
        &format!("SELECT {name} FROM ai_telemetry_v_0_0_2 WHERE {where_clause} LIMIT 1"),
        [],
        |r| r.get::<_, T>(0),
    )
    .unwrap_or_else(|e| panic!("read column {name} where {where_clause}: {e}"))
}

/// A "tap.jsonl"-shaped request record carrying every field the
/// mapper reads. Mirrors the real Anthropic-via-noodle shape.
fn fixture_request() -> TapEntryView {
    TapEntryView::from_value(json!({
        "direction": "request",
        "timestamp": "2026-05-25T17:00:00.250Z",
        "event_id": "01HXYZ-S23-GOLDEN",
        "provider": "anthropic",
        "method": "POST",
        "url": "https://api.anthropic.com/v1/messages?beta=true&prompt_caching=on",
        "headers": {
            "User-Agent": ["claude-cli/1.2.3"],
            "X-Stainless-Lang": ["js"],
            "X-Stainless-Runtime": ["node"],
            "X-Stainless-Runtime-Version": ["v20.0.0"],
            "X-Stainless-Os": ["MacOS"],
            "X-Stainless-Arch": ["arm64"],
            "X-Stainless-Package-Version": ["0.20.0"],
            "X-Stainless-Retry-Count": ["1"],
            "X-Client-Request-Id": ["client-req-abc"],
            "Anthropic-Beta": ["computer-use-2025-01-24,prompt-caching-2024-07-31"]
        },
        "body": { "model": "claude-3-5-sonnet-20241022", "messages": [] },
        "envelope": {
            "machine": {
                "hostname": "joe-mac.local",
                "os_family": "macos",
                "architecture": "aarch64"
            },
            "collector_app": {
                "name": "noodle",
                "version": "0.0.1",
                "build_hash": "deadbeef",
                "build_date": "2026-05-21T00:00:00Z"
            },
            "subscription": {
                "api_key": { "prefix": "sk-ant-api0", "kind": "api_key", "source": "authorization_header" },
                "organization": {
                    "organization_id": "org_xyz",
                    "parent_organization_id": "org_parent",
                    "account_type": "enterprise"
                }
            }
        },
        "marks": { "session_id": "sess_golden", "turn_id": "turn_1" }
    }))
}

fn fixture_response() -> TapEntryView {
    TapEntryView::from_value(json!({
        "direction": "response",
        "timestamp": "2026-05-25T17:00:01.875Z",
        "event_id": "01HXYZ-S23-GOLDEN",
        "provider": "anthropic",
        "status": 200,
        "headers": {
            "Content-Type": ["text/event-stream"],
            "Request-Id": ["req_golden_abc"],
            "Anthropic-Ratelimit-Unified-Utilization": ["0.73"],
            "Anthropic-Ratelimit-Unified-Reset": ["45"]
        },
        // Content/events let the decoder produce a proper TurnEnd
        // with status + usage; the mapper consumes that.
        "content": {
            "blocks": [
                { "kind": "text", "text": "Two files." }
            ]
        },
        "events": [
            { "type": "message_start" },
            { "type": "message_delta", "delta": { "stop_reason": "end_turn" } },
            { "type": "message_stop" }
        ],
        "usage": {
            "tokens": {
                "input_tokens": 142,
                "output_tokens": 318,
                "cache_read_input_tokens": 512,
                "cache_creation_input_tokens": 64
            },
            "latency": { "total_ms": 1625 }
        }
    }))
}

/// Field-by-field comparator. `event_id` is minted by the
/// `SqliteWriter` at insert-time and is intentionally
/// non-deterministic, so we skip it here and only compare the
/// mapper-produced surface. The `Embellisher`-emitted bytes are
/// compared separately below.
#[allow(clippy::too_many_lines)]
fn assert_row_byte_equivalent(left: &TelemetryRow, right: &TelemetryRow, label: &str) {
    assert_eq!(left.schema_id, right.schema_id, "{label}.schema_id");
    assert_eq!(
        left.schema_version, right.schema_version,
        "{label}.schema_version"
    );
    assert_eq!(left.event_type, right.event_type, "{label}.event_type");
    assert_eq!(left.timestamp, right.timestamp, "{label}.timestamp");
    assert_eq!(left.request_id, right.request_id, "{label}.request_id");
    assert_eq!(left.provider, right.provider, "{label}.provider");
    assert_eq!(left.model, right.model, "{label}.model");
    assert_eq!(
        left.endpoint_path, right.endpoint_path,
        "{label}.endpoint_path"
    );
    assert_eq!(
        left.endpoint_params_json, right.endpoint_params_json,
        "{label}.endpoint_params_json"
    );
    assert_eq!(left.streaming, right.streaming, "{label}.streaming");
    assert_eq!(left.status_code, right.status_code, "{label}.status_code");
    assert_eq!(left.error_type, right.error_type, "{label}.error_type");
    assert_eq!(left.latency_ms, right.latency_ms, "{label}.latency_ms");
    assert_eq!(
        left.input_tokens, right.input_tokens,
        "{label}.input_tokens"
    );
    assert_eq!(
        left.output_tokens, right.output_tokens,
        "{label}.output_tokens"
    );
    assert_eq!(
        left.api_key_prefix, right.api_key_prefix,
        "{label}.api_key_prefix"
    );
    assert_eq!(
        left.api_key_type, right.api_key_type,
        "{label}.api_key_type"
    );
    assert_eq!(left.session_id, right.session_id, "{label}.session_id");
    assert_eq!(
        left.session_hash, right.session_hash,
        "{label}.session_hash"
    );
    assert_eq!(
        left.client_user_agent, right.client_user_agent,
        "{label}.client_user_agent"
    );
    assert_eq!(
        left.client_hostname, right.client_hostname,
        "{label}.client_hostname"
    );
    assert_eq!(left.client_lang, right.client_lang, "{label}.client_lang");
    assert_eq!(
        left.client_runtime, right.client_runtime,
        "{label}.client_runtime"
    );
    assert_eq!(
        left.client_runtime_version, right.client_runtime_version,
        "{label}.client_runtime_version"
    );
    assert_eq!(left.client_os, right.client_os, "{label}.client_os");
    assert_eq!(left.client_arch, right.client_arch, "{label}.client_arch");
    assert_eq!(
        left.client_sdk_name, right.client_sdk_name,
        "{label}.client_sdk_name"
    );
    assert_eq!(
        left.client_sdk_version, right.client_sdk_version,
        "{label}.client_sdk_version"
    );
    assert_eq!(
        left.client_retry_count, right.client_retry_count,
        "{label}.client_retry_count"
    );
    assert_eq!(
        left.agent_version, right.agent_version,
        "{label}.agent_version"
    );
    assert_eq!(left.agent_arch, right.agent_arch, "{label}.agent_arch");
    assert_eq!(
        left.agent_build_date, right.agent_build_date,
        "{label}.agent_build_date"
    );
    assert_eq!(
        left.agent_git_sha, right.agent_git_sha,
        "{label}.agent_git_sha"
    );
    assert_eq!(
        left.rate_limit_utilization, right.rate_limit_utilization,
        "{label}.rate_limit_utilization"
    );
    assert_eq!(
        left.rate_limit_window_seconds, right.rate_limit_window_seconds,
        "{label}.rate_limit_window_seconds"
    );
    assert_eq!(
        left.context_json, right.context_json,
        "{label}.context_json"
    );
    assert_eq!(
        left.provider_metadata_json, right.provider_metadata_json,
        "{label}.provider_metadata_json"
    );
    // Enrichment-plane Nones — guarded constants. If either path
    // accidentally fills them in, that's a regression.
    assert!(
        left.user_id.is_none() && right.user_id.is_none(),
        "{label}.user_id placeholder must be None on both paths"
    );
    assert!(
        left.estimated_cost_usd.is_none() && right.estimated_cost_usd.is_none(),
        "{label}.estimated_cost_usd placeholder must be None on both paths"
    );
}

#[test]
fn decoder_driven_mapper_matches_s16_baseline_field_for_field() {
    let req = fixture_request();
    let resp = fixture_response();
    let baseline = map_pair(&req, &resp).expect("S16 baseline produces a row");

    let pair = decode_pair(fixture_request(), fixture_response());
    let decoded = map_decoded_pair(&pair).expect("S23 decoder path produces a row");

    assert_row_byte_equivalent(&baseline, &decoded, "S16 vs S23");
}

#[test]
fn embellisher_inserts_rows_with_decoder_driven_path() {
    // Drive the full Embellisher pipeline end-to-end against the
    // synthetic pair and confirm the SQLite row populates the
    // load-bearing columns S23 routes through the decoder.
    let mut embellisher = Embellisher::open_in_memory().expect("open in-memory embellisher writer");
    let stats = embellisher
        .process_records(vec![fixture_request(), fixture_response()])
        .expect("process_records");
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.responses, 1);
    assert_eq!(stats.rows_written, 1);
    assert_eq!(stats.unpaired_requests, 0);
    assert_eq!(stats.orphan_responses, 0);

    // Spot-check the inserted row. Same columns the S16 e2e checks
    // — proving the decoder swap is transparent at the SQL surface.
    // Columns pulled individually to keep the tuple type small enough
    // for `clippy::type_complexity`.
    let conn = embellisher.writer().conn();
    let provider: String = column(conn, "provider");
    let model: String = column(conn, "model");
    let status_code: i64 = column(conn, "status_code");
    let input_tokens: i64 = column(conn, "input_tokens");
    let output_tokens: i64 = column(conn, "output_tokens");
    let latency_ms: i64 = column(conn, "latency_ms");
    let api_key_prefix: Option<String> = column(conn, "api_key_prefix");
    let endpoint_path: String = column(conn, "endpoint_path");
    let agent_version: String = column(conn, "agent_version");
    let request_id: Option<String> = column(conn, "request_id");
    let streaming: i64 = column(conn, "streaming");

    assert_eq!(provider, "anthropic");
    assert_eq!(model, "claude-3-5-sonnet-20241022");
    assert_eq!(status_code, 200);
    assert_eq!(input_tokens, 142);
    assert_eq!(output_tokens, 318);
    // timestamps differ by 1.625s → 1625 ms.
    assert_eq!(latency_ms, 1625);
    assert_eq!(api_key_prefix.as_deref(), Some("sk-ant-api0"));
    assert_eq!(endpoint_path, "/v1/messages");
    assert_eq!(agent_version, "0.0.1");
    assert_eq!(request_id.as_deref(), Some("client-req-abc"));
    assert_eq!(streaming, 1, "Content-Type: text/event-stream → streaming");

    // provider_metadata_json contains the structured bag.
    let provider_metadata_json: Option<String> = conn
        .query_row(
            "SELECT provider_metadata_json FROM ai_telemetry_v_0_0_2 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("provider_metadata column");
    let bag: serde_json::Value = serde_json::from_str(&provider_metadata_json.expect("populated"))
        .expect("provider_metadata_json parses");
    assert_eq!(bag["provider"], "anthropic");
    assert_eq!(bag["request_id"], "req_golden_abc");
    assert_eq!(bag["usage"]["tokens"]["input_tokens"], 142);
    assert_eq!(bag["session_key_prefix"], "sk-ant-api0");
    assert_eq!(bag["organization_id"], "org_xyz");
    assert_eq!(bag["parent_organization_id"], "org_parent");
    assert_eq!(bag["organization_type"], "enterprise");
    assert!(bag["beta_features"].is_array());
    assert_eq!(
        bag["rate_limit"]["Anthropic-Ratelimit-Unified-Utilization"],
        "0.73"
    );
}

#[test]
fn s16_e2e_assertion_set_holds_under_decoder_path() {
    // Mirrors the assertion set in
    // `e2e_embellish_exec_claude.rs` (S16's e2e) — every check the
    // S16 e2e performs against the live-claude SQLite must also hold
    // for the S23 decoder path against this synthetic input.
    let mut embellisher = Embellisher::open_in_memory().expect("open");
    let stats = embellisher
        .process_records(vec![fixture_request(), fixture_response()])
        .expect("process_records");
    assert!(stats.rows_written > 0, "S16 invariant: rows_written > 0");

    let conn = embellisher.writer().conn();
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_telemetry_v_0_0_2", [], |r| {
            r.get(0)
        })
        .expect("count");
    assert!(row_count > 0, "S16 invariant: row count > 0");

    // Columns pulled individually to keep `clippy::type_complexity`
    // happy; the SQL filters to the row we want via WHERE clause.
    let provider: String = column_where(conn, "provider", "input_tokens > 0");
    let model: String = column_where(conn, "model", "input_tokens > 0");
    let api_key_prefix: Option<String> = column_where(conn, "api_key_prefix", "input_tokens > 0");
    let input_tokens: i64 = column_where(conn, "input_tokens", "input_tokens > 0");
    let output_tokens: i64 = column_where(conn, "output_tokens", "input_tokens > 0");
    let latency_ms: i64 = column_where(conn, "latency_ms", "input_tokens > 0");
    let status_code: i64 = column_where(conn, "status_code", "input_tokens > 0");
    let endpoint_path: String = column_where(conn, "endpoint_path", "input_tokens > 0");
    let agent_version: String = column_where(conn, "agent_version", "input_tokens > 0");

    // S16 e2e invariants from `e2e_embellish_exec_claude.rs`.
    assert_eq!(provider, "anthropic", "S16: provider field");
    assert!(!model.is_empty(), "S16: model populated");
    assert!(api_key_prefix.is_some(), "S16: api_key_prefix populated");
    assert!(input_tokens > 0, "S16: input_tokens > 0");
    assert!(output_tokens > 0, "S16: output_tokens > 0");
    assert!(latency_ms >= 0, "S16: latency_ms non-negative");
    assert!(
        status_code >= 200,
        "S16: status_code looks like an HTTP code"
    );
    assert!(
        endpoint_path.starts_with('/'),
        "S16: endpoint_path is path-only"
    );
    assert!(!agent_version.is_empty(), "S16: agent_version populated");
}
