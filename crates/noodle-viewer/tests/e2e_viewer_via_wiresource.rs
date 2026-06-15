//! End-to-end: the viewer hub consumes a live `tap.jsonl` through
//! `WireSource::FileTail` (S15 of the 027–031 refactor; refactor-
//! overview.md §2 S15).
//!
//! Demonstrable outcome: a real `noodle-proxy` writes to a real
//! `tap.jsonl` tempfile while the viewer's
//! [`noodle_viewer::adapters::TapJsonlSource`] (now backed by
//! `noodle_tap::source::FileTail`) tails the same file and forwards
//! every record to the [`noodle_viewer::hub::HubService`]. We assert:
//!
//! 1. The hub broadcasts every record the proxy wrote.
//! 2. The hub's history contains the expected count of `Exchange`
//!    messages (2 per round-trip — request + response).
//! 3. `event_id` order is preserved across the hub.
//!
//! Discipline (AGENTS.md §"End-to-end test discipline"): no fixture
//! replay. Real proxy, real `TapJsonlLog`, real tempfile, real reader.

use std::convert::Infallible;
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
use rama::{
    http::{Body, Request, Response, StatusCode, server::HttpServer},
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
};
use tempfile::tempdir;

/// Minimal upstream that 200-OKs every request with a JSON body.
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

/// Drive N round-trips through a real proxy whose `WireSink` is a real
/// `TapJsonlLog` writing to a real tempfile. Attach a real
/// `TapJsonlSource` (S15, backed by `WireSource::FileTail`) at the
/// same tempfile to a real `HubService`. Assert the hub broadcasts
/// 2N `Exchange` messages, in the order the proxy wrote them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn hub_observes_records_via_wire_source_file_tail() {
    const N: usize = 8;

    // ── Real proxy → real TapJsonlLog → real tempfile ────────────
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

    // The writer task in `TapJsonlLog::spawn` truncates the file
    // synchronously before returning; this is the belt-and-suspenders
    // wait so the viewer adapter's `wait_for_file` succeeds.
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

    // Subscribe BEFORE attaching the source so the broadcast receiver
    // sees every Exchange the hub publishes (the hub also caches in
    // history, but the explicit receiver gives us a stable channel
    // to drain).
    let (_history, mut hub_rx) = hub.subscribe().await;
    // Keep the `tap_source` alive — its `close()` is called below at
    // shutdown to stop the blocking worker thread cleanly. Dropping
    // it early would leave the worker polling forever and prevent
    // the test's tokio runtime from exiting.
    hub.attach_source(&tap_source);

    // ── Drive N requests through the proxy ───────────────────────
    for i in 0..N {
        let r = client
            .get(format!("http://{upstream}/echo?n={i}"))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), 200);
        let _ = r.text().await.expect("body");
    }

    // ── Collect 2N Exchange messages from the hub ────────────────
    //
    // Each round-trip produces one request record + one response
    // record. The proxy flushes asynchronously; the tail polls
    // every 50ms; allow generous time for convergence.
    let mut collected: Vec<noodle_viewer::model::Exchange> = Vec::with_capacity(2 * N);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while collected.len() < 2 * N {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        match tokio::time::timeout(timeout, hub_rx.recv()).await {
            Ok(Ok(msg)) => {
                if let ServerMsg::Exchange(ex) = &*msg {
                    collected.push(ex.clone());
                }
                // Hello, Capture, Frame, SideEffect — not relevant.
            }
            // Channel closed or deadline fired — stop collecting; the
            // count assertion below reports any gap.
            Ok(Err(_)) | Err(_) => break,
        }
    }

    // ── Shutdown ─────────────────────────────────────────────────
    proxy
        .shutdown(Duration::from_secs(2))
        .await
        .expect("proxy shutdown");
    Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders after proxy shutdown")
        .unwrap()
        .shutdown()
        .await;

    // Stop the tail worker so the blocking thread exits and the
    // tokio runtime can shut down cleanly at the end of the test.
    tap_source.close();
    drop(tap_source);

    // ── Assertions ───────────────────────────────────────────────
    assert_eq!(
        collected.len(),
        2 * N,
        "hub observed {} Exchange messages; expected {} (2 per round-trip)",
        collected.len(),
        2 * N
    );

    // Compare to what landed on disk. The hub must not miss or
    // duplicate records that the tail observed.
    let on_disk = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let on_disk_lines: Vec<&str> = on_disk.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        on_disk_lines.len(),
        2 * N,
        "on-disk record count {} != expected {}",
        on_disk_lines.len(),
        2 * N
    );

    // event_id order match: collected[i].event_id == on_disk[i].event_id
    for (i, (live, raw)) in collected.iter().zip(on_disk_lines.iter()).enumerate() {
        let disk: serde_json::Value = serde_json::from_str(raw).expect("parse on-disk line");
        let disk_id = disk
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let disk_dir = disk
            .get("direction")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert_eq!(
            live.event_id,
            disk_id,
            "record {i}: hub event_id {live_id:?} != on-disk {disk_id:?}",
            live_id = live.event_id,
        );
        let live_dir = match live.direction {
            noodle_viewer::model::Direction::Request => "request",
            noodle_viewer::model::Direction::Response => "response",
        };
        assert_eq!(
            live_dir, disk_dir,
            "record {i}: hub direction {live_dir:?} != on-disk {disk_dir:?}",
        );
    }

    // Sanity: every other record alternates request/response (the
    // driver issues requests serially, so the proxy writes them in
    // strict order).
    for (i, ex) in collected.iter().enumerate() {
        let expected = if i % 2 == 0 {
            noodle_viewer::model::Direction::Request
        } else {
            noodle_viewer::model::Direction::Response
        };
        assert_eq!(
            ex.direction,
            expected,
            "record {i}: expected {expected:?}, got {got:?}",
            got = ex.direction,
        );
    }
}
