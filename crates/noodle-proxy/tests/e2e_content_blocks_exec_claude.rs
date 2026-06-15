//! End-to-end validation of `content.blocks[]` on `tap.jsonl`
//! response records (S9 of the 027–031 refactor; ADR 030 §2;
//! refactor-overview.md §2 S9).
//!
//! Per ADR 030 §2, every response record routed through the
//! proxy must carry decoded content blocks alongside the raw
//! body bytes. The proxy walks the Anthropic SSE stream's
//! `content_block_start` / `content_block_delta` /
//! `content_block_stop` events and assembles typed blocks
//! (`text`, `thinking`, `tool_use`) on the wire log.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API, then reads the real
//! `tap.jsonl` and asserts:
//!
//! 1. At least one anthropic response record carries
//!    `content.blocks[]` with at least one entry.
//! 2. At least one record's blocks include a `text` block.
//! 3. At least one record's blocks include a `tool_use` block
//!    (claude-code makes tool calls; in a real session you'll
//!    see them).
//! 4. Observed block kinds are printed for human verification.
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_content_blocks_exec_claude \
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
async fn content_blocks_populated_on_real_tap_jsonl() {
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

    // Prompt is shaped so claude-code is highly likely to call
    // at least one tool (Bash + Read or LS) — exercises the
    // tool_use block code path in a single round-trip.
    let prompt = format!(
        "Run `ls {tmp}` and then briefly describe what's in the directory.",
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

    // Filter to api.anthropic.com responses — that's where
    // Anthropic emits content blocks on SSE.
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

    assert!(
        !anthropic_responses.is_empty(),
        "no anthropic response records in tap.jsonl — claude didn't \
         reach api.anthropic.com through the proxy"
    );

    // ─── Walk every anthropic response, collect block kinds ─────
    //
    // Track which kinds we've seen across the full session and
    // count records that carried at least one block. The §S9
    // demonstrable outcome (refactor-overview.md §2) requires
    // at least one record to carry `content.blocks[*].kind`
    // populated as `text` / `thinking` / `tool_use`; for a
    // real claude session that exercises tools, we should
    // see both `text` AND `tool_use`.

    let mut kinds_seen: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut records_with_blocks = 0usize;
    let mut total_blocks = 0usize;

    for rec in &anthropic_responses {
        let Some(blocks) = rec.pointer("/content/blocks").and_then(Value::as_array) else {
            continue;
        };
        if blocks.is_empty() {
            continue;
        }
        records_with_blocks += 1;
        total_blocks += blocks.len();
        for b in blocks {
            if let Some(k) = b.get("kind").and_then(Value::as_str) {
                *kinds_seen.entry(k.to_string()).or_default() += 1;
            }
        }
    }

    eprintln!("e2e: records with content.blocks[]: {records_with_blocks}");
    eprintln!("e2e: total blocks observed: {total_blocks}");
    eprintln!("e2e: block kinds observed: {kinds_seen:?}");

    assert!(
        records_with_blocks > 0,
        "no anthropic response record carries content.blocks[]. \
         Anthropic responses: {} but none with blocks. First record: {}",
        anthropic_responses.len(),
        serde_json::to_string_pretty(anthropic_responses.first().unwrap()).unwrap_or_default(),
    );

    let text_count = kinds_seen.get("text").copied().unwrap_or(0);
    let tool_use_count = kinds_seen.get("tool_use").copied().unwrap_or(0);
    let thinking_count = kinds_seen.get("thinking").copied().unwrap_or(0);

    assert!(
        text_count > 0,
        "no `text` block observed across {records_with_blocks} records. \
         kinds: {kinds_seen:?}",
    );
    assert!(
        tool_use_count > 0,
        "no `tool_use` block observed across {records_with_blocks} records. \
         claude-code should make at least one tool call for the test prompt. \
         kinds: {kinds_seen:?}",
    );

    // Spot-check a tool_use block carries the per-ADR-030 §2.2
    // shape (`tool_use_id`, `tool_name`, `input`) — a downstream
    // consumer pattern-matching on these field names should not
    // see any required field missing.
    for rec in &anthropic_responses {
        let Some(blocks) = rec.pointer("/content/blocks").and_then(Value::as_array) else {
            continue;
        };
        for b in blocks {
            if b.get("kind").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            assert!(
                b.get("tool_use_id").is_some(),
                "tool_use block missing tool_use_id: {b}"
            );
            assert!(
                b.get("tool_name").is_some(),
                "tool_use block missing tool_name: {b}"
            );
            assert!(
                b.get("input").is_some(),
                "tool_use block missing input: {b}"
            );
            // First tool_use observed is enough — bail.
            break;
        }
    }

    eprintln!(
        "e2e: PASS — refactor-overview §2 S9 verified end-to-end. \
         text_blocks={text_count}, tool_use_blocks={tool_use_count}, \
         thinking_blocks={thinking_count}, total_records_with_blocks={records_with_blocks}",
    );
}
