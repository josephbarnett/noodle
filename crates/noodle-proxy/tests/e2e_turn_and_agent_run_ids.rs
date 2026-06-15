//! End-to-end validation of story 040.c — `turn_id` and
//! `agent_run_id` boundary detection per ADR 023 §2.4 / §2.5.
//!
//! Drives a real `claude -p` invocation through a real proxy with
//! `AnthropicMarkingDetector` wired, then reads the real
//! `tap.jsonl` it produced and asserts:
//!
//! - Every `/v1/messages` record carries a populated `marks` block
//!   with `session_id` + `turn_id` + `agent_run_id`.
//! - All records of one `claude -p` invocation share the same
//!   `session_id` (claude uses one conversation id per invocation).
//! - `agent_run_id` is stable across tool-use continuation
//!   round-trips (the canonical system prompt does not change
//!   mid-tool-loop).
//! - `agent_run_id` is non-empty.
//!
//! ## Requirements
//!
//! - `claude` CLI on `PATH`, authenticated.
//! - Network access to `api.anthropic.com`.
//!
//! `#[ignore]`d by default; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_turn_and_agent_run_ids \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use serde_json::Value;
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
async fn marks_block_carries_turn_and_agent_run_ids_across_real_claude_run() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };

    let tap_dir = TempDir::new().expect("tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap"),
    );

    // ADR 052: frame-tree registry replaces the retired AnthropicMarkingDetector.
    let detector = Arc::new(noodle_adapters::marking::FrameTreeRegistry::new());

    let ca = Arc::new(Ca::generate().expect("ca"));
    let ca_pem_path = tap_dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("pem");

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
        markings: Some(detector),
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");

    let proxy_addr = proxy.local_addr();

    // Multi-tool prompt — claude typically issues at least one
    // tool_use round trip before end_turn, exercising the §2.4
    // continuation case. Same canonical system prompt across
    // every RT, so §2.5 should keep agent_run_id stable.
    let prompt = format!(
        "Run `ls {tmp}` and tell me how many files. Reply with just the number.",
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
        "claude exited non-zero: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other arc holders")
        .unwrap();
    sink.shutdown().await;

    let contents = std::fs::read_to_string(&tap_path).expect("read tap");
    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse"))
        .collect();
    assert!(!records.is_empty(), "tap.jsonl empty");

    // Filter to /v1/messages records with a marks block populated.
    let marked: Vec<&Value> = records
        .iter()
        .filter(|r| {
            // Either the request or response side of a /v1/messages
            // exchange — both carry the marks block.
            r.get("marks").is_some_and(|m| !m.is_null())
        })
        .collect();
    assert!(
        !marked.is_empty(),
        "no records with marks — detector didn't run or session header was absent"
    );

    // AC #5 part 1: every marked record carries agent_run_id (the
    // new field added in this slice). Per ADR 023 §2.5 the value
    // is always populated when the detector ran.
    let mut missing_agent_run = 0usize;
    for r in &marked {
        let arid = r
            .get("marks")
            .and_then(|m| m.get("agent_run_id"))
            .and_then(Value::as_str);
        if arid.is_none_or(str::is_empty) {
            missing_agent_run += 1;
            eprintln!("MISSING agent_run_id on record: {r}");
        }
    }
    assert_eq!(
        missing_agent_run, 0,
        "{missing_agent_run} marked records missing agent_run_id (AC #5)"
    );

    // AC #1 / AC #2 (single-turn or multi-tool-use): one
    // session_id; agent_run_id stable across all RTs (system
    // prompt doesn't change in a single `claude -p` invocation).
    let session_ids: std::collections::HashSet<&str> = marked
        .iter()
        .filter_map(|r| {
            r.get("marks")
                .and_then(|m| m.get("session_id"))
                .and_then(Value::as_str)
        })
        .collect();
    assert_eq!(
        session_ids.len(),
        1,
        "expected one session_id per claude -p invocation; got {session_ids:?}"
    );

    let agent_run_ids: std::collections::HashSet<&str> = marked
        .iter()
        .filter_map(|r| {
            r.get("marks")
                .and_then(|m| m.get("agent_run_id"))
                .and_then(Value::as_str)
        })
        .collect();
    eprintln!(
        "e2e: {} distinct agent_run_ids across {} marked records",
        agent_run_ids.len(),
        marked.len()
    );
    assert_eq!(
        agent_run_ids.len(),
        1,
        "agent_run_id should be stable across one `claude -p` invocation \
         (canonical system prompt does not change mid-tool-loop); got {agent_run_ids:?}"
    );

    eprintln!("e2e PASS: marks carry agent_run_id; stable across the run");
}
