//! End-to-end validation of the full `noodle → embellish → SQLite`
//! pipeline (S16 of the 027–031 refactor; ADR 031 §1 architecture
//! diagram).
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is NOT
//! acceptable for tap.jsonl contracts — the only validating test
//! is one that spawns the real `claude` CLI through the real
//! proxy and asserts on the `SQLite` the real embellisher wrote.
//!
//! The harness:
//!
//! 1. Spawns a real noodle proxy with file-based wire sink.
//! 2. Spawns real `claude` CLI through `HTTPS_PROXY=noodle`.
//! 3. Waits for claude to exit; shuts the proxy down cleanly.
//! 4. Runs [`Embellisher`] against the captured `tap.jsonl`.
//! 5. Opens the resulting `SQLite` with `rusqlite` and asserts:
//!    - `schema_version` row matches ai-telemetry v0.0.2,
//!    - at least one row in `ai_telemetry_v_0_0_2`,
//!    - spot-check fields: `provider == "anthropic"`,
//!      `api_key_prefix` populated, `input_tokens > 0`,
//!      `model` non-empty, `latency_ms >= 0`.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-embellish --test e2e_embellish_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_embellish::{Embellisher, SCHEMA_ID, SCHEMA_VERSION};
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::process::Command;

fn claude_binary() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn embellish_produces_sqlite_from_real_claude_session() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    let db_path = tap_dir.path().join("events.sqlite");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());
    eprintln!("e2e: sqlite db path: {}", db_path.display());

    // ─── stage 1: spin up real noodle ──────────────────────────────
    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );
    let ca = Arc::new(Ca::generate().expect("generate test CA"));
    let ca_pem_path = tap_dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("write CA pem");

    let proxy = start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 8 * 1024 * 1024,
        wire: tap_sink.clone(),
        codecs: None,
        engine: None,
        filters: Vec::new(),
        enhancers: Vec::new(),
        context: None,
        sessions: Arc::new(InMemorySessionStore::new()),
        ca: Arc::clone(&ca),
        markings: None,
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");

    let proxy_addr = proxy.local_addr();
    eprintln!("e2e: noodle proxy listening on {proxy_addr}");

    // ─── stage 2: spawn real claude CLI ────────────────────────────
    let prompt = format!(
        "Run `ls {tmp}` and tell me how many files are in the directory. \
         Reply with just the number.",
        tmp = tap_dir.path().display()
    );

    let output = Command::new(&claude_bin)
        .arg("-p")
        .arg(&prompt)
        .env("HTTPS_PROXY", format!("http://{proxy_addr}"))
        .env("NODE_EXTRA_CA_CERTS", &ca_pem_path)
        .env("https_proxy", format!("http://{proxy_addr}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn claude");

    assert!(
        output.status.success(),
        "claude exited non-zero: {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // Sanity-check the tap.jsonl exists and is non-empty before we
    // hand it to the embellisher.
    let tap_size = std::fs::metadata(&tap_path).expect("stat tap.jsonl").len();
    assert!(tap_size > 0, "tap.jsonl is empty — proxy wrote nothing");
    eprintln!("e2e: tap.jsonl size: {tap_size} bytes");

    // ─── stage 3: run embellisher in-process ───────────────────────
    let mut embellisher = Embellisher::open(&db_path).expect("open sqlite db");
    let stats = embellisher
        .process_file(&tap_path)
        .expect("embellisher.process_file");
    eprintln!(
        "e2e: embellisher stats: records_read={} requests={} responses={} \
         rows_written={} unpaired_req={} orphan_resp={}",
        stats.records_read,
        stats.requests,
        stats.responses,
        stats.rows_written,
        stats.unpaired_requests,
        stats.orphan_responses
    );

    assert!(
        stats.rows_written > 0,
        "no telemetry rows written — claude session produced no paired requests/responses"
    );
    // Drop the in-process embellisher so the SQLite file's WAL is
    // flushed before we re-open it read-only below.
    drop(embellisher);

    // ─── stage 4: open SQLite and assert ───────────────────────────
    let conn = Connection::open(&db_path).expect("reopen sqlite for verification");

    // schema_version row matches the pinned v0.0.2 contract.
    let (sid, sver): (String, String) = conn
        .query_row(
            "SELECT schema_id, schema_version FROM schema_version",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("schema_version row present");
    assert_eq!(sid, SCHEMA_ID, "schema_id drift");
    assert_eq!(sver, SCHEMA_VERSION, "schema_version drift");
    eprintln!("e2e: schema_version row: {sid} v{sver}");

    // Total rows.
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_telemetry_v_0_0_2", [], |r| {
            r.get(0)
        })
        .expect("count rows");
    assert!(
        row_count > 0,
        "ai_telemetry_v_0_0_2 is empty — embellisher wrote schema but no rows"
    );
    eprintln!("e2e: ai_telemetry_v_0_0_2 row count: {row_count}");

    // Spot-check a row. ADR 031 §5 mapping calls for:
    //   - provider == "anthropic"
    //   - api_key_prefix populated (S5 + S7)
    //   - input_tokens > 0 (S8)
    //   - model non-empty (from request body)
    //   - latency_ms >= 0
    let mut stmt = conn
        .prepare(
            "SELECT provider, model, api_key_prefix, input_tokens, output_tokens, \
                    latency_ms, status_code, endpoint_path, agent_version \
             FROM ai_telemetry_v_0_0_2 \
             WHERE input_tokens > 0 \
             LIMIT 1",
        )
        .expect("prepare spot-check query");
    let mut rows = stmt.query([]).expect("query spot-check row");
    let row = rows
        .next()
        .expect("step row")
        .expect("at least one row with input_tokens > 0");

    let provider: String = row.get(0).expect("provider");
    let model: String = row.get(1).expect("model");
    let api_key_prefix: Option<String> = row.get(2).expect("api_key_prefix");
    let input_tokens: i64 = row.get(3).expect("input_tokens");
    let output_tokens: i64 = row.get(4).expect("output_tokens");
    let latency_ms: i64 = row.get(5).expect("latency_ms");
    let status_code: i64 = row.get(6).expect("status_code");
    let endpoint_path: String = row.get(7).expect("endpoint_path");
    let agent_version: String = row.get(8).expect("agent_version");

    assert_eq!(provider, "anthropic", "provider field");
    assert!(!model.is_empty(), "model populated");
    assert!(
        api_key_prefix.is_some(),
        "api_key_prefix populated (S5/S7 must be on the wire)"
    );
    let prefix = api_key_prefix.unwrap();
    assert!(!prefix.is_empty(), "api_key_prefix non-empty");
    assert!(input_tokens > 0, "input_tokens > 0");
    assert!(output_tokens > 0, "output_tokens > 0");
    assert!(latency_ms >= 0, "latency_ms non-negative");
    assert!(status_code >= 200, "status_code looks like an HTTP code");
    assert!(
        endpoint_path.starts_with('/'),
        "endpoint_path is path-only: {endpoint_path}"
    );
    assert!(!agent_version.is_empty(), "agent_version populated (S6)");

    // First 4 chars of the prefix is auditable without leaking the
    // full credential.
    let truncated = &prefix[..prefix.len().min(4)];
    eprintln!(
        "e2e: spot-checked row — provider={provider} model={model} \
         api_key_prefix={truncated}… input={input_tokens} output={output_tokens} \
         latency={latency_ms}ms status={status_code} endpoint={endpoint_path} \
         agent={agent_version}"
    );

    // provider_metadata_json round-trips as valid JSON containing
    // `provider`. This proves the bag-construction path didn't ship
    // garbage.
    let metadata_json: Option<String> = conn
        .query_row(
            "SELECT provider_metadata_json FROM ai_telemetry_v_0_0_2 WHERE input_tokens > 0 LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("query metadata json");
    let metadata = metadata_json.expect("provider_metadata_json populated");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("metadata parses");
    assert_eq!(parsed["provider"], "anthropic");

    eprintln!(
        "e2e: PASS — noodle → embellish → SQLite pipeline (S16, ADR 031) verified end-to-end"
    );
}
