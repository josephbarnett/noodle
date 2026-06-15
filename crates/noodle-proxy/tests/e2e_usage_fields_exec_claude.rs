//! End-to-end validation of `usage.tokens` and `usage.latency`
//! on `tap.jsonl` response records (S8 of the 027–031 refactor;
//! ADR 029 §2.4 family 12; refactor-overview.md §2 S8).
//!
//! Per the ADR and refactor plan, every response record routed
//! through the proxy must carry typed token-count and latency
//! data on the wire log. Anthropic emits the canonical counts
//! under `usage.{input_tokens,output_tokens,cache_read_input_tokens,...}`
//! on the SSE `message_delta` event; the proxy measures
//! request-send → first-byte and request-send → close to derive
//! latency.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API, then reads the real
//! `tap.jsonl` and asserts:
//!
//! 1. At least one response record against
//!    `api.anthropic.com/v1/messages` carries `usage.tokens.input_tokens > 0`.
//! 2. The same record carries `usage.tokens.output_tokens > 0`.
//! 3. The same record carries `usage.latency.total_ms > 0`.
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_usage_fields_exec_claude \
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
async fn usage_fields_populated_on_real_tap_jsonl() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

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

    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap.jsonl line"))
        .collect();
    eprintln!("e2e: {} total tap records", records.len());

    // Filter to api.anthropic.com /v1/messages responses — that's
    // where Anthropic emits the canonical usage block.
    let anthropic_responses: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("direction").and_then(Value::as_str) == Some("response")
                && r.get("provider").and_then(Value::as_str) == Some("anthropic")
        })
        .collect();
    eprintln!(
        "e2e: {} response records against anthropic provider",
        anthropic_responses.len()
    );

    // We want to see at least one anthropic response on the wire
    // — if zero, the test is meaningless (claude went around the
    // proxy somehow). Same defensive check as
    // `e2e_redaction_exec_claude`.
    assert!(
        !anthropic_responses.is_empty(),
        "no anthropic response records in tap.jsonl — claude didn't \
         reach api.anthropic.com through the proxy"
    );

    // ─── Find the first record with a non-zero usage payload ────
    //
    // Some responses don't carry token counts on the wire
    // (errors, early aborts, non-/v1/messages endpoints), so
    // we scan for the first one that does. The slice contract
    // (refactor-overview.md §2 S8) is "a response record carries
    // usage.tokens.input_tokens, output_tokens, cache_read_input_tokens,
    // etc." — at least one record must satisfy the full positive
    // assertion.

    let mut observed_input_tokens: Option<u64> = None;
    let mut observed_output_tokens: Option<u64> = None;
    let mut observed_total_ms: Option<u64> = None;
    let mut observed_with_all_three: Option<&Value> = None;

    for rec in &anthropic_responses {
        let Some(usage) = rec.get("usage") else {
            continue;
        };
        let input = usage
            .pointer("/tokens/input_tokens")
            .and_then(Value::as_u64);
        let output = usage
            .pointer("/tokens/output_tokens")
            .and_then(Value::as_u64);
        let total = usage.pointer("/latency/total_ms").and_then(Value::as_u64);

        if let Some(v) = input
            && v > 0
            && observed_input_tokens.is_none()
        {
            observed_input_tokens = Some(v);
        }
        if let Some(v) = output
            && v > 0
            && observed_output_tokens.is_none()
        {
            observed_output_tokens = Some(v);
        }
        if let Some(v) = total
            && v > 0
            && observed_total_ms.is_none()
        {
            observed_total_ms = Some(v);
        }
        if matches!(input, Some(i) if i > 0)
            && matches!(output, Some(o) if o > 0)
            && matches!(total, Some(t) if t > 0)
            && observed_with_all_three.is_none()
        {
            observed_with_all_three = Some(rec);
        }
    }

    eprintln!(
        "e2e: observed token counts — input={observed_input_tokens:?}, \
         output={observed_output_tokens:?}, total_ms={observed_total_ms:?}",
    );

    let exemplar = observed_with_all_three.unwrap_or_else(|| {
        panic!(
            "no anthropic response record carries all of \
             usage.tokens.input_tokens > 0, output_tokens > 0, latency.total_ms > 0. \
             Observed: input={observed_input_tokens:?}, output={observed_output_tokens:?}, \
             total_ms={observed_total_ms:?}. \
             First 2 anthropic responses for diagnosis: {:?}",
            anthropic_responses.iter().take(2).collect::<Vec<_>>(),
        );
    });

    let usage = exemplar
        .get("usage")
        .expect("exemplar has usage (by construction above)");
    let input_tokens = usage
        .pointer("/tokens/input_tokens")
        .and_then(Value::as_u64)
        .expect("exemplar has input_tokens");
    let output_tokens = usage
        .pointer("/tokens/output_tokens")
        .and_then(Value::as_u64)
        .expect("exemplar has output_tokens");
    let total_ms = usage
        .pointer("/latency/total_ms")
        .and_then(Value::as_u64)
        .expect("exemplar has latency.total_ms");

    assert!(
        input_tokens > 0,
        "input_tokens must be > 0 on the exemplar record"
    );
    assert!(
        output_tokens > 0,
        "output_tokens must be > 0 on the exemplar record"
    );
    assert!(
        total_ms > 0,
        "latency.total_ms must be > 0 on the exemplar record"
    );

    eprintln!(
        "e2e: PASS — refactor-overview §2 S8 verified end-to-end. \
         input_tokens={input_tokens}, output_tokens={output_tokens}, total_ms={total_ms}",
    );
}
