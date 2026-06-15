//! Operations HTTP API — tap control, health, readiness, metrics.
//!
//! Conventionally bound to `127.0.0.1:9091` for local use and to
//! `$NOODLE_OPS_LISTEN` (e.g. `0.0.0.0:9091`) for off-machine
//! deployments (ADR 043 §2.7).
//!
//! Route table:
//!
//! ```text
//! GET    /healthz             →  200 "ok" (liveness — process is running)
//! GET    /readyz              →  200 "ready" / 503 "not ready" (engine wired)
//! GET    /metrics             →  200 Prometheus text exposition format
//! GET    /debug/tap/status    →  { "enabled": bool, "file": "..." }
//! POST   /debug/tap/enable    →  { "enabled": true,  "file": "..." }
//! POST   /debug/tap/disable   →  { "enabled": false }
//! POST   /debug/tap/clear     →  501 (deferred — restart noodle for now)
//! ```
//!
//! Lives in `noodle-proxy` (not `noodle-tap`) because it needs an HTTP
//! server (`rama`), and `noodle-tap` is engine-free by design.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use noodle_tap::TapJsonlLog;
use rama::{
    Layer,
    error::BoxError,
    http::{
        Body, HeaderName, HeaderValue, Method, Request, Response, StatusCode,
        layer::set_header::SetResponseHeaderLayer, server::HttpServer,
    },
    layer::ConsumeErrLayer,
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
};
use serde_json::json;

/// Shared state the ops endpoints expose. Cloneable across the rama
/// service; all fields are reference-counted or `Copy`.
#[derive(Clone)]
pub struct OpsState {
    pub tap: Arc<TapJsonlLog>,
    /// Flipped to `true` once the engine is wired and the proxy is
    /// ready to accept inspectable traffic. `readyz` returns 200
    /// only when this is `true`.
    pub ready: Arc<AtomicBool>,
    /// Process start time; uptime is computed from this.
    pub started_at: Instant,
}

/// Bind on `addr`, spawn the ops server task, return immediately.
/// The task lives until the proxy shuts down.
pub async fn spawn(addr: &str, state: OpsState, exec: Executor) -> Result<(), BoxError> {
    let tcp = TcpListener::build(exec.clone()).bind_address(addr).await?;
    let local = tcp.local_addr()?;
    rama::telemetry::tracing::info!(addr = %local, "ops API listening");

    // CORS: allow viewer (served from any origin) + scrapers.
    let cors = SetResponseHeaderLayer::overriding(
        HeaderName::from_static("access-control-allow-origin"),
        HeaderValue::from_static("*"),
    );

    let svc = HttpServer::auto(exec.clone()).service(
        (ConsumeErrLayer::default(), cors).into_layer(service_fn({
            move |req: Request| {
                let state = state.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(&req, &state)) }
            }
        })),
    );

    tokio::spawn(async move {
        tcp.serve(svc).await;
    });

    Ok(())
}

fn handle(req: &Request, state: &OpsState) -> Response {
    let path = req.uri().path();
    match (req.method(), path) {
        // Health: process is alive. Always 200 for a running task —
        // anything that prevents this endpoint from answering is by
        // definition unhealthy, and the kubelet's TCP/HTTP probe
        // failure will catch it.
        (&Method::GET, "/healthz") => text_ok("ok\n"),

        // Readiness: engine is wired and accepting traffic. 503
        // until the wiring completes; flips to 200 thereafter.
        (&Method::GET, "/readyz") => {
            if state.ready.load(Ordering::Acquire) {
                text_ok("ready\n")
            } else {
                text_status(StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
            }
        }

        // Prometheus scrape. Minimum-honest set: uptime + build
        // info. Additional counters land additively as their
        // emission paths get instrumented.
        (&Method::GET, "/metrics") => text_ok(&prometheus_metrics(state)),

        (&Method::GET, "/debug/tap/status") => json_ok(&json!({
            "enabled": state.tap.enabled(),
            "file": state.tap.path().to_string_lossy(),
        })),
        (&Method::POST, "/debug/tap/enable") => {
            state.tap.set_enabled(true);
            json_ok(&json!({
                "enabled": true,
                "file": state.tap.path().to_string_lossy(),
            }))
        }
        (&Method::POST, "/debug/tap/disable") => {
            state.tap.set_enabled(false);
            json_ok(&json!({"enabled": false}))
        }
        (&Method::POST, "/debug/tap/clear") => json_status(
            StatusCode::NOT_IMPLEMENTED,
            &json!({
                "error": "clear not yet implemented; restart noodle for a fresh log"
            }),
        ),
        _ => json_status(StatusCode::NOT_FOUND, &json!({"error": "not found"})),
    }
}

/// Emit the Prometheus text exposition format. Keep the metric set
/// strictly minimal-honest: only values we actually have, no
/// fabricated counters. Add metrics here as their emission paths
/// get instrumented in the engine.
fn prometheus_metrics(state: &OpsState) -> String {
    let uptime = state.started_at.elapsed().as_secs_f64();
    let version = env!("CARGO_PKG_VERSION");
    format!(
        "# HELP noodle_proxy_uptime_seconds Seconds since the proxy started.\n\
         # TYPE noodle_proxy_uptime_seconds gauge\n\
         noodle_proxy_uptime_seconds {uptime}\n\
         \n\
         # HELP noodle_proxy_build_info Build metadata. Value is always 1.\n\
         # TYPE noodle_proxy_build_info gauge\n\
         noodle_proxy_build_info{{version=\"{version}\"}} 1\n\
         \n\
         # HELP noodle_proxy_tap_enabled Whether the tap debugger is currently writing (1) or paused (0).\n\
         # TYPE noodle_proxy_tap_enabled gauge\n\
         noodle_proxy_tap_enabled {tap_enabled}\n",
        tap_enabled = u8::from(state.tap.enabled()),
    )
}

fn text_ok(body: &str) -> Response {
    text_status(StatusCode::OK, body)
}

fn text_status(code: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(code)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(body.to_owned()))
        .expect("static response builds")
}

fn json_ok(v: &serde_json::Value) -> Response {
    json_status(StatusCode::OK, v)
}

fn json_status(status: StatusCode, v: &serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

#[cfg(test)]
mod tests {
    //! End-to-end tests over the actual ops HTTP server bound to
    //! ephemeral ports — proves `/healthz`, `/readyz`, `/metrics`
    //! return the expected payloads. Pairs with ADR 043 §2.7
    //! acceptance signals.
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Drives every ops route through a freshly bound rama server
    /// on `127.0.0.1:0` (OS-assigned port). Asserts on response
    /// status + body shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn ops_endpoints_serve_the_documented_shapes() {
        let tmpdir = tempfile::tempdir().expect("tmp");
        let tap_file = tmpdir.path().join("tap.jsonl");
        let tap = Arc::new(
            TapJsonlLog::spawn(tap_file.clone(), 16)
                .await
                .expect("open tap"),
        );
        let ready = Arc::new(AtomicBool::new(false));
        let state = OpsState {
            tap,
            ready: ready.clone(),
            started_at: Instant::now(),
        };

        // Bind ourselves to a known port so the test can hit it.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe");
        let addr = listener.local_addr().expect("addr");
        drop(listener); // release for rama to re-bind

        let exec = Executor::default();
        spawn(&addr.to_string(), state, exec)
            .await
            .expect("spawn ops server");

        // Give the spawned task a tick to actually bind.
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // /healthz — always 200.
        let r = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("healthz");
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.expect("body"), "ok\n");

        // /readyz — 503 while ready is false.
        let r = client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz off");
        assert_eq!(r.status().as_u16(), 503);
        assert_eq!(r.text().await.expect("body"), "not ready\n");

        // Flip the readiness gate.
        ready.store(true, Ordering::Release);

        let r = client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .expect("readyz on");
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.expect("body"), "ready\n");

        // /metrics — Prometheus text format with the documented metrics.
        let r = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("metrics");
        assert_eq!(r.status().as_u16(), 200);
        let body = r.text().await.expect("body");
        assert!(
            body.contains("noodle_proxy_uptime_seconds"),
            "uptime metric present: {body}"
        );
        assert!(
            body.contains("noodle_proxy_build_info"),
            "build_info metric present: {body}"
        );
        assert!(
            body.contains("noodle_proxy_tap_enabled"),
            "tap_enabled metric present: {body}"
        );
        // # HELP and # TYPE lines required by the Prometheus exposition format.
        assert!(body.contains("# HELP noodle_proxy_uptime_seconds"));
        assert!(body.contains("# TYPE noodle_proxy_uptime_seconds gauge"));

        // /debug/tap/status — unchanged behaviour preserved.
        let r = client
            .get(format!("{base}/debug/tap/status"))
            .send()
            .await
            .expect("tap status");
        assert_eq!(r.status().as_u16(), 200);
        let v: serde_json::Value = r.json().await.expect("json");
        assert_eq!(v["enabled"], serde_json::json!(true));

        // /not-a-route — 404.
        let r = client
            .get(format!("{base}/nope"))
            .send()
            .await
            .expect("nope");
        assert_eq!(r.status().as_u16(), 404);
    }
}
