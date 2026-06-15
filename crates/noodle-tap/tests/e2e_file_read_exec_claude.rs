//! Exec-claude e2e for `WireSource::FileRead` (refactor overview §2 S13).
//!
//! Spawns the real `claude` CLI through a real noodle proxy. After claude
//! exits and the proxy is shut down (flushing the writer task), opens
//! `FileRead` on the same `tap.jsonl` and collects every record to EOF.
//!
//! Asserts:
//! 1. The reader observed at least one record (claude did reach the
//!    proxy).
//! 2. Every record parses cleanly (no surprise on-disk shape).
//! 3. The read count matches the on-disk line count exactly (batch
//!    reader never misses or duplicates records the proxy wrote).
//!
//! Per the noodle e2e contract (AGENTS.md §"End-to-end test discipline"
//! and global memory `feedback_no_fixture_extraction`), fixture replay
//! is not acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-tap --test e2e_file_read_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{WireSink, WireSource};
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tap::source::FileRead;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn file_read_collects_records_from_real_claude_session() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("tempdir");
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
        wire: Arc::clone(&tap_sink) as Arc<dyn WireSink>,
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

    // ── Drive claude through noodle ────────────────────────────
    let prompt = "Reply with just the word: hello".to_owned();

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
    eprintln!(
        "e2e: claude stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Give the writer task a beat to flush the last records.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shut down proxy + sink — forces a final flush and makes the file
    // finite. After this, no more bytes can land in tap_path.
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // ── Read the finished file with FileRead to EOF ─────────────
    let mut rd = FileRead::open(&tap_path).expect("open file read");
    let mut collected: Vec<Value> = Vec::new();
    loop {
        match rd.next_record() {
            Ok(Some(v)) => collected.push(v),
            Ok(None) => break,
            Err(e) => panic!("FileRead error: {e}"),
        }
    }
    // EOF must be idempotent.
    assert!(
        rd.next_record().expect("post-EOF").is_none(),
        "FileRead yielded after first EOF"
    );

    // ── Compare to on-disk ────────────────────────────────────
    let on_disk = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let on_disk_lines: Vec<&str> = on_disk.lines().filter(|l| !l.trim().is_empty()).collect();
    eprintln!(
        "e2e: FileRead observed {} records ; on disk: {} records",
        collected.len(),
        on_disk_lines.len()
    );

    assert!(
        !collected.is_empty(),
        "FileRead saw zero records — claude didn't reach the proxy or proxy didn't flush"
    );
    assert!(
        !on_disk_lines.is_empty(),
        "proxy wrote zero tap.jsonl records — claude didn't reach the proxy"
    );
    assert_eq!(
        collected.len(),
        on_disk_lines.len(),
        "FileRead count diverges from on-disk count; read={} disk={}",
        collected.len(),
        on_disk_lines.len()
    );

    // Spot-check: every record has a direction + event_id (proves
    // parsing succeeded on every line).
    for (i, rec) in collected.iter().enumerate() {
        let dir = rec.get("direction").and_then(Value::as_str);
        let id = rec.get("event_id").and_then(Value::as_str);
        assert!(
            dir == Some("request") || dir == Some("response"),
            "record {i} bad direction: {rec}"
        );
        assert!(id.is_some(), "record {i} missing event_id: {rec}");
    }

    let req_count = collected
        .iter()
        .filter(|r| {
            r.get("direction").and_then(Value::as_str) == Some("request")
                && r.get("provider").and_then(Value::as_str) == Some("anthropic")
        })
        .count();
    let resp_count = collected
        .iter()
        .filter(|r| {
            r.get("direction").and_then(Value::as_str) == Some("response")
                && r.get("provider").and_then(Value::as_str) == Some("anthropic")
        })
        .count();
    eprintln!("e2e: anthropic req={req_count} resp={resp_count}");
    assert!(
        req_count > 0,
        "no anthropic request records in FileRead output"
    );
    assert!(
        resp_count > 0,
        "no anthropic response records in FileRead output"
    );

    // Order check: read event_ids match on-disk event_ids in order.
    for (i, (live, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let from_disk: Value = serde_json::from_str(raw).expect("parse on-disk line");
        let live_id = live.get("event_id").and_then(Value::as_str);
        let disk_id = from_disk.get("event_id").and_then(Value::as_str);
        assert_eq!(
            live_id, disk_id,
            "record {i}: read event_id {live_id:?} != on-disk {disk_id:?}"
        );
    }
    eprintln!(
        "e2e: PASS — FileRead observed {} records matching on-disk; anthropic req={req_count} resp={resp_count}",
        collected.len()
    );
}
