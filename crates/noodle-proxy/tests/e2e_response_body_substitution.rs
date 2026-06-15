#![allow(deprecated)]
// A.8.a: integration test exercises the legacy ProviderCodec path. Migration to layered tracked under A.8.b.

//! Slice 031.b proof: the engine's encoded response bytes
//! actually reach the client through `WireLogLayer`.
//!
//! Three properties:
//! 1. **Unmutated stream passes through byte-identical.**
//!    Without any transform, the substituted bytes equal the
//!    upstream bytes — ADR 015 §2.1.1 round-trip invariant, ADR
//!    017 provenance (`FrameSource::Upstream` replays verbatim).
//! 2. **`MarkerStripTransform`'s mutation reaches the wire.**
//!    With the transform registered, the `<noodle:NAME>VALUE`
//!    marker is **absent** from the bytes the client receives.
//!    This is the test ADR 017 §7 explicitly deferred until
//!    item 4's wirelog wiring landed — that is, this test.
//! 3. **Engine produces a `ResolvedRecord` at flow end on the
//!    sink.** Slice 031.a wired the engine helper; slice 031.b
//!    wires the wirelog to call it from `EngineState::finish`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::sse::SseFrameCodec;
use noodle_adapters::transform::marker_strip::MarkerStripTransform;
use noodle_core::layered::{
    BodyFrameEvent, CodecRegistry as LayeredRegistry, InspectionEngine, Layer, Pipeline,
    ResolvedRecord, SideEffect, SideEffectSink, TransformAttachment, TransformRegistry,
};
use noodle_core::{NormalizedEvent, WireSink};
use noodle_proxy::wirelog::WireLogLayer;
use rama::{
    Layer as RamaLayer, Service,
    bytes::Bytes,
    http::{Body, Request, Response, StatusCode, body::util::BodyExt},
    service::service_fn,
};

#[derive(Default)]
struct CapturingWire(Mutex<Vec<noodle_core::WireEvent>>);
impl WireSink for CapturingWire {
    fn record(&self, e: noodle_core::WireEvent) {
        self.0.lock().unwrap().push(e);
    }
}

#[derive(Default)]
struct CapturingSideEffects(Mutex<Vec<SideEffect>>);
impl SideEffectSink for CapturingSideEffects {
    fn record(&self, effect: SideEffect) {
        self.0.lock().unwrap().push(effect);
    }
}

impl CapturingSideEffects {
    fn resolved(&self) -> Vec<ResolvedRecord> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                SideEffect::Resolved(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Anthropic SSE stream with a `<noodle:work_type>refactor</noodle:work_type>`
/// marker embedded mid-token.
const STREAM_WITH_MARKER: &[&str] = &[
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Here is the plan. <noodle:work_type>refactor</noodle:work_type>Proceeding now.\"}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

const STREAM_PLAIN: &[&str] = &[
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_y\",\"role\":\"assistant\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Plain output, no marker.\"}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

fn sse_inner(
    chunks: &'static [&'static str],
) -> impl Service<Request, Output = Response, Error = std::convert::Infallible> + Clone {
    service_fn(move |_req: Request| async move {
        let stream = async_stream::stream! {
            for chunk in chunks {
                tokio::time::sleep(Duration::from_millis(0)).await;
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(chunk.as_bytes()));
            }
        };
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap())
    })
}

/// Engine with the marker-strip transform registered on the L5
/// response chain (matches what `tap_setup` registers under
/// `NOODLE_LAYERED_CORE`).
fn engine_with_marker_strip(sink: Arc<dyn SideEffectSink>) -> Arc<InspectionEngine> {
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
            .l5_transforms(
                TransformRegistry::<NormalizedEvent>::builder()
                    .with_transform(
                        // Allow-list must be non-empty: an empty
                        // list disables the scanner (per
                        // MarkerScanner::enabled). The marker
                        // grammar is fixed for v1; story 034
                        // makes both the grammar and the
                        // allow-list configurable.
                        MarkerStripTransform::new(["work_type"]),
                        TransformAttachment::new(Layer::VendorSemantics, Pipeline::Response, 0),
                    )
                    .build(),
            )
            .sink(sink)
            .build(),
    )
}

/// Engine without any transform — the unmutated baseline.
fn engine_passthrough(sink: Arc<dyn SideEffectSink>) -> Arc<InspectionEngine> {
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
            .sink(sink)
            .build(),
    )
}

async fn drive_stream(engine: Arc<InspectionEngine>, chunks: &'static [&'static str]) -> Vec<u8> {
    let svc = WireLogLayer::with_engine(
        Arc::new(CapturingWire::default()) as Arc<dyn WireSink>,
        engine,
    )
    .layer(sse_inner(chunks));

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", "api.anthropic.com")
        .body(Body::from(""))
        .unwrap();
    let resp = svc.serve(req).await.expect("serve");
    let body = resp.into_body().collect().await.expect("drain");
    body.to_bytes().to_vec()
}

/// Property 1: with no transforms, the response body the client
/// receives is byte-identical to what upstream emitted (modulo
/// chunk boundaries — the codecs re-emit at SSE-frame granularity
/// rather than at upstream-chunk granularity, which is fine
/// because SSE is frame-oriented).
#[tokio::test]
async fn passthrough_response_is_byte_identical_per_frame() {
    let sink = Arc::new(CapturingSideEffects::default()) as Arc<dyn SideEffectSink>;
    let engine = engine_passthrough(Arc::clone(&sink));

    let client_bytes = drive_stream(engine, STREAM_PLAIN).await;

    let upstream_bytes: Vec<u8> = STREAM_PLAIN
        .iter()
        .flat_map(|s| s.as_bytes().to_vec())
        .collect();

    assert_eq!(
        client_bytes, upstream_bytes,
        "unmutated stream must round-trip byte-identical (ADR 015 §2.1.1)"
    );
}

/// Property 2 — THE milestone of slice 031.b: the marker that
/// `MarkerStripTransform` strips is absent from the bytes the
/// client receives. Without slice 031.b's wirelog substitution,
/// this assertion would fail: the strip would update `Token.text`
/// but the original upstream bytes (with marker) would still
/// reach the client.
#[tokio::test]
async fn marker_strip_mutation_reaches_the_client_bytes() {
    let sink = Arc::new(CapturingSideEffects::default());
    let engine = engine_with_marker_strip(Arc::clone(&sink) as Arc<dyn SideEffectSink>);

    let client_bytes = drive_stream(engine, STREAM_WITH_MARKER).await;
    let body_str = String::from_utf8_lossy(&client_bytes);

    // The marker must NOT appear in what the client sees.
    assert!(
        !body_str.contains("<noodle:work_type>"),
        "marker '<noodle:work_type>' leaked to client bytes — slice 031.b's substitution failed:\n{body_str}"
    );
    assert!(
        !body_str.contains("</noodle:work_type>"),
        "closing tag '</noodle:work_type>' leaked to client bytes:\n{body_str}"
    );
    assert!(
        !body_str.contains("refactor"),
        "marker value 'refactor' leaked to client bytes:\n{body_str}"
    );

    // The surrounding prose must still be present.
    assert!(
        body_str.contains("Here is the plan."),
        "pre-marker prose missing — strip removed too much:\n{body_str}"
    );
    assert!(
        body_str.contains("Proceeding now."),
        "post-marker prose missing — strip removed too much:\n{body_str}"
    );

    // The engine's end-of-flow drain must have produced a
    // ResolvedRecord on the sink (the slice's secondary
    // property — the loop closes the Resolver hand-off).
    let resolved = sink.resolved();
    assert!(
        !resolved.is_empty(),
        "expected at least one ResolvedRecord on the sink (slice 031.b drain wiring)"
    );
}

/// Property 3 — fail-before companion to property 2: WITHOUT
/// `MarkerStripTransform` registered, the marker DOES survive to
/// the client. This is the "before" — proving the assertion in
/// property 2 is meaningful (it would trivially pass if the
/// stream were somehow stripped elsewhere).
#[tokio::test]
async fn without_marker_strip_marker_reaches_client_unchanged() {
    let sink = Arc::new(CapturingSideEffects::default()) as Arc<dyn SideEffectSink>;
    let engine = engine_passthrough(Arc::clone(&sink));

    let client_bytes = drive_stream(engine, STREAM_WITH_MARKER).await;
    let body_str = String::from_utf8_lossy(&client_bytes);

    // Without the transform, the marker survives byte-for-byte.
    assert!(
        body_str.contains("<noodle:work_type>"),
        "without strip the marker must be present in client bytes — \
         test invariant broken:\n{body_str}"
    );
    assert!(
        body_str.contains("refactor"),
        "without strip the marker value must be present:\n{body_str}"
    );
}
