#![allow(deprecated)]
// A.8.a: integration test exercises the legacy ProviderCodec path. Migration to layered tracked under A.8.b.

//! End-to-end test: a `<noodle:NAME>VALUE</noodle:NAME>` marker placed
//! in a real upstream response is stripped before the client sees it,
//! AND the wire log records the (already-stripped) bytes the client
//! actually received.
//!
//! Setup mirrors `e2e_forward_proxy.rs` — mock upstream + spawn proxy +
//! reqwest client. The new wrinkle is `ProxyConfig::filters` carrying a
//! `MarkerStripFilterFactory`.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::filter::MarkerStripFilterFactory;
use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{FilterFactory, WireDirection, WireEvent, WireSink};
use noodle_proxy::{ProxyConfig, start};
use rama::{
    http::{Body, Request, Response, StatusCode, server::HttpServer},
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
};

#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<WireEvent>>,
}
impl CapturingSink {
    fn snapshot(&self) -> Vec<WireEvent> {
        self.events.lock().unwrap().clone()
    }
}
impl WireSink for CapturingSink {
    fn record(&self, event: WireEvent) {
        self.events.lock().unwrap().push(event);
    }
}

async fn spawn_upstream<F, Fut>(handler: F) -> std::net::SocketAddr
where
    F: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Response, Infallible>> + Send + 'static,
{
    let exec = Executor::default();
    let listener = TcpListener::build(exec.clone())
        .bind_address("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream local_addr");
    let svc = HttpServer::auto(exec).service(service_fn(move |req| {
        let h = handler.clone();
        async move { h(req).await }
    }));
    tokio::spawn(async move {
        listener.serve(svc).await;
    });
    addr
}

async fn spawn_proxy_with_marker_strip(
    sink: Arc<CapturingSink>,
    tag_names: Vec<&'static str>,
) -> noodle_proxy::ProxyHandle {
    let factory: Arc<dyn FilterFactory> = Arc::new(MarkerStripFilterFactory::new(tag_names));
    start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 2 * 1024 * 1024,
        wire: sink,
        codecs: None,
        engine: None,
        filters: vec![factory],
        enhancers: Vec::new(),
        context: None,
        sessions: Arc::new(InMemorySessionStore::new()),
        ca: Arc::new(noodle_tls::ca::Ca::generate().expect("test CA")),
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

// ── Tests ──────────────────────────────────────────────────────────

#[tokio::test]
async fn marker_in_text_response_is_stripped_before_client() {
    let upstream = spawn_upstream(|_req| async move {
        // Realistic shape: prose, marker, more prose.
        let body = "I built the auth flow. <noodle:work_type>build</noodle:work_type>\nThanks.";
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; charset=utf-8")
            .body(Body::from(body))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_marker_strip(sink.clone(), vec!["work_type"]).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{upstream}/turn"))
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("body");

    // The client must NOT see the marker. The trailing newline that
    // immediately followed the close is also eaten by the FSM.
    assert_eq!(body, "I built the auth flow. Thanks.");
    assert!(!body.contains("<noodle:"));
    assert!(!body.contains("</noodle:"));

    // The wire log records BOTH views:
    //   body_in  = upstream's original (with the marker)
    //   body_out = what the client saw (stripped)
    // The diff is the audit trail of what the filter removed.
    let events = sink.snapshot();
    assert_eq!(events.len(), 2);
    let resp_ev = events
        .iter()
        .find(|e| e.direction == WireDirection::Response)
        .expect("response event");
    assert_eq!(resp_ev.status, Some(200));
    assert_eq!(
        std::str::from_utf8(&resp_ev.body_in).ok(),
        Some("I built the auth flow. <noodle:work_type>build</noodle:work_type>\nThanks."),
        "body_in must be the upstream's original (with the marker)",
    );
    assert_eq!(
        std::str::from_utf8(&resp_ev.body_out).ok(),
        Some("I built the auth flow. Thanks."),
        "body_out must be the post-filter bytes the client received (no marker)",
    );
    assert_ne!(
        resp_ev.body_in, resp_ev.body_out,
        "mutation must be observable in the wire log",
    );

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn marker_split_across_chunks_in_pipeline_still_stripped() {
    // The proxy buffers the full body before applying filters today,
    // so this test exercises the within-buffer FSM correctness rather
    // than cross-chunk streaming. (Streaming through a per-event
    // pipeline lands when ProviderCodec is wired into the response
    // path.) The upstream returns one big body containing a marker.
    let upstream = spawn_upstream(|_req| async move {
        let body = format!(
            "{}<noodle:work_type>{}</noodle:work_type>{}",
            "x".repeat(2048),
            "research",
            "y".repeat(2048),
        );
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(Body::from(body))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_marker_strip(sink.clone(), vec!["work_type"]).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{upstream}/big"))
        .send()
        .await
        .expect("send");

    let body = resp.text().await.expect("body");
    assert_eq!(body.len(), 4096);
    assert!(!body.contains("<noodle:"));
    assert!(body.starts_with(&"x".repeat(2048)));
    assert!(body.ends_with(&"y".repeat(2048)));

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn binary_body_passes_through_unmodified() {
    // Filters only apply to text bodies. A response with a binary
    // content-type that happens to contain marker-shaped bytes should
    // not be filtered.
    let payload: Vec<u8> = b"prefix<noodle:work_type>foo</noodle:work_type>suffix".to_vec();
    let payload_for_upstream = payload.clone();
    let upstream = spawn_upstream(move |_req| {
        let payload = payload_for_upstream.clone();
        async move {
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/octet-stream")
                .body(Body::from(payload))
                .unwrap())
        }
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_marker_strip(sink.clone(), vec!["work_type"]).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{upstream}/bin"))
        .send()
        .await
        .expect("send");

    let body = resp.bytes().await.expect("body");
    assert_eq!(body.as_ref(), payload.as_slice());

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn no_marker_in_response_passes_through_unchanged() {
    let upstream = spawn_upstream(|_req| async move {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(Body::from("just plain text"))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_marker_strip(sink.clone(), vec!["work_type"]).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{upstream}/plain"))
        .send()
        .await
        .expect("send");

    assert_eq!(resp.text().await.unwrap(), "just plain text");

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}
