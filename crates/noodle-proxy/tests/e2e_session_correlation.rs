//! End-to-end: per-flow `Session` lookup is wired through the proxy.
//! Two requests carrying the same `x-noodle-session` header resolve
//! to the same `Arc<Session>` in the store; two requests with
//! different headers resolve to different sessions.
//!
//! Sessions don't yet drive any visible behavior in the proxy
//! (Enhancers aren't wired into the request path). The point of this
//! test is to pin the correlation contract: when a future `ContextEnhancer`
//! checks `Session::directive_enhanced`, the right `Session` is in
//! scope.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{Session, SessionStore, WireEvent, WireSink};
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
impl WireSink for CapturingSink {
    fn record(&self, event: WireEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// `SessionStore` wrapper that records every `get_or_init` call so the
/// test can assert the proxy looked up exactly one session per
/// distinct identity.
struct ObservingStore {
    inner: InMemorySessionStore,
    looked_up: Mutex<Vec<noodle_core::SessionId>>,
}

impl ObservingStore {
    fn new() -> Self {
        Self {
            inner: InMemorySessionStore::new(),
            looked_up: Mutex::new(Vec::new()),
        }
    }
    fn looked_up_ids(&self) -> Vec<noodle_core::SessionId> {
        self.looked_up.lock().unwrap().clone()
    }
    fn distinct_ids(&self) -> usize {
        self.looked_up_ids()
            .into_iter()
            .collect::<HashSet<_>>()
            .len()
    }
}

impl SessionStore for ObservingStore {
    fn get_or_init(&self, id: &noodle_core::SessionId) -> Arc<Session> {
        self.looked_up.lock().unwrap().push(id.clone());
        self.inner.get_or_init(id)
    }
}

async fn spawn_upstream() -> std::net::SocketAddr {
    let exec = Executor::default();
    let listener = TcpListener::build(exec.clone())
        .bind_address("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream local_addr");
    let svc = HttpServer::auto(exec).service(service_fn(|_req: Request| async move {
        Ok::<_, Infallible>(
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(Body::from("ok"))
                .unwrap(),
        )
    }));
    tokio::spawn(async move {
        listener.serve(svc).await;
    });
    addr
}

async fn spawn_proxy(
    sink: Arc<CapturingSink>,
    sessions: Arc<dyn SessionStore>,
) -> noodle_proxy::ProxyHandle {
    start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 2 * 1024 * 1024,
        wire: sink,
        codecs: None,
        engine: None,
        filters: Vec::new(),
        enhancers: Vec::new(),
        context: None,
        sessions,
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

#[tokio::test]
async fn same_session_header_resolves_to_one_session_across_requests() {
    let upstream = spawn_upstream().await;
    let store = Arc::new(ObservingStore::new());
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone(), store.clone()).await;

    let client = proxied_client(proxy.local_addr());

    // Three requests carrying the same x-noodle-session header.
    for _ in 0..3 {
        let resp = client
            .get(format!("http://{upstream}/turn"))
            .header("authorization", "Bearer test-token")
            .header("x-noodle-session", "session-A")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
    }

    let ids = store.looked_up_ids();
    assert_eq!(ids.len(), 3, "one lookup per request");
    assert_eq!(
        store.distinct_ids(),
        1,
        "same session header → same SessionId across requests"
    );

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn different_session_headers_resolve_to_distinct_sessions() {
    let upstream = spawn_upstream().await;
    let store = Arc::new(ObservingStore::new());
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone(), store.clone()).await;

    let client = proxied_client(proxy.local_addr());

    for sid in &["session-A", "session-B", "session-C"] {
        let resp = client
            .get(format!("http://{upstream}/turn"))
            .header("authorization", "Bearer test-token")
            .header("x-noodle-session", *sid)
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
    }

    assert_eq!(store.distinct_ids(), 3);

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn missing_headers_resolve_to_anonymous_session() {
    // No auth, no session header → all anonymous requests share one
    // SessionId. This is the lenient debug-friendly behaviour;
    // strict-mode rejection is a future config knob per the design.
    let upstream = spawn_upstream().await;
    let store = Arc::new(ObservingStore::new());
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy(sink.clone(), store.clone()).await;

    let client = proxied_client(proxy.local_addr());

    for _ in 0..2 {
        let resp = client
            .get(format!("http://{upstream}/anon"))
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 200);
    }

    assert_eq!(
        store.distinct_ids(),
        1,
        "two anonymous requests share one session"
    );

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}
