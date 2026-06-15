#![allow(deprecated)]
// A.8.a: integration test exercises the legacy ProviderCodec path. Migration to layered tracked under A.8.b.

//! End-to-end gate for ADR 018 item 3 18.6: the outbound request
//! pipeline wired into `WireLogService`. `WireLogLayer::with_engine`
//! over an *echo* inner — the inner returns the request body it
//! received as the response body, so the test reads back exactly
//! what the proxy forwarded **upstream**.
//!
//! Directive enhancement lives at the raw-body `ContextEnhancer` seam
//! (`ConfiguredAnthropicEnhancer`, ADR 048 gap review R3) and runs
//! BEFORE the engine in production (`apply_enhancers` →
//! `WireLogService`). The tests mirror that order: the enhanced
//! body must survive the engine's decode → encode round-trip to
//! the upstream bytes, and un-enhanced / unmatched requests must
//! remain byte-identical (§8).

use std::convert::Infallible;
use std::sync::Arc;

use noodle_adapters::enhancer::ConfiguredAnthropicEnhancer;
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::request::anthropic_messages::AnthropicMessagesRequestCodec;
use noodle_adapters::request::claude_ai::ClaudeAiChatRequestCodec;
use noodle_adapters::sse::SseFrameCodec;
use noodle_core::config::context::{Enhancement, Placement};
use noodle_core::layered::{BodyFrameEvent, CodecRegistry as LayeredRegistry, InspectionEngine};
use noodle_core::{ContextEnhancer, NormalizedEvent, NormalizedRequest, WireSink};
use noodle_proxy::wirelog::WireLogLayer;
use rama::{
    Layer as _, Service,
    bytes::Bytes,
    http::{Body, Request, Response, StatusCode, body::util::BodyExt},
    service::service_fn,
};

const DIRECTIVE: &str =
    "<system-reminder>Begin every reply with the noodle tags.</system-reminder>";

#[derive(Default)]
struct NullWire;
impl WireSink for NullWire {
    fn record(&self, _e: noodle_core::WireEvent) {}
}

/// Inner that echoes the received request body as the response
/// body — the proxy's *upstream* view.
fn echo_inner() -> impl Service<Request, Output = Response, Error = Infallible> + Clone {
    service_fn(|req: Request| async move {
        let bytes = req
            .into_body()
            .collect()
            .await
            .map(rama::http::body::util::Collected::to_bytes)
            .unwrap_or_default();
        Ok::<_, Infallible>(
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from(bytes))
                .unwrap(),
        )
    })
}

/// Engine mirroring `tap_setup` post-R3: request codecs, no
/// request transforms (enhancement is the raw-body seam's job).
fn engine() -> Arc<InspectionEngine> {
    Arc::new(
        InspectionEngine::builder()
            .l4_codecs(
                LayeredRegistry::<Bytes, BodyFrameEvent>::builder()
                    .with_codec(SseFrameCodec)
                    .build(),
            )
            .l5_codecs(
                LayeredRegistry::<BodyFrameEvent, NormalizedEvent>::builder()
                    .with_codec(LayeredAnthropicCodec)
                    .build(),
            )
            .request_codecs(
                LayeredRegistry::<Bytes, NormalizedRequest>::builder()
                    .with_codec(AnthropicMessagesRequestCodec)
                    .with_codec(ClaudeAiChatRequestCodec)
                    .build(),
            )
            .build(),
    )
}

fn svc(
    eng: Arc<InspectionEngine>,
) -> impl Service<Request, Output = Response, Error: std::fmt::Debug> {
    WireLogLayer::with_engine(Arc::new(NullWire) as Arc<dyn WireSink>, eng).layer(echo_inner())
}

/// Like [`svc`] but wires the request-body enhancers into the
/// `WireLogService` exactly as `mitm::build_mitm_service_with_issuer`
/// does for the production HTTPS relay (ADR 048 gap review R3). The
/// service — not the test — performs enhancement here.
fn svc_with_enhancers(
    eng: Arc<InspectionEngine>,
    enhancers: Arc<Vec<Arc<dyn ContextEnhancer>>>,
) -> impl Service<Request, Output = Response, Error: std::fmt::Debug> {
    WireLogLayer::with_engine(Arc::new(NullWire) as Arc<dyn WireSink>, eng)
        .with_enhancers(enhancers)
        .layer(echo_inner())
}

fn directive_enhancers() -> Arc<Vec<Arc<dyn ContextEnhancer>>> {
    Arc::new(vec![
        Arc::new(ConfiguredAnthropicEnhancer::new(vec![Enhancement {
            r#as: Placement::UserPrepend,
            text: DIRECTIVE.into(),
            tags: Vec::new(),
        }])) as Arc<dyn ContextEnhancer>,
    ])
}

async fn forwarded<S>(service: &S, req: Request) -> Bytes
where
    S: Service<Request, Output = Response>,
    S::Error: std::fmt::Debug,
{
    let resp = service.serve(req).await.expect("serve");
    resp.into_body().collect().await.expect("drain").to_bytes()
}

/// Apply the raw-seam enhancer exactly as production does
/// (`apply_enhancers` before the request reaches `WireLogService`).
fn raw_seam_enhance(body: &str, placement: Placement) -> Bytes {
    let enhancer = ConfiguredAnthropicEnhancer::new(vec![Enhancement {
        r#as: placement,
        text: DIRECTIVE.into(),
        tags: Vec::new(),
    }]);
    let session = noodle_core::Session::new(
        noodle_core::SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id(),
    );
    let ctx = noodle_core::EnhanceContext {
        provider: "anthropic",
        path: "/v1/messages",
        session: &session,
    };
    enhancer
        .enhance(&ctx, Bytes::from(body.to_owned()))
        .expect("enhance")
}

// Realistic shape: Claude Code always sends a top-level `system`
// (it is also one of `is_anthropic_shaped`'s two tells).
const ANTHROPIC_BODY: &str = r#"{"model":"claude-sonnet-4-6","system":[{"type":"text","text":"agent"}],"messages":[{"role":"user","content":"hi"}]}"#;

const CLAUDE_AI_BODY: &str = r#"{"prompt":"what is a man in the middle?","personalized_styles":[{"type":"default","key":"Default","name":"Normal","prompt":"Normal\n","isDefault":true}],"model":"claude-haiku-4-5-20251001"}"#;

/// The raw-seam-enhanced body must survive the engine's
/// decode → encode round-trip to the upstream bytes with the
/// directive intact at the configured placement.
#[tokio::test]
async fn enhanced_anthropic_body_reaches_upstream_with_directive() {
    let service = svc(engine());
    let enhanced = raw_seam_enhance(ANTHROPIC_BODY, Placement::UserPrepend);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", "api.anthropic.com")
        .header("content-type", "application/json")
        .body(Body::from(enhanced))
        .unwrap();

    let body = forwarded(&service, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("forwarded body is JSON");

    let blocks = v["messages"][0]["content"]
        .as_array()
        .expect("user content normalized to block form");
    assert_eq!(
        blocks[0]["text"],
        DIRECTIVE,
        "directive must lead the last user message upstream: {}",
        String::from_utf8_lossy(&body),
    );
    // Conversation integrity: the user's text is untouched.
    assert!(
        blocks.iter().any(|b| b["text"].as_str() == Some("hi")),
        "the user's own text must survive: {}",
        String::from_utf8_lossy(&body),
    );
}

/// ADR 048 gap review R3 (MITM enhancer wiring) — the regression
/// the original suite missed. Earlier tests pre-enhanced the body
/// via `raw_seam_enhance` and only proved the engine *round-trips* an
/// already-enhanced body; they never proved the **service enhances**.
/// Production HTTPS traffic terminates in the MITM relay, which wires
/// the enhancers into `WireLogService` (not the plain-HTTP leaf), so
/// the service itself must add the directive. Here the client body
/// arrives CLEAN on the real `/v1/messages?beta=true` path (note the
/// query string — present on every live Claude Code request) and the
/// directive must appear at `user_prepend` in the forwarded bytes.
#[tokio::test]
async fn wirelog_service_enhances_clean_anthropic_body() {
    let service = svc_with_enhancers(engine(), directive_enhancers());
    // CLEAN body — NOT passed through `raw_seam_enhance`. If the
    // service doesn't enhance, this body reaches upstream verbatim
    // and the assertion fails — which is exactly what the deployed
    // proxy did before this fix.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("host", "api.anthropic.com")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();

    let body = forwarded(&service, req).await;
    let v: serde_json::Value = serde_json::from_slice(&body).expect("forwarded body is JSON");
    let blocks = v["messages"][0]["content"]
        .as_array()
        .expect("user content normalized to block form");
    assert_eq!(
        blocks[0]["text"],
        DIRECTIVE,
        "the SERVICE must enhance the directive at user_prepend on a clean body: {}",
        String::from_utf8_lossy(&body),
    );
    assert!(
        blocks.iter().any(|b| b["text"].as_str() == Some("hi")),
        "the user's own text must survive: {}",
        String::from_utf8_lossy(&body),
    );
}

/// Idempotence on the service path (G0 content-based dedup): a body
/// that already carries the directive must not be doubled when it
/// flows through the enhancer-wired service again.
#[tokio::test]
async fn wirelog_service_does_not_double_enhance() {
    let service = svc_with_enhancers(engine(), directive_enhancers());
    let pre_enhanced = raw_seam_enhance(ANTHROPIC_BODY, Placement::UserPrepend);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("host", "api.anthropic.com")
        .header("content-type", "application/json")
        .body(Body::from(pre_enhanced))
        .unwrap();

    let body = forwarded(&service, req).await;
    let occurrences = String::from_utf8_lossy(&body).matches(DIRECTIVE).count();
    assert_eq!(
        occurrences,
        1,
        "directive must appear exactly once (content-idempotent): {}",
        String::from_utf8_lossy(&body),
    );
}

/// claude.ai chat-shape enhancement retired with the engine-path
/// enhancer (v1 scope is the Anthropic Messages cell — ADR 048
/// Appendix A; `tap_setup/mod.rs` notes the follow-up). The chat
/// body must now pass through byte-identical.
#[tokio::test]
async fn claude_ai_chat_request_passes_through_byte_identical() {
    let service = svc(engine());
    let req = Request::builder()
        .method("POST")
        .uri("/api/organizations/org-1/chat_conversations/conv-1/completion")
        .header("host", "claude.ai")
        .header("content-type", "application/json")
        .body(Body::from(CLAUDE_AI_BODY))
        .unwrap();

    let body = forwarded(&service, req).await;
    assert_eq!(
        body,
        Bytes::from(CLAUDE_AI_BODY),
        "claude.ai chat body must round-trip byte-identical (no enhancer registered for this cell)",
    );
}

#[tokio::test]
async fn unmatched_endpoint_passes_through_byte_identical() {
    let service = svc(engine());
    let original = r#"{"messages":[{"role":"user","content":"x"}]}"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", "telemetry.example.com") // no codec matches
        .header("content-type", "application/json")
        .body(Body::from(original))
        .unwrap();

    let body = forwarded(&service, req).await;
    assert_eq!(
        body,
        Bytes::from(original),
        "an unmatched endpoint must forward the client body verbatim",
    );
}

#[tokio::test]
async fn matched_but_unenhanced_is_byte_identical() {
    // Codec matches but the raw seam enhanced nothing → ADR 018
    // §8 requires byte-identical raw replay through the engine.
    let service = svc(engine());
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", "api.anthropic.com")
        .header("content-type", "application/json")
        .body(Body::from(ANTHROPIC_BODY))
        .unwrap();

    let body = forwarded(&service, req).await;
    assert_eq!(
        body,
        Bytes::from(ANTHROPIC_BODY),
        "un-enhanced request must round-trip byte-identical (§8)",
    );
}
