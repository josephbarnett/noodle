#![allow(deprecated)]
// A.8.a: integration test exercises the legacy ProviderCodec path. Migration to layered tracked under A.8.b.

//! Slice 031.c — **the closing test for backlog item 4**.
//!
//! Proves the full attribution-product loop closes end-to-end:
//!
//! 1. Client sends a request with `User-Agent: Claude-Code/...`
//!    and the directive-enhancement probe through the proxy.
//! 2. The request half runs (raw-seam `ConfiguredAnthropicEnhancer`
//!    would enhance in production; the e2e doesn't require it
//!    because what matters here is the side-effect bus).
//! 3. The engine's `UserAgentDetector` (a `RequestDetector` per
//!    ADR 021) derives a `Hint { category: "tool", value: "Claude
//!    Code", confidence: 0.95 }` from the UA header at flow open.
//!    Replaced slice 031.c's inline `user_agent_hint` stand-in.
//! 4. Engine drains the request-flow side-effects through the
//!    `SideEffectsJsonlSink` and runs the Resolver. A
//!    `ResolvedRecord { resolved: {"tool": "Claude Code"} }`
//!    lands on the sink as a JSONL line.
//! 5. Upstream responds with an SSE stream containing a
//!    `<noodle:work_type>refactor</noodle:work_type>` marker.
//! 6. `MarkerStripTransform` strips the marker (031.b's encode
//!    path makes the strip reach the client); a Resolved record
//!    for the response flow is also emitted on the sink.
//! 7. Client receives bytes with no marker.
//! 8. `side_effects.jsonl` carries:
//!    - the Hint (request side, from UA)
//!    - the Artifact (response side, from marker-strip)
//!    - at least one Resolved with the attribution `tool` entry
//!
//! This is the milestone the entire item 4 was working toward.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::enhancer::ConfiguredAnthropicEnhancer;
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::request::anthropic_messages::AnthropicMessagesRequestCodec;
use noodle_adapters::request_detector::UserAgentDetector;
use noodle_adapters::sse::SseFrameCodec;
use noodle_adapters::transform::marker_strip::MarkerStripTransform;
use noodle_core::config::context::{Enhancement, Placement};
use noodle_core::layered::{
    BodyFrameEvent, CodecRegistry as LayeredRegistry, InspectionEngine, Layer, Pipeline,
    RequestDetectorRegistry, SideEffectSink, TransformAttachment, TransformRegistry,
};
use noodle_core::{ContextEnhancer, NormalizedEvent, NormalizedRequest, WireSink};
use noodle_proxy::wirelog::WireLogLayer;
use noodle_sinks::SideEffectsJsonlSink;
use rama::{
    Layer as RamaLayer, Service,
    bytes::Bytes,
    http::{Body, Request, Response, StatusCode, body::util::BodyExt},
    service::service_fn,
};

#[derive(Default)]
struct NoopWire;
impl WireSink for NoopWire {
    fn record(&self, _e: noodle_core::WireEvent) {}
}

const DIRECTIVE: &str = "<system-reminder>Begin every reply with <noodle:work_type>VALUE</noodle:work_type>.</system-reminder>";

const ANTHROPIC_STREAM_WITH_MARKER: &[&str] = &[
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_full_loop\",\"role\":\"assistant\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Pre-text <noodle:work_type>refactor</noodle:work_type> post-text.\"}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

fn sse_inner_with_request_capture(
    chunks: &'static [&'static str],
    captured: Arc<Mutex<Vec<u8>>>,
) -> impl Service<Request, Output = Response, Error = std::convert::Infallible> + Clone {
    service_fn(move |req: Request| {
        let captured = Arc::clone(&captured);
        async move {
            // Capture the request body bytes that the proxy
            // forwarded upstream (so the e2e can also verify
            // the request enhancement / passthrough behaviour).
            let (parts, body) = req.into_parts();
            let req_bytes = body.collect().await.expect("collect req").to_bytes();
            captured
                .lock()
                .expect("capture")
                .extend_from_slice(&req_bytes);
            let _ = parts; // keep the parts alive to mirror real usage
            let stream = async_stream::stream! {
                for chunk in chunks {
                    tokio::time::sleep(Duration::from_millis(0)).await;
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(chunk.as_bytes()));
                }
            };
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap(),
            )
        }
    })
}

/// The full-loop e2e. Builds an `InspectionEngine` mirroring
/// what `tap_setup` registers under `NOODLE_LAYERED_CORE`:
/// request codec on request side (enhancement at the raw seam),
/// `MarkerStripTransform` on the response side,
/// `SideEffectsJsonlSink` composed with `TracingSink`, the
/// default `CategoryConfig` (`with_attribution_defaults`).
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn full_attribution_loop_closes_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let jsonl_path = dir.path().join("side_effects.jsonl");
    let jsonl_sink = SideEffectsJsonlSink::spawn(&jsonl_path)
        .await
        .expect("spawn");

    let sink: Arc<dyn SideEffectSink> = Arc::new(jsonl_sink);

    let engine = Arc::new(
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
            .l5_transforms(
                TransformRegistry::<NormalizedEvent>::builder()
                    .with_transform(
                        MarkerStripTransform::new(["work_type", "tool"]),
                        TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
                    )
                    .build(),
            )
            .request_codecs(
                LayeredRegistry::<Bytes, NormalizedRequest>::builder()
                    .with_codec(AnthropicMessagesRequestCodec)
                    .build(),
            )
            // No engine request transforms post-R3 — enhancement is
            // the raw-body seam's job (applied below, before the
            // service, exactly as `apply_enhancers` does in
            // production).
            .request_transforms(TransformRegistry::<NormalizedRequest>::builder().build())
            // ADR 021: header-level Detector emits the UA hint.
            // Mirrors `tap_setup`'s production wiring; this is
            // what makes Property 2 (UA-derived `tool` Hint
            // present in `side_effects.jsonl`) pass.
            .request_detectors(
                RequestDetectorRegistry::builder()
                    .with_detector(UserAgentDetector::new())
                    .build(),
            )
            .sink(Arc::clone(&sink))
            .build(),
    );

    let upstream_req_capture: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let svc = WireLogLayer::with_engine(Arc::new(NoopWire) as Arc<dyn WireSink>, engine).layer(
        sse_inner_with_request_capture(
            ANTHROPIC_STREAM_WITH_MARKER,
            Arc::clone(&upstream_req_capture),
        ),
    );

    // Realistic-shape Anthropic Messages request, with the
    // Claude-Code User-Agent that drives the v1 UA-derived hint.
    let req_body = r#"{
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 64,
        "system": [{"type":"text","text":"You are helpful."}],
        "messages": [{"role":"user","content":"Help me refactor."}],
        "stream": true
    }"#;
    // Request half of the loop: the raw-body seam enhances the
    // operator's verbatim directive BEFORE the request reaches
    // the WireLogService — mirrors production `apply_enhancers`
    // order (ADR 048 gap review R3).
    let enhancer = ConfiguredAnthropicEnhancer::new(vec![Enhancement {
        r#as: Placement::UserPrepend,
        text: DIRECTIVE.into(),
        tags: Vec::new(),
    }]);
    let enhance_session = noodle_core::Session::new(
        noodle_core::SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id(),
    );
    let enhanced_body = enhancer
        .enhance(
            &noodle_core::EnhanceContext {
                provider: "anthropic",
                path: "/v1/messages",
                session: &enhance_session,
            },
            Bytes::from(req_body),
        )
        .expect("enhance");
    assert_ne!(
        enhanced_body,
        Bytes::from(req_body),
        "raw seam must have mutated the request"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", "api.anthropic.com")
        .header("content-type", "application/json")
        .header("user-agent", "Claude-Code/0.42.0 (linux; arm64)")
        .body(Body::from(enhanced_body))
        .unwrap();
    let resp = svc.serve(req).await.expect("serve");
    let client_body = resp.into_body().collect().await.expect("drain").to_bytes();

    // Drop everything that holds a sink reference so the
    // `SideEffectsJsonlSink`'s async writer task observes its
    // last sender go away and drains pending bytes to disk.
    drop(svc);
    drop(sink);
    // Async writer task drains on channel close. Give it a
    // window longer than `SIDE_EFFECT_FLUSH_INTERVAL` (100ms)
    // before reading the file.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ─── Property 1: marker absent from client-visible bytes ──
    let client_str = String::from_utf8_lossy(&client_body);
    assert!(
        !client_str.contains("<noodle:work_type>"),
        "marker leaked to client bytes:\n{client_str}"
    );
    assert!(
        !client_str.contains("refactor</noodle:work_type>"),
        "marker close tag + value leaked to client bytes:\n{client_str}"
    );
    assert!(
        client_str.contains("Pre-text"),
        "pre-marker prose missing — strip removed too much:\n{client_str}"
    );
    assert!(
        client_str.contains("post-text"),
        "post-marker prose missing — strip removed too much:\n{client_str}"
    );

    // ─── Property 2: side_effects.jsonl carries the attribution ──
    let jsonl = std::fs::read_to_string(&jsonl_path).expect("side_effects.jsonl exists");
    let entries: Vec<serde_json::Value> = jsonl
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is valid JSON"))
        .collect();
    assert!(
        !entries.is_empty(),
        "side_effects.jsonl is empty — drain wiring failed"
    );

    // The UA-derived Hint must be present.
    let ua_hint = entries
        .iter()
        .find(|v| v["kind"] == "hint" && v["category"] == "tool" && v["source"] == "user_agent");
    assert!(
        ua_hint.is_some(),
        "expected a UA-derived 'tool' Hint with source='user_agent' in side_effects.jsonl;\nentries: {entries:#?}"
    );
    assert_eq!(ua_hint.unwrap()["value"], "Claude Code");

    // The marker-strip Artifact must be present.
    let marker_artifact = entries
        .iter()
        .find(|v| v["kind"] == "artifact" && v["source_transform"] == "marker-strip");
    assert!(
        marker_artifact.is_some(),
        "expected a marker-strip Artifact in side_effects.jsonl;\nentries: {entries:#?}"
    );
    assert_eq!(marker_artifact.unwrap()["name"], "work_type");
    assert_eq!(marker_artifact.unwrap()["value"], "refactor");

    // The marker-strip Hint must also be present — without this,
    // the Artifact is observable but the Resolver never sees the
    // model's self-tag, so no `work_type` attribution emerges.
    // (The marker-strip-emits-Hint fix.)
    let marker_hint = entries
        .iter()
        .find(|v| v["kind"] == "hint" && v["source"] == "marker");
    assert!(
        marker_hint.is_some(),
        "expected a 'marker'-source Hint in side_effects.jsonl — \
         the marker-strip-emits-Hint contract is broken;\nentries: {entries:#?}"
    );
    assert_eq!(marker_hint.unwrap()["category"], "work_type");
    assert_eq!(marker_hint.unwrap()["value"], "refactor");

    // At least one ResolvedRecord must include the resolved tool
    // category (from the UA-derived Hint, request-side).
    let attribution_resolved_tool = entries
        .iter()
        .find(|v| v["kind"] == "resolved" && v["resolved"]["tool"] == "Claude Code");
    assert!(
        attribution_resolved_tool.is_some(),
        "expected a Resolved record with tool='Claude Code' in side_effects.jsonl — \
         the attribution loop has not closed end-to-end (request side);\nentries: {entries:#?}"
    );

    // At least one ResolvedRecord must include the resolved
    // work_type category (from the marker-derived Hint,
    // response-side). This is the loop-closes-from-marker proof.
    let attribution_resolved_work_type = entries
        .iter()
        .find(|v| v["kind"] == "resolved" && v["resolved"]["work_type"] == "refactor");
    assert!(
        attribution_resolved_work_type.is_some(),
        "expected a Resolved record with work_type='refactor' in side_effects.jsonl — \
         the marker-derived attribution path is broken (response side);\nentries: {entries:#?}"
    );

    // ─── Property 3: the directive reached the upstream bytes ──
    // The raw-seam enhancement applied before the service must
    // survive the engine's decode → encode round-trip to upstream
    // — the request half of the loop (ADR 048 gap review R3).
    let upstream_bytes = upstream_req_capture.lock().unwrap().clone();
    assert!(
        !upstream_bytes.is_empty(),
        "upstream never saw the request — proxy short-circuited"
    );
    let upstream_str = String::from_utf8_lossy(&upstream_bytes);
    assert!(
        upstream_str.contains("Begin every reply with"),
        "operator directive missing from the upstream request bytes:\n{upstream_str}"
    );
}
