//! End-to-end: the viewer's `GET /api/decoded-exchanges` SSE
//! endpoint streams typed [`DecodedExchange`] records to clients
//! (S22 of the 027–031 refactor — refactor-overview.md §10).
//!
//! Demonstrable outcome: a real `noodle-proxy` writes records to
//! `tap.jsonl`; the viewer's HTTP server runs `decoded_sse`
//! against the hub; an HTTP client connects to
//! `/api/decoded-exchanges` and reads SSE frames whose `data:`
//! payloads parse back into JSON with the typed S22 wire shape
//! (`marks.turn_id`, `envelope.collector_app.name`, `usage.tokens.
//! input_tokens`, etc.).
//!
//! Discipline (AGENTS.md §"End-to-end test discipline"): no
//! fixture replay. Real proxy, real `TapJsonlLog`, real tempfile,
//! real viewer HTTP server, real HTTP client.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::WireSink;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use noodle_viewer::adapters::{DecodedTapJsonlSource, HttpDebugProxy};
use noodle_viewer::hub::HubService;
use noodle_viewer::server;
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

/// Read SSE frames from a `data:` byte stream and return the
/// concatenated `data:` payload of the first N events that match
/// `event_name`. Frames are separated by `\n\n`; lines inside an
/// event are `field: value`.
async fn collect_n_sse_data_payloads(
    mut response: reqwest::Response,
    event_name: &str,
    n: usize,
    deadline: tokio::time::Instant,
) -> Vec<String> {
    let mut collected: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut current_event: Option<String> = None;
    let mut current_data: Vec<String> = Vec::new();

    while collected.len() < n {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        let chunk = tokio::time::timeout(timeout, response.chunk()).await;
        let Ok(Ok(Some(chunk))) = chunk else { break };
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // Process any complete lines.
        while let Some(idx) = buf.find('\n') {
            let line = buf[..idx].trim_end_matches('\r').to_owned();
            buf.drain(..=idx);

            if line.is_empty() {
                // End-of-event boundary.
                if current_event.as_deref() == Some(event_name) && !current_data.is_empty() {
                    collected.push(current_data.join("\n"));
                    if collected.len() >= n {
                        break;
                    }
                }
                current_event = None;
                current_data.clear();
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                current_event = Some(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("data:") {
                current_data.push(rest.trim().to_owned());
            }
            // Ignore `id:`, `retry:`, `:` comments.
        }
    }
    collected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn decoded_exchanges_sse_endpoint_streams_typed_records_with_wire_shape() {
    const N_ROUNDTRIPS: usize = 3;
    const EXPECTED_RECORDS: usize = 2 * N_ROUNDTRIPS; // req + resp each

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

    // Wait briefly for the writer task to truncate the file.
    for _ in 0..40 {
        if std::fs::metadata(&tap_path).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(std::fs::metadata(&tap_path).is_ok());

    // ── Viewer hub + decoded source + HTTP server ────────────────
    let hub = HubService::new();
    let decoded_source = DecodedTapJsonlSource::spawn(tap_path.clone(), 1024)
        .await
        .expect("spawn decoded tap source");
    hub.attach_decoded_source(&decoded_source);

    // Bind the viewer HTTP server on an ephemeral port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind viewer");
    let viewer_addr = listener.local_addr().expect("viewer local_addr");
    let server_proxy = HttpDebugProxy::new("http://127.0.0.1:65535".to_owned());
    // Use server::serve to get the same router the binary uses.
    // It takes a listener-bound address, so we re-use the same
    // construction by passing the address (binds inside).
    drop(listener); // free the port; server::serve will rebind
    // V2 OTLP query tab — pass a `RollupsState` pointing at a path
    // the embellisher never writes; this test only exercises the
    // SSE endpoint, so the rollups state is intentionally inert.
    let rollups = noodle_viewer::server::rollups::RollupsState::new(std::path::PathBuf::from(
        "/tmp/noodle-viewer-e2e-test-rollups-does-not-exist.db",
    ));
    let server_handle = server::serve(viewer_addr, hub.clone(), server_proxy, rollups)
        .await
        .expect("spawn viewer http server");

    // ── Open the SSE endpoint BEFORE driving traffic ────────────
    // Otherwise: the hub broadcast is not history-retained on the
    // decoded channel; we must subscribe first, then drive traffic,
    // then receive.
    let sse_url = format!("http://{viewer_addr}/api/decoded-exchanges");
    let sse_client = reqwest::Client::builder()
        .no_proxy() // direct connect to the loopback viewer
        .build()
        .expect("sse client");
    let sse_response = sse_client
        .get(&sse_url)
        .header("accept", "text/event-stream")
        .send()
        .await
        .expect("open SSE");
    assert!(
        sse_response.status().is_success(),
        "SSE endpoint returned {}",
        sse_response.status()
    );
    assert_eq!(
        sse_response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream",
        "SSE content-type"
    );

    // ── Drive N requests through the proxy ──────────────────────
    for i in 0..N_ROUNDTRIPS {
        let r = client
            .get(format!("http://{upstream}/echo?n={i}"))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), 200);
        let _ = r.text().await.expect("body");
    }

    // ── Read SSE frames ─────────────────────────────────────────
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let payloads =
        collect_n_sse_data_payloads(sse_response, "decoded_exchange", EXPECTED_RECORDS, deadline)
            .await;

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
    decoded_source.close();
    drop(decoded_source);
    server_handle.abort();

    // ── Assertions ───────────────────────────────────────────────
    assert_eq!(
        payloads.len(),
        EXPECTED_RECORDS,
        "SSE endpoint streamed {} decoded events; expected {}",
        payloads.len(),
        EXPECTED_RECORDS
    );

    // Every payload must parse back into a JSON object with the
    // S22 wire shape — `exchange.event_id` populated, snake_case
    // keys.
    let parsed: Vec<serde_json::Value> = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).expect("payload parses as JSON"))
        .collect();

    for (i, v) in parsed.iter().enumerate() {
        assert!(
            v["exchange"]["event_id"].is_string(),
            "record {i}: missing exchange.event_id — wire shape regressed?"
        );
        assert!(
            v["exchange"]["provider"].is_string(),
            "record {i}: missing exchange.provider"
        );
    }

    // Spot-check the envelope: at least one record carries
    // envelope.collector_app.name == "noodle" (compile-time embedded
    // by the proxy). Pins that the typed extractor's projection
    // reached the wire as the on-disk shape rather than the
    // internal struct shape.
    let with_collector = parsed.iter().find(|v| {
        v["envelope"]["collector_app"]["name"]
            .as_str()
            .is_some_and(|n| n == "noodle")
    });
    assert!(
        with_collector.is_some(),
        "no SSE frame carried envelope.collector_app.name == \"noodle\" — either the proxy stopped \
         stamping it or the typed extractor/wire shape regressed"
    );

    eprintln!(
        "e2e: PASS — SSE endpoint streamed {} decoded events; envelope.collector_app present",
        parsed.len()
    );
}
