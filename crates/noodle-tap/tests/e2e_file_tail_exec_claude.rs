//! Exec-claude e2e for `WireSource::FileTail` (refactor overview §2 S12).
//!
//! Spawns the real `claude` CLI through a real noodle proxy and opens
//! `FileTail` on the same `tap.jsonl` that the proxy is writing. The
//! tail runs on a background blocking thread and accumulates records
//! while claude executes. After claude exits, the test asserts:
//!
//! 1. The tail observed at least one anthropic request/response pair.
//! 2. The live-tail record count matches the on-disk record count
//!    (live tail never misses or duplicates records that the proxy
//!    actually wrote).
//!
//! Per the noodle e2e contract (AGENTS.md §"End-to-end test discipline"
//! and global memory `feedback_no_fixture_extraction`), fixture replay
//! is not acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-tap --test e2e_file_tail_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{WireSink, WireSource};
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tap::source::{FileTail, FileTailError};
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
async fn file_tail_observes_records_from_real_claude_session() {
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

    // Wait for the writer task to create the file before opening the
    // tail. Spawn truncates synchronously inside spawn(), so the file
    // exists by the time we get back from TapJsonlLog::spawn; this is
    // belt-and-suspenders.
    for _ in 0..40 {
        if std::fs::metadata(&tap_path).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        std::fs::metadata(&tap_path).is_ok(),
        "tap.jsonl did not appear at {}",
        tap_path.display()
    );

    // ── Spawn the tail BEFORE claude runs ──────────────────────
    let tail_path = tap_path.clone();
    let (records_tx, mut records_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let close_holder: Arc<tokio::sync::OnceCell<noodle_tap::source::CloseHandle>> =
        Arc::new(tokio::sync::OnceCell::new());
    let close_setter = Arc::clone(&close_holder);

    let tail_task = tokio::task::spawn_blocking(move || {
        let mut tail = FileTail::open(&tail_path)
            .expect("open file tail")
            .with_poll_interval(Duration::from_millis(50));
        let _ = close_setter.set(tail.close_handle());
        loop {
            match tail.next_record() {
                Ok(Some(value)) => {
                    if records_tx.send(value).is_err() {
                        break;
                    }
                }
                // EOF in tail mode shouldn't actually happen — the
                // contract says we block forever — but be tolerant.
                // Either explicit close or unexpected EOF: stop.
                Ok(None) | Err(FileTailError::Closed) => break,
                Err(e) => {
                    eprintln!("file tail error: {e}");
                    break;
                }
            }
        }
    });

    // Give the tail a moment to start polling before launching claude.
    while close_holder.get().is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    eprintln!("e2e: file tail running; launching claude");

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

    // Give the writer task a beat to flush the last records, plus the
    // tail one poll interval to pick them up.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shut down proxy + sink (forces a final flush). Then close the
    // tail (no more records can arrive).
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // Drain remaining records from the channel before we close the
    // tail — the tail may still be holding a few lines flushed by
    // the writer's shutdown drain.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut collected: Vec<Value> = Vec::new();
    while tokio::time::Instant::now() < drain_deadline {
        let timeout = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(timeout.min(Duration::from_millis(100)), records_rx.recv()).await
        {
            Ok(Some(v)) => collected.push(v),
            Ok(None) => break,
            Err(_) => {
                // No record in last 100ms; bail out of the drain.
                break;
            }
        }
    }
    if let Some(h) = close_holder.get() {
        h.close();
    }
    // Final drain after close.
    while let Ok(v) = records_rx.try_recv() {
        collected.push(v);
    }
    let _ = tail_task.await;

    // ── Compare to on-disk ────────────────────────────────────
    let on_disk = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let on_disk_lines: Vec<&str> = on_disk.lines().filter(|l| !l.trim().is_empty()).collect();
    eprintln!(
        "e2e: tail observed {} records ; on disk: {} records",
        collected.len(),
        on_disk_lines.len()
    );

    assert!(
        !on_disk_lines.is_empty(),
        "proxy wrote zero tap.jsonl records — claude didn't reach the proxy"
    );
    assert_eq!(
        collected.len(),
        on_disk_lines.len(),
        "live tail count diverges from on-disk count; tail={} disk={}",
        collected.len(),
        on_disk_lines.len()
    );

    // Spot-check: at least one anthropic request/response pair.
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
    assert!(req_count > 0, "no anthropic request records in live tail");
    assert!(resp_count > 0, "no anthropic response records in live tail");

    // Order check: tailed event_ids match on-disk event_ids in order.
    for (i, (live, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let from_disk: Value = serde_json::from_str(raw).expect("parse on-disk line");
        let live_id = live.get("event_id").and_then(Value::as_str);
        let disk_id = from_disk.get("event_id").and_then(Value::as_str);
        assert_eq!(
            live_id, disk_id,
            "record {i}: tailed event_id {live_id:?} != on-disk {disk_id:?}"
        );
    }
    eprintln!(
        "e2e: PASS — file tail observed {} records matching on-disk; anthropic req={req_count} resp={resp_count}",
        collected.len()
    );
}
