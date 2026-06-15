//! End-to-end: an `OpenAiAttributionEnhancer` wired into
//! `ProxyConfig.enhancers` mutates outbound OpenAI-shape JSON bodies
//! by prepending a system message.
//!
//! Setup mirrors the other e2e files — mock upstream + spawn proxy +
//! reqwest client. The mock upstream echoes the request body so the
//! test client can see exactly what the proxy sent.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::enhancer::OpenAiAttributionEnhancer;
use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{ContextEnhancer, WireEvent, WireSink};
use noodle_proxy::{ProxyConfig, start};
use rama::{
    http::{Body, Request, Response, StatusCode, body::util::BodyExt, server::HttpServer},
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

/// Mock upstream that echoes any POST body verbatim with the same
/// Content-Type. Lets the test inspect what the proxy sent.
async fn spawn_echo_upstream() -> std::net::SocketAddr {
    let exec = Executor::default();
    let listener = TcpListener::build(exec.clone())
        .bind_address("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("upstream local_addr");
    let svc = HttpServer::auto(exec).service(service_fn(|req: Request| async move {
        let ct = req
            .headers()
            .get("content-type")
            .cloned()
            .unwrap_or_else(|| "application/octet-stream".parse().unwrap());
        let bytes = req
            .into_body()
            .collect()
            .await
            .map(rama::http::body::util::Collected::to_bytes)
            .unwrap_or_default();
        let mut resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(bytes))
            .unwrap();
        resp.headers_mut().insert("content-type", ct);
        Ok::<_, Infallible>(resp)
    }));
    tokio::spawn(async move {
        listener.serve(svc).await;
    });
    addr
}

async fn spawn_proxy_with_enhancer(
    sink: Arc<CapturingSink>,
    enhancer: Arc<dyn ContextEnhancer>,
) -> noodle_proxy::ProxyHandle {
    start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 2 * 1024 * 1024,
        wire: sink,
        codecs: None,
        engine: None,
        filters: Vec::new(),
        enhancers: vec![enhancer],
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

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_shape_request_gets_directive_prepended() {
    let upstream = spawn_echo_upstream().await;
    let enhancer: Arc<dyn ContextEnhancer> =
        Arc::new(OpenAiAttributionEnhancer::new("ATTR_DIRECTIVE"));
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_enhancer(sink, enhancer).await;

    let payload = r#"{"messages":[{"role":"user","content":"build the auth flow"}]}"#;
    let echoed: String = proxied_client(proxy.local_addr())
        .post(format!("http://{upstream}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("x-noodle-session", "session-A")
        .body(payload.to_string())
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    let parsed: serde_json::Value = serde_json::from_str(&echoed).expect("echoed JSON");
    let messages = parsed["messages"].as_array().expect("messages array");

    // The proxy should have prepended a system message containing
    // the directive, ahead of the original user message.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "ATTR_DIRECTIVE");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "build the auth flow");

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn every_fresh_request_gets_the_directive() {
    let upstream = spawn_echo_upstream().await;
    let enhancer: Arc<dyn ContextEnhancer> =
        Arc::new(OpenAiAttributionEnhancer::new("ATTR_DIRECTIVE"));
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_enhancer(sink, enhancer).await;

    let client = proxied_client(proxy.local_addr());
    let payload = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let url = format!("http://{upstream}/v1/chat/completions");

    let send = || async {
        client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-noodle-session", "session-B")
            .body(payload.to_string())
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("text")
    };

    let first = send().await;
    let second = send().await;

    let first_msgs: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&first)
        .unwrap()["messages"]
        .as_array()
        .cloned()
        .unwrap();
    let second_msgs: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&second)
        .unwrap()["messages"]
        .as_array()
        .cloned()
        .unwrap();

    // ADR 048 gap review G0/G4a: the client rebuilds its body
    // every round trip and never carries our wire-only mutation —
    // so BOTH fresh requests must receive the directive. (The old
    // once-per-session gate silently dropped it from round trip 2
    // onward; that was the bug, not the contract.)
    assert_eq!(first_msgs.len(), 2);
    assert_eq!(first_msgs[0]["role"], "system");
    assert_eq!(second_msgs.len(), 2);
    assert_eq!(second_msgs[0]["role"], "system");
    assert_eq!(second_msgs[0]["content"], "ATTR_DIRECTIVE");

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

/// Content idempotence: a body that ALREADY leads with our exact
/// system directive (e.g. a replayed/pre-enhanced request) must
/// not be double-enhanced.
#[tokio::test]
async fn body_already_carrying_directive_is_not_doubled() {
    let upstream = spawn_echo_upstream().await;
    let enhancer: Arc<dyn ContextEnhancer> =
        Arc::new(OpenAiAttributionEnhancer::new("ATTR_DIRECTIVE"));
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_enhancer(sink, enhancer).await;

    let client = proxied_client(proxy.local_addr());
    let payload = r#"{"messages":[{"role":"system","content":"ATTR_DIRECTIVE"},{"role":"user","content":"hi"}]}"#;
    let url = format!("http://{upstream}/v1/chat/completions");

    let echoed = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-noodle-session", "session-pre")
        .body(payload.to_string())
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");
    let msgs: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&echoed).unwrap()
        ["messages"]
        .as_array()
        .cloned()
        .unwrap();
    assert_eq!(msgs.len(), 2, "directive must not be doubled: {echoed}");
    assert_eq!(msgs[0]["content"], "ATTR_DIRECTIVE");

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn distinct_sessions_each_get_one_enhancement() {
    let upstream = spawn_echo_upstream().await;
    let enhancer: Arc<dyn ContextEnhancer> =
        Arc::new(OpenAiAttributionEnhancer::new("ATTR_DIRECTIVE"));
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_enhancer(sink, enhancer).await;

    let client = proxied_client(proxy.local_addr());
    let payload = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let url = format!("http://{upstream}/v1/chat/completions");

    for sid in &["session-X", "session-Y"] {
        let echoed = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-noodle-session", *sid)
            .body(payload.to_string())
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("text");
        let messages: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&echoed)
            .unwrap()["messages"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(messages.len(), 2, "session {sid} got first-time enhance");
        assert_eq!(messages[0]["role"], "system");
    }

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn non_openai_body_passes_through_untouched() {
    let upstream = spawn_echo_upstream().await;
    let enhancer: Arc<dyn ContextEnhancer> =
        Arc::new(OpenAiAttributionEnhancer::new("ATTR_DIRECTIVE"));
    let sink = Arc::new(CapturingSink::default());
    let proxy = spawn_proxy_with_enhancer(sink, enhancer).await;

    // Body has no `messages` array — enhancer should not touch it.
    let payload = r#"{"prompt":"completion-style","max_tokens":42}"#;
    let echoed = proxied_client(proxy.local_addr())
        .post(format!("http://{upstream}/v1/completions"))
        .header("content-type", "application/json")
        .header("x-noodle-session", "session-Z")
        .body(payload.to_string())
        .send()
        .await
        .expect("send")
        .text()
        .await
        .expect("text");

    assert_eq!(echoed, payload);

    proxy.shutdown(Duration::from_secs(2)).await.unwrap();
}
