//! End-to-end validation of tool-use cross-record pairing on
//! `tap.jsonl` (S11 of the 027–031 refactor; ADR 030 §4;
//! refactor-overview.md §2 S11).
//!
//! Per ADR 030 §4, a `tool_use` block emitted on a response
//! record is paired with the matching `tool_result` that arrives
//! on a subsequent request record. The proxy stamps the back-
//! reference (`pairing.resolves_tool_use_in_request_id`) on the
//! request record and emits a `direction: "patch"` record that
//! back-fills the forward reference
//! (`pairing.resolved_by_request_id`) on the prior response
//! record per ADR 030 §4.3 / §7.3.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API. claude-code makes many tool
//! calls in any non-trivial session, so the trace produced by
//! a single run typically contains multiple pairable
//! `tool_use` / `tool_result` round trips. The test reads the
//! real `tap.jsonl`, walks it, and asserts:
//!
//! 1. At least one anthropic response record's
//!    `content.blocks[]` contains a `tool_use` block.
//! 2. At least one subsequent anthropic request record carries
//!    `pairing.resolves_tool_use_in_request_id` pointing back
//!    at a prior `event_id`.
//! 3. At least one `patch` record on `tap.jsonl` targets a
//!    prior response record's `event_id` with a
//!    `pairing.resolved_by_request_id` update.
//! 4. The pair is coherent: the request's
//!    `resolves_tool_use_in_request_id` value equals the
//!    `target_request_id` of a matching patch record (i.e. both
//!    halves of the same pair agree on the prior response's id).
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy \
//!     --test e2e_tool_use_pairing_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::collections::HashMap;
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
async fn tool_use_pairing_populated_on_real_tap_jsonl() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 2048)
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

    // Prompt is shaped so claude-code is highly likely to invoke
    // multiple tools in sequence (Bash + LS + Read) — exercises
    // the pairing path across several round-trips in a single
    // session.
    let prompt = format!(
        "Run `ls -la {tmp}`, then create a file called test.txt with the \
         content 'hello world', then read it back and tell me what's in it.",
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

    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap.jsonl line"))
        .collect();
    eprintln!("e2e: {} total tap records", records.len());

    // ─── Index 1: tool_use blocks observed on anthropic response records ─────
    //
    // Map `tool_use_id` → `event_id` (the response record's id).
    // S11's response-side hook registers these in the
    // pending-tool-uses table at flow close.
    let mut tool_use_origins: HashMap<String, String> = HashMap::new();
    for rec in &records {
        if rec.get("direction").and_then(Value::as_str) != Some("response") {
            continue;
        }
        if rec.get("provider").and_then(Value::as_str) != Some("anthropic") {
            continue;
        }
        let Some(event_id) = rec.get("event_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(blocks) = rec.pointer("/content/blocks").and_then(Value::as_array) else {
            continue;
        };
        for b in blocks {
            if b.get("kind").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            if let Some(tu_id) = b.get("tool_use_id").and_then(Value::as_str) {
                tool_use_origins.insert(tu_id.to_string(), event_id.to_string());
            }
        }
    }
    eprintln!(
        "e2e: observed {} unique tool_use ids across anthropic responses",
        tool_use_origins.len(),
    );

    assert!(
        !tool_use_origins.is_empty(),
        "no tool_use blocks observed on any anthropic response — \
         pairing has nothing to test. Records: {}",
        records.len(),
    );

    // ─── Index 2: request-side pairing back-references ─────────────────
    //
    // Every anthropic request record whose pairing block points
    // at a prior response id: (request_event_id, prior_response_event_id).
    let mut request_pairings: Vec<(String, String)> = Vec::new();
    for rec in &records {
        if rec.get("direction").and_then(Value::as_str) != Some("request") {
            continue;
        }
        if rec.get("provider").and_then(Value::as_str) != Some("anthropic") {
            continue;
        }
        let Some(event_id) = rec.get("event_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(resolves) = rec
            .pointer("/pairing/resolves_tool_use_in_request_id")
            .and_then(Value::as_str)
        else {
            continue;
        };
        request_pairings.push((event_id.to_string(), resolves.to_string()));
    }
    eprintln!(
        "e2e: observed {} request-side pairing back-references",
        request_pairings.len(),
    );

    // ─── Index 3: patch records back-filling response-side forward references ─────
    //
    // (target_request_id, resolved_by_request_id) per patch.
    let mut patch_records: Vec<(String, String)> = Vec::new();
    for rec in &records {
        if rec.get("direction").and_then(Value::as_str) != Some("patch") {
            continue;
        }
        let Some(target) = rec.get("target_request_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(patches) = rec.get("patches").and_then(Value::as_array) else {
            continue;
        };
        for p in patches {
            if p.get("path").and_then(Value::as_str) != Some("pairing.resolved_by_request_id") {
                continue;
            }
            if let Some(value) = p.get("value").and_then(Value::as_str) {
                patch_records.push((target.to_string(), value.to_string()));
            }
        }
    }
    eprintln!(
        "e2e: observed {} pairing patch records on tap.jsonl",
        patch_records.len(),
    );

    // ─── Assertions ─────────────────────────────────────────────────
    //
    // The S11 demonstrable outcome (refactor overview §2 S11):
    // "a `tool_use` in record N carries `pairing.resolved_by_request_id`;
    //  matching `tool_result` in record N+k carries
    //  `pairing.resolves_tool_use_in_request_id`."
    //
    // For this run we assert at least one full pair lights up:
    //  - at least one request carries `resolves_tool_use_in_request_id`
    //  - at least one patch back-fills `resolved_by_request_id`
    //  - the two halves agree on the same prior response's event_id
    //
    // (Pairing might be incomplete in rare cases — e.g. proxy
    // restart mid-session or table eviction. For a single fresh
    // claude session the table never fills.)

    assert!(
        !request_pairings.is_empty(),
        "S11 contract: no request record carries \
         `pairing.resolves_tool_use_in_request_id`. \
         Observed {} tool_use ids in responses, {} patch records. \
         Records: {}",
        tool_use_origins.len(),
        patch_records.len(),
        records.len(),
    );

    assert!(
        !patch_records.is_empty(),
        "S11 contract: no `direction: patch` record carries \
         `pairing.resolved_by_request_id`. \
         Observed {} tool_use ids in responses, {} request pairings. \
         Records: {}",
        tool_use_origins.len(),
        request_pairings.len(),
        records.len(),
    );

    // Pair coherence: at least one (request → resolves prior
    // response) must also have a corresponding patch (patch
    // targets the same prior response → resolved_by this request).
    let mut coherent_pairs: Vec<(String, String)> = Vec::new();
    for (req_id, resolves_id) in &request_pairings {
        // Find a patch that targets the prior response and
        // back-resolves to THIS request.
        if patch_records
            .iter()
            .any(|(tgt, by)| tgt == resolves_id && by == req_id)
        {
            coherent_pairs.push((req_id.clone(), resolves_id.clone()));
        }
    }
    eprintln!("e2e: coherent pairs: {}", coherent_pairs.len());
    for (req, resp) in &coherent_pairs {
        eprintln!("  request {req} ↔ response {resp}");
    }

    assert!(
        !coherent_pairs.is_empty(),
        "S11 coherence: no (request, prior-response) pair has \
         BOTH halves of the pairing populated. Request pairings: \
         {request_pairings:?}; patches: {patch_records:?}",
    );

    eprintln!(
        "e2e: PASS — refactor-overview §2 S11 verified end-to-end. \
         tool_use_blocks={}, request_pairings={}, patches={}, coherent={}",
        tool_use_origins.len(),
        request_pairings.len(),
        patch_records.len(),
        coherent_pairs.len(),
    );
}
