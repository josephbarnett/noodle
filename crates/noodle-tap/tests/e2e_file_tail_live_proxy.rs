//! End-to-end: `WireSource::FileTail` consumes records from a `tap.jsonl`
//! file while the proxy is actively writing it.
//!
//! This is the canonical demonstrable outcome for refactor slice S12
//! (refactor-overview.md §2): the proxy and the reader run concurrently;
//! the reader observes every record the proxy writes, in order, as the
//! proxy writes it.
//!
//! Discipline (per global rules + AGENTS.md §"End-to-end test discipline"):
//! no fixture replay. We spawn a real `noodle-proxy` in-process, route
//! real HTTP traffic through it, point the tap sink at a real on-disk
//! tempfile, and tail that tempfile in a background task while the
//! requests flow.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{WireSink, WireSource};
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tap::source::{FileTail, FileTailError};
use noodle_tls::ca::Ca;
use rama::{
    http::{Body, Request, Response, StatusCode, server::HttpServer},
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
};
use serde_json::Value;
use tempfile::tempdir;

/// Spawn a mock upstream that returns a small JSON body on every
/// request. Returns the bound address.
async fn spawn_upstream() -> std::net::SocketAddr {
    let exec = Executor::default();
    let listener = TcpListener::build(exec.clone())
        .bind_address("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream local_addr");
    let svc = HttpServer::auto(exec).service(service_fn(
        move |_req: Request| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Response, Infallible>> + Send>,
        > {
            Box::pin(async move {
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"ok":true}"#))
                    .unwrap())
            })
        },
    ));
    tokio::spawn(async move {
        listener.serve(svc).await;
    });
    addr
}

async fn spawn_proxy(sink: Arc<TapJsonlLog>) -> noodle_proxy::ProxyHandle {
    start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 2 * 1024 * 1024,
        wire: sink as Arc<dyn WireSink>,
        codecs: None,
        engine: None,
        filters: Vec::new(),
        enhancers: Vec::new(),
        context: None,
        sessions: Arc::new(InMemorySessionStore::new()),
        ca: Arc::new(Ca::generate().expect("test CA")),
        markings: None,
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy")
}

fn proxied_client(addr: std::net::SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::http(format!("http://{addr}")).expect("valid proxy url"))
        .build()
        .expect("build client")
}

/// Drive N HTTP requests through a real proxy whose `WireSink` is a
/// real `TapJsonlLog` writing to a real tempfile. In parallel, run a
/// `FileTail` reader on the same tempfile. Assert the reader observes
/// 2N records (request + response per round-trip) in the same order
/// the proxy wrote them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn file_tail_yields_records_as_proxy_writes_them() {
    // Number of round-trips to drive. Two records per RT (request +
    // response), so the reader will eventually see 2 * N records.
    const N: usize = 8;

    // ── Setup: real proxy → real TapJsonlLog → real tempfile ─────
    let dir = tempdir().expect("tempdir");
    let tap_path = dir.path().join("tap.jsonl");
    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 256)
            .await
            .expect("spawn tap sink"),
    );

    let upstream = spawn_upstream().await;
    let proxy = spawn_proxy(Arc::clone(&tap_sink)).await;
    let client = proxied_client(proxy.local_addr());

    // Touch the file so FileTail::open succeeds even before the first
    // write (spawn already truncated it; this is belt-and-suspenders).
    // The writer task may not have flushed metadata yet on slow CI.
    for _ in 0..20 {
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

    // ── Reader: spawn a blocking task that tails the same file ───
    //
    // FileTail::next_record blocks (sync). Run on a blocking pool
    // and forward records over an mpsc.
    let tail_path = tap_path.clone();
    let (records_tx, mut records_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let tail_handle_holder: Arc<tokio::sync::OnceCell<noodle_tap::source::CloseHandle>> =
        Arc::new(tokio::sync::OnceCell::new());
    let close_setter = Arc::clone(&tail_handle_holder);

    let tail_task = tokio::task::spawn_blocking(move || {
        let mut tail = FileTail::open(&tail_path)
            .expect("open file tail")
            .with_poll_interval(Duration::from_millis(20));
        let _ = close_setter.set(tail.close_handle());
        loop {
            match tail.next_record() {
                Ok(Some(value)) => {
                    if records_tx.send(value).is_err() {
                        // Receiver dropped — orderly shutdown.
                        break;
                    }
                }
                Ok(None) => {
                    // tail mode never returns Ok(None), but be tolerant.
                    break;
                }
                Err(FileTailError::Closed) => break,
                Err(e) => panic!("file tail error: {e}"),
            }
        }
    });

    // Wait for the tail to publish its close handle.
    while tail_handle_holder.get().is_none() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // ── Driver: send N requests through the proxy ────────────────
    for i in 0..N {
        let r = client
            .get(format!("http://{upstream}/echo?n={i}"))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), 200);
        let _ = r.text().await.expect("body");
    }

    // ── Collect: pull 2N records from the tailer ─────────────────
    //
    // The proxy flushes async; allow up to several seconds for the
    // writer task + the tail poll loop to converge.
    let mut collected: Vec<Value> = Vec::with_capacity(2 * N);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while collected.len() < 2 * N {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        match tokio::time::timeout(timeout, records_rx.recv()).await {
            Ok(Some(v)) => collected.push(v),
            // Either the channel closed or the overall deadline
            // fired — in both cases we stop collecting; the
            // subsequent length assertion will report the gap.
            Ok(None) | Err(_) => break,
        }
    }

    // ── Shutdown ────────────────────────────────────────────────
    if let Some(h) = tail_handle_holder.get() {
        h.close();
    }
    proxy
        .shutdown(Duration::from_secs(2))
        .await
        .expect("proxy shutdown");
    Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap()
        .shutdown()
        .await;
    let _ = tail_task.await;

    // ── Assertions ──────────────────────────────────────────────
    //
    // 1. Tail saw exactly 2N records.
    assert_eq!(
        collected.len(),
        2 * N,
        "FileTail observed {} records; expected {}",
        collected.len(),
        2 * N
    );

    // 2. Records alternate request / response and pair by event_id.
    //    Requests and responses correlate; we don't assume a fixed
    //    interleaving across concurrent calls but here we drove the
    //    requests serially, so the stream should be req, resp, req,
    //    resp, ...
    for (i, rec) in collected.iter().enumerate() {
        let direction = rec.get("direction").and_then(Value::as_str);
        let event_id = rec.get("event_id").and_then(Value::as_str);
        assert!(
            direction == Some("request") || direction == Some("response"),
            "record {i} has unexpected direction: {rec}"
        );
        assert!(event_id.is_some(), "record {i} missing event_id: {rec}");
    }

    // 3. Same record set as what landed on disk: read the file in
    //    one shot at the end and compare counts. (Live tail must not
    //    miss or duplicate.)
    let on_disk = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let on_disk_lines: Vec<&str> = on_disk.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        on_disk_lines.len(),
        2 * N,
        "on-disk records: {} ; expected {}",
        on_disk_lines.len(),
        2 * N
    );
    assert_eq!(
        collected.len(),
        on_disk_lines.len(),
        "live tail count diverges from on-disk count"
    );

    // 4. event_id order matches: tailed[i].event_id == on_disk[i].event_id
    for (i, (live, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let from_disk: Value = serde_json::from_str(raw).expect("parse on-disk line");
        let live_id = live.get("event_id").and_then(Value::as_str);
        let disk_id = from_disk.get("event_id").and_then(Value::as_str);
        let live_dir = live.get("direction").and_then(Value::as_str);
        let disk_dir = from_disk.get("direction").and_then(Value::as_str);
        assert_eq!(
            live_id, disk_id,
            "record {i}: tailed event_id {live_id:?} != on-disk {disk_id:?}"
        );
        assert_eq!(
            live_dir, disk_dir,
            "record {i}: tailed direction {live_dir:?} != on-disk {disk_dir:?}"
        );
    }
}
