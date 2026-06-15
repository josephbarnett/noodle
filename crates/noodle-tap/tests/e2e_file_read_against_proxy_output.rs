//! End-to-end: `WireSource::FileRead` consumes records from a `tap.jsonl`
//! file the proxy wrote during a previous session.
//!
//! This is the canonical demonstrable outcome for refactor slice S13
//! (refactor-overview.md §2): a finished capture is read from start to
//! EOF; the reader yields every record the proxy emitted.
//!
//! Discipline (per global rules + AGENTS.md §"End-to-end test discipline"):
//! no fixture replay. We spawn a real `noodle-proxy` in-process, route
//! real HTTP traffic through it, point the tap sink at a real on-disk
//! tempfile, shut everything down so the file is flushed and finite,
//! then point `FileRead` at the same tempfile.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{WireSink, WireSource};
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tap::source::FileRead;
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
/// real `TapJsonlLog` writing to a real tempfile. Shut down to flush.
/// Open `FileRead` on the tempfile and assert it yields exactly 2N
/// records (request + response per round-trip), in the same order they
/// appear on disk, then signals EOF (idempotent).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn file_read_collects_all_records_proxy_emitted() {
    // Number of round-trips to drive. Two records per RT (request +
    // response), so FileRead will yield 2 * N records before EOF.
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

    // ── Shutdown so the file is finite ───────────────────────────
    //
    // Shut down proxy first (stops accepting new requests + drains in
    // flight), then drop our sink Arc by shutting it down (flushes the
    // writer task's mpsc + fsync). After this, no more bytes can land
    // in tap_path.
    proxy
        .shutdown(Duration::from_secs(2))
        .await
        .expect("proxy shutdown");
    Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap()
        .shutdown()
        .await;

    // ── Reader: batch-read the now-finished file ─────────────────
    let mut rd = FileRead::open(&tap_path).expect("open file read");
    let mut collected: Vec<Value> = Vec::with_capacity(2 * N);
    while let Some(rec) = rd.next_record().expect("next_record") {
        collected.push(rec);
    }
    // EOF must be idempotent.
    assert!(
        rd.next_record().expect("post-EOF call").is_none(),
        "FileRead returned a record after first EOF"
    );

    // ── Assertions ──────────────────────────────────────────────
    //
    // 1. Reader saw exactly 2N records (matches proxy's emitted count).
    assert_eq!(
        collected.len(),
        2 * N,
        "FileRead observed {} records; expected {}",
        collected.len(),
        2 * N
    );

    // 2. Every record has a direction and an event_id.
    for (i, rec) in collected.iter().enumerate() {
        let direction = rec.get("direction").and_then(Value::as_str);
        let event_id = rec.get("event_id").and_then(Value::as_str);
        assert!(
            direction == Some("request") || direction == Some("response"),
            "record {i} has unexpected direction: {rec}"
        );
        assert!(event_id.is_some(), "record {i} missing event_id: {rec}");
    }

    // 3. Read count matches on-disk line count exactly. The reader
    //    must not miss or duplicate any record.
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
        "FileRead count diverges from on-disk count"
    );

    // 4. event_id order matches: collected[i].event_id == on_disk[i].event_id.
    for (i, (live, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let from_disk: Value = serde_json::from_str(raw).expect("parse on-disk line");
        let live_id = live.get("event_id").and_then(Value::as_str);
        let disk_id = from_disk.get("event_id").and_then(Value::as_str);
        let live_dir = live.get("direction").and_then(Value::as_str);
        let disk_dir = from_disk.get("direction").and_then(Value::as_str);
        assert_eq!(
            live_id, disk_id,
            "record {i}: read event_id {live_id:?} != on-disk {disk_id:?}"
        );
        assert_eq!(
            live_dir, disk_dir,
            "record {i}: read direction {live_dir:?} != on-disk {disk_dir:?}"
        );
    }
}
