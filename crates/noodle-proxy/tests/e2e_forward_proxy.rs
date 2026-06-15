//! End-to-end tests for the forward proxy + wire log.
//!
//! Each test:
//!   1. Spins up a mock upstream HTTP server on an ephemeral port.
//!   2. Spins up noodle-proxy in-process on an ephemeral port with a
//!      `Vec<WireEvent>`-capturing `WireSink`.
//!   3. Sends a real request through the proxy to the upstream.
//!   4. Asserts response correctness AND wire log capture.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{WireDirection, WireEvent, WireSink};
use noodle_proxy::{ProxyConfig, start};
use rama::{
    http::{Body, Request, Response, StatusCode, server::HttpServer},
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
};

/// In-memory `WireSink` for assertions.
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

/// Spawns a mock upstream that runs `handler` for every incoming
/// request. Returns the bound address.
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

async fn spawn_proxy(sink: Arc<CapturingSink>) -> noodle_proxy::ProxyHandle {
    start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 2 * 1024 * 1024,
        wire: sink,
        codecs: None,
        engine: None,
        filters: Vec::new(),
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

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plain_get_forwards_and_wire_log_captures_both_directions() {
    let upstream = spawn_upstream(|_req| async move {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(Body::from("hello from upstream"))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone()).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{upstream}/echo"))
        .header("x-marker", "alpha")
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello from upstream");

    let events = sink.snapshot();
    assert_eq!(events.len(), 2, "expected one request and one response");

    let req_ev = &events[0];
    let resp_ev = &events[1];
    assert_eq!(req_ev.direction, WireDirection::Request);
    assert_eq!(resp_ev.direction, WireDirection::Response);
    assert_eq!(req_ev.request_id, resp_ev.request_id, "ids correlate");
    assert_eq!(req_ev.method.as_deref(), Some("GET"));
    assert!(req_ev.url.as_deref().unwrap_or("").contains("/echo"));
    assert!(
        req_ev
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("x-marker") && h.value == "alpha"),
        "x-marker header captured verbatim"
    );
    assert_eq!(resp_ev.status, Some(200));
    assert_eq!(
        std::str::from_utf8(&resp_ev.body_in).ok(),
        Some("hello from upstream")
    );

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn post_body_round_trips_and_is_captured_byte_faithful() {
    let upstream = spawn_upstream(|req| async move {
        let bytes = rama::http::body::util::BodyExt::collect(req.into_body())
            .await
            .unwrap()
            .to_bytes();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(bytes))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone()).await;

    let payload = r#"{"hello":"world","n":42}"#;
    let resp = proxied_client(proxy.local_addr())
        .post(format!("http://{upstream}/echo"))
        .header("content-type", "application/json")
        .body(payload.to_string())
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), payload);

    let events = sink.snapshot();
    assert_eq!(events.len(), 2);
    assert_eq!(std::str::from_utf8(&events[0].body_in).ok(), Some(payload));
    assert_eq!(events[0].body_in.len(), payload.len());
    assert_eq!(std::str::from_utf8(&events[1].body_in).ok(), Some(payload));
    assert_eq!(events[1].body_in.len(), payload.len());

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn upstream_unreachable_yields_502() {
    // Bind a port, then drop the listener so the address is dead.
    let dead_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone()).await;

    let resp = proxied_client(proxy.local_addr())
        .get(format!("http://{dead_addr}/nope"))
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), 502);

    let events = sink.snapshot();
    // We always log the request; the response is the 502 we synthesized.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].direction, WireDirection::Request);
    assert_eq!(events[1].direction, WireDirection::Response);
    assert_eq!(events[1].status, Some(502));

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn concurrent_requests_get_unique_correlated_ids() {
    let upstream = spawn_upstream(|_req| async move {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Body::from("ok"))
            .unwrap())
    })
    .await;

    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone()).await;
    let client = proxied_client(proxy.local_addr());

    let mut handles = vec![];
    for i in 0..10 {
        let c = client.clone();
        let url = format!("http://{upstream}/req?n={i}");
        handles.push(tokio::spawn(async move {
            c.get(url).send().await.unwrap().status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), 200);
    }

    let events = sink.snapshot();
    assert_eq!(events.len(), 20, "10 requests × 2 events each");

    // Group by request_id; each id should have exactly one Request and one Response.
    let mut by_id: HashMap<String, (usize, usize)> = HashMap::new();
    for ev in &events {
        let entry = by_id.entry(ev.request_id.to_string()).or_default();
        match ev.direction {
            WireDirection::Request => entry.0 += 1,
            WireDirection::Response => entry.1 += 1,
        }
    }
    assert_eq!(by_id.len(), 10, "ten distinct request_ids");
    for (id, (reqs, resps)) in &by_id {
        assert_eq!(*reqs, 1, "id {id} has exactly one request");
        assert_eq!(*resps, 1, "id {id} has exactly one response");
    }

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}
