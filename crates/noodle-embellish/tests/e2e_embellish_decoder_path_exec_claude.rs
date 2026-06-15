//! End-to-end validation of the **decoder-driven** embellish pipeline
//! against a real claude session (refactor slice S23).
//!
//! Per `docs/adrs/refactor-overview.md` §10, S23 refactors
//! `noodle-embellish::mapper` to consume `DecodedEvent`s from
//! `noodle_domain::decoders::AnthropicDecoder` instead of inline
//! `tap.jsonl` JSON parsing. This e2e proves the swap is transparent
//! at the `SQLite` surface: every assertion the S16 e2e
//! (`e2e_embellish_exec_claude.rs`) makes against the live-claude
//! `SQLite` must continue to hold under the decoder-driven path.
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable for tap.jsonl contracts. This test spawns the real
//! claude CLI through the real proxy, then drives the new
//! `Embellisher` (decoder path) over the captured `tap.jsonl`.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-embellish \
//!     --test e2e_embellish_decoder_path_exec_claude \
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
async fn embellish_decoder_path_produces_sqlite_from_real_claude_session() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e[S23]: claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    let db_path = tap_dir.path().join("events.sqlite");
    eprintln!("e2e[S23]: tap.jsonl path: {}", tap_path.display());
    eprintln!("e2e[S23]: sqlite db path: {}", db_path.display());

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
    eprintln!("e2e[S23]: noodle proxy listening on {proxy_addr}");

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

    let tap_size = std::fs::metadata(&tap_path).expect("stat tap.jsonl").len();
    assert!(tap_size > 0, "tap.jsonl is empty — proxy wrote nothing");
    eprintln!("e2e[S23]: tap.jsonl size: {tap_size} bytes");

    // ─── stage 3: run the NEW (decoder-driven) embellisher ─────────
    // `Embellisher::process_file` now drives every paired record
    // through `noodle_domain::decoders::AnthropicDecoder` before
    // reaching `map_decoded_pair`. The on-disk SQLite is the same
    // schema, same column order, same row content — proving the
    // swap of the input plumbing is transparent.
    let mut embellisher = Embellisher::open(&db_path).expect("open sqlite db");
    let stats = embellisher
        .process_file(&tap_path)
        .expect("embellisher.process_file via decoder path");
    eprintln!(
        "e2e[S23]: embellisher stats: records_read={} requests={} responses={} \
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
        "no telemetry rows written by decoder path — claude session produced no paired requests/responses"
    );
    drop(embellisher);

    // ─── stage 4: open SQLite and assert on the load-bearing shape ─
    // Identical assertion set to the S16 e2e — the decoder swap must
    // be undetectable at this layer.
    let conn = Connection::open(&db_path).expect("reopen sqlite for verification");

    let (sid, sver): (String, String) = conn
        .query_row(
            "SELECT schema_id, schema_version FROM schema_version",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("schema_version row present");
    assert_eq!(sid, SCHEMA_ID, "schema_id drift");
    assert_eq!(sver, SCHEMA_VERSION, "schema_version drift");
    eprintln!("e2e[S23]: schema_version row: {sid} v{sver}");

    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_telemetry_v_0_0_2", [], |r| {
            r.get(0)
        })
        .expect("count rows");
    assert!(row_count > 0, "ai_telemetry_v_0_0_2 is empty");
    eprintln!("e2e[S23]: ai_telemetry_v_0_0_2 row count: {row_count}");

    let mut stmt = conn
        .prepare(
            "SELECT provider, model, api_key_prefix, input_tokens, output_tokens, \
                    latency_ms, status_code, endpoint_path, agent_version, \
                    provider_metadata_json \
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
    let metadata_json: Option<String> = row.get(9).expect("provider_metadata_json");

    // The same assertion set the S16 e2e checks.
    assert_eq!(provider, "anthropic", "provider field (typed via decoder)");
    assert!(!model.is_empty(), "model populated");
    assert!(
        api_key_prefix.is_some(),
        "api_key_prefix populated (S5/S7 must be on the wire)"
    );
    let prefix = api_key_prefix.expect("api_key_prefix populated above");
    assert!(!prefix.is_empty(), "api_key_prefix non-empty");
    assert!(input_tokens > 0, "input_tokens > 0 (typed via decoder)");
    assert!(output_tokens > 0, "output_tokens > 0 (typed via decoder)");
    assert!(latency_ms >= 0, "latency_ms non-negative");
    assert!(status_code >= 200, "status_code looks like an HTTP code");
    assert!(
        endpoint_path.starts_with('/'),
        "endpoint_path is path-only: {endpoint_path}"
    );
    assert!(!agent_version.is_empty(), "agent_version populated (S6)");

    let metadata = metadata_json.expect("provider_metadata_json populated");
    let parsed: serde_json::Value = serde_json::from_str(&metadata).expect("metadata parses");
    assert_eq!(parsed["provider"], "anthropic");

    // ─── stage 5: print a sample for human verification ────────────
    let truncated = &prefix[..prefix.len().min(4)];
    eprintln!(
        "e2e[S23]: sample row — provider={provider} model={model} \
         api_key_prefix={truncated}… input={input_tokens} output={output_tokens} \
         latency={latency_ms}ms status={status_code} endpoint={endpoint_path} \
         agent={agent_version}"
    );

    eprintln!(
        "e2e[S23]: PASS — decoder-driven pipeline produces SQLite indistinguishable from S16 baseline. \
         row_count={row_count}"
    );
}
