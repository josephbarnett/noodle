//! End-to-end: viewer hub consumes a live `tap.jsonl` through the
//! new typed [`DecodedTapJsonlSource`] (S21 of the 027–031
//! refactor — refactor-overview.md §10).
//!
//! Demonstrable outcome: a real `noodle-proxy` writes to a real
//! `tap.jsonl` tempfile while the viewer's
//! [`DecodedTapJsonlSource`] tails the same file, runs every
//! record through the [`ProviderDecoderRegistry`], and forwards
//! the resulting [`DecodedExchange`]s to the
//! [`HubService::attach_decoded_source`] fanout. We assert:
//!
//! 1. The hub broadcasts `2 * N` decoded events for `N`
//!    round-trips.
//! 2. Every decoded event has its typed `Exchange` populated
//!    (same shape the slim S15 path produced).
//! 3. At least one request record carries an `envelope` block
//!    whose `collector_app.name == "noodle"` — proving the typed
//!    extractor reached into the envelope wire shape.
//!
//! Discipline (AGENTS.md §"End-to-end test discipline"): no
//! fixture replay. Real proxy, real `TapJsonlLog`, real tempfile,
//! real reader.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::WireSink;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use noodle_viewer::adapters::DecodedTapJsonlSource;
use noodle_viewer::hub::HubService;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn hub_observes_decoded_exchanges_via_provider_decoder_registry() {
    const N: usize = 4;

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

    // ── Real viewer hub + decoded source ─────────────────────────
    let hub = HubService::new();
    let decoded_source = DecodedTapJsonlSource::spawn(tap_path.clone(), 1024)
        .await
        .expect("spawn decoded tap source");
    let (_history, mut decoded_rx) = hub.subscribe_decoded().await;
    hub.attach_decoded_source(&decoded_source);

    // ── Drive N requests through the proxy ──────────────────────
    for i in 0..N {
        let r = client
            .get(format!("http://{upstream}/echo?n={i}"))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status(), 200);
        let _ = r.text().await.expect("body");
    }

    // ── Collect 2N DecodedExchange messages ─────────────────────
    let mut collected: Vec<noodle_viewer::model::DecodedExchange> = Vec::with_capacity(2 * N);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while collected.len() < 2 * N {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        match tokio::time::timeout(timeout, decoded_rx.recv()).await {
            Ok(Ok(dx)) => collected.push((*dx).clone()),
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
    decoded_source.close();
    drop(decoded_source);

    // ── Assertions ───────────────────────────────────────────────
    assert_eq!(
        collected.len(),
        2 * N,
        "hub observed {} DecodedExchange messages; expected {}",
        collected.len(),
        2 * N
    );

    // Every decoded record must have its slim Exchange populated
    // (event_id, direction, provider). This proves the registry's
    // serde_from_value path ran successfully through every record.
    for (i, dx) in collected.iter().enumerate() {
        assert!(
            !dx.exchange.event_id.is_empty(),
            "decoded record {i} missing event_id"
        );
        assert!(
            !dx.exchange.provider.is_empty(),
            "decoded record {i} missing provider"
        );
    }

    // Spot-check: at least one record carries a typed envelope
    // whose collector_app.name == "noodle". The proxy stamps
    // collector_app from compile-time build info on every record;
    // if this fails, the envelope shape changed or the typed
    // extractor regressed.
    let with_collector = collected
        .iter()
        .filter_map(|dx| dx.envelope.as_ref())
        .filter_map(|env| env.collector_app.as_ref())
        .find(|c| c.name == "noodle");
    assert!(
        with_collector.is_some(),
        "no DecodedExchange carried envelope.collector_app.name == \"noodle\" — \
         either the proxy stopped stamping it or the typed extractor regressed"
    );

    eprintln!(
        "e2e: PASS — hub observed {} DecodedExchanges; at least one envelope.collector_app populated",
        collected.len()
    );
}
