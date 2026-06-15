//! Exec-claude e2e for the viewer's `WireSource::FileTail`-backed
//! tap.jsonl ingestion (S15 of the 027–031 refactor; refactor-
//! overview.md §2 S15).
//!
//! Spawns the real `claude` CLI through a real noodle proxy whose
//! `WireSink` is a real `TapJsonlLog`. The viewer's `HubService`
//! tails the same `tap.jsonl` through the new `TapJsonlSource`
//! (backed by `noodle_tap::source::FileTail`). After claude exits we
//! assert that the hub observed every record the proxy wrote.
//!
//! Per the noodle e2e contract (AGENTS.md §"End-to-end test
//! discipline"; `~/.claude/CLAUDE.md` memory rule
//! `feedback_no_fixture_extraction`), fixture replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-viewer --test e2e_viewer_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::WireSink;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use noodle_viewer::adapters::TapJsonlSource;
use noodle_viewer::hub::HubService;
use noodle_viewer::model::ServerMsg;
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
async fn viewer_hub_observes_records_from_real_claude_session() {
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

    // The TapJsonlLog writer truncates the file synchronously inside
    // spawn(), but be belt-and-suspenders here.
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

    // ── Real viewer hub + TapJsonlSource via FileTail ────────────
    let hub = HubService::new();
    let tap_source = TapJsonlSource::spawn(tap_path.clone(), 1024)
        .await
        .expect("spawn tap source");
    let (_history, mut hub_rx) = hub.subscribe().await;
    // Keep `tap_source` alive so its `close()` can be called at
    // shutdown — dropping early would leave the blocking worker
    // polling forever and prevent the test runtime from exiting.
    hub.attach_source(&tap_source);

    // ── Spawn a task that drains hub_rx into a Vec ───────────────
    let (collect_tx, mut collect_rx) =
        tokio::sync::mpsc::unbounded_channel::<noodle_viewer::model::Exchange>();
    let drain = tokio::spawn(async move {
        while let Ok(msg) = hub_rx.recv().await {
            if let ServerMsg::Exchange(ex) = &*msg
                && collect_tx.send(ex.clone()).is_err()
            {
                break;
            }
        }
    });

    eprintln!("e2e: viewer hub running; launching claude");

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

    // Give the writer task a moment to flush, plus the tail one poll
    // interval to pick up the last records, plus the hub's broadcast
    // round-trip.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shut down proxy → writer drain → tail eventually returns.
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // Stop the tail worker so the blocking thread exits and the
    // tokio runtime can shut down cleanly.
    tap_source.close();
    drop(tap_source);

    // Drain any in-flight records from the collector channel before
    // we stop the drain task (last 1s of polling).
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut collected: Vec<noodle_viewer::model::Exchange> = Vec::new();
    while tokio::time::Instant::now() < drain_deadline {
        let timeout = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(timeout.min(Duration::from_millis(100)), collect_rx.recv()).await
        {
            Ok(Some(ex)) => collected.push(ex),
            // Channel closed (Ok(None)) or no record in last 100ms (Err(_)):
            // bail out of the drain loop.
            Ok(None) | Err(_) => break,
        }
    }
    // Final non-blocking drain.
    while let Ok(ex) = collect_rx.try_recv() {
        collected.push(ex);
    }
    drain.abort();
    let _ = drain.await;

    // ── Compare hub-observed vs on-disk ──────────────────────────
    let on_disk = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let on_disk_lines: Vec<&str> = on_disk.lines().filter(|l| !l.trim().is_empty()).collect();
    eprintln!(
        "e2e: hub observed {} Exchange msgs ; on disk: {} records",
        collected.len(),
        on_disk_lines.len()
    );

    assert!(
        !on_disk_lines.is_empty(),
        "proxy wrote zero tap.jsonl records — claude didn't reach the proxy"
    );

    // Per the slice's definition of done: the hub receives every
    // record the proxy wrote, no more, no less.
    assert_eq!(
        collected.len(),
        on_disk_lines.len(),
        "hub Exchange count diverges from on-disk record count; hub={} disk={}",
        collected.len(),
        on_disk_lines.len()
    );

    // Spot-check: at least one anthropic request + response.
    let req_count = collected
        .iter()
        .filter(|r| {
            matches!(r.direction, noodle_viewer::model::Direction::Request)
                && r.provider == "anthropic"
        })
        .count();
    let resp_count = collected
        .iter()
        .filter(|r| {
            matches!(r.direction, noodle_viewer::model::Direction::Response)
                && r.provider == "anthropic"
        })
        .count();
    eprintln!("e2e: anthropic req={req_count} resp={resp_count}");
    assert!(
        req_count > 0,
        "no anthropic request Exchange in hub-observed records"
    );
    assert!(
        resp_count > 0,
        "no anthropic response Exchange in hub-observed records"
    );

    // Order check: hub event_ids match on-disk event_ids in order.
    for (i, (hub_ex, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let from_disk: serde_json::Value = serde_json::from_str(raw).expect("parse on-disk line");
        let disk_id = from_disk
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert_eq!(
            hub_ex.event_id, disk_id,
            "record {i}: hub event_id {:?} != on-disk {:?}",
            hub_ex.event_id, disk_id
        );
    }

    eprintln!(
        "e2e: PASS — viewer hub via FileTail observed {} records matching on-disk; \
         anthropic req={req_count} resp={resp_count}",
        collected.len()
    );
}
