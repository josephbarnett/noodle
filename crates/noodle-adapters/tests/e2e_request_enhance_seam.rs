//! The bidirectional mutating engine seam, exercised with the
//! **real** request codec (`AnthropicMessagesRequestCodec`) — the
//! adapters-level complement to the core engine unit tests (which
//! use fakes).
//!
//! `RequestFlow` runs the single-stage `Bytes → NormalizedRequest
//! → Bytes` round trip (ADR 018 §9; the PR #35 two-stage spike
//! shape was superseded). Asserts:
//!
//! - **fail-before** (no transform): the engine request path is
//!   byte-faithful — bytes out == bytes in (015 §2.1.1, ADR 018
//!   §8). Proves the encode seam is honest, not lossy, so any
//!   later difference is the enhancement.
//! - **pass-after** (with an enhancing transform): the directive
//!   reaches the re-encoded upstream `system` array and the
//!   enhancement is recorded on the side channel.
//!
//! Scope: proves the *seam* with a real codec. The full proxy
//! wiring + both domains is `noodle-proxy`'s
//! `e2e_request_enhance.rs` (item 3 18.6c).

use bytes::Bytes;
use http::{HeaderMap, Method};
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::request::anthropic_messages::AnthropicMessagesRequestCodec;
use noodle_adapters::sse::SseFrameCodec;
use noodle_core::NormalizedRequest;
use noodle_core::layered::{
    AuditEvent, AuditKind, CodecProbe, CodecRegistry, InspectionEngine, Layer, Pipeline,
    SideChannelTx, SideEffect, Transform, TransformAttachment, TransformInstance,
    TransformRegistry,
};

const DIRECTIVE: &str = "<<noodle-enhanced-directive>>";

/// Request enhancer: sets the abstract `SystemDirective` (the
/// per-domain codec maps it to the wire `system` slot) and records
/// an `Enhanced` audit. Stands in for the real
/// `AttributionEnhancer` — this file's job is the *seam*, not the
/// directive text.
struct EnhanceTransform;
struct EnhanceInstance;

impl Transform for EnhanceTransform {
    type Event = NormalizedRequest;
    fn name(&self) -> &'static str {
        "spike.enhance"
    }
    fn open(
        &self,
        _a: &TransformAttachment,
    ) -> Box<dyn TransformInstance<Event = NormalizedRequest>> {
        Box::new(EnhanceInstance)
    }
}

impl TransformInstance for EnhanceInstance {
    type Event = NormalizedRequest;
    fn apply(
        &mut self,
        mut ev: NormalizedRequest,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<NormalizedRequest> {
        ev.system.set_directive(DIRECTIVE);
        side.emit_audit(AuditEvent {
            kind: AuditKind::Enhanced,
            layer: Layer::VendorSemantics,
            transform: smol_str::SmolStr::new_static("spike.enhance"),
            flow_id: 0,
            at_unix_ms: 0,
            detail: serde_json::json!({ "directive": DIRECTIVE }),
            correlation: None,
        });
        vec![ev]
    }
}

const BODY: &[u8] =
    br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hello world"}]}"#;

fn probe<'a>(method: &'a Method, headers: &'a HeaderMap) -> CodecProbe<'a> {
    CodecProbe {
        host: "api.anthropic.com",
        path: "/v1/messages",
        method,
        request_headers: headers,
        response_status: None,
        response_content_type: None,
    }
}

fn engine(enhance: bool) -> InspectionEngine {
    let mut xforms = TransformRegistry::<NormalizedRequest>::builder();
    if enhance {
        xforms = xforms.with_transform(
            EnhanceTransform,
            TransformAttachment::new(Layer::VendorSemantics, Pipeline::Request, 0),
        );
    }
    // Response registries are required by the builder even though
    // this test drives only the request path.
    InspectionEngine::builder()
        .l4_codecs(CodecRegistry::builder().with_codec(SseFrameCodec).build())
        .l5_codecs(
            CodecRegistry::builder()
                .with_codec(LayeredAnthropicCodec)
                .build(),
        )
        .request_codecs(
            CodecRegistry::builder()
                .with_codec(AnthropicMessagesRequestCodec)
                .build(),
        )
        .request_transforms(xforms.build())
        .build()
}

fn run(enhance: bool) -> (Vec<u8>, Vec<SideEffect>) {
    let eng = engine(enhance);
    let method = Method::POST;
    let headers = HeaderMap::new();
    let mut flow = eng
        .open_request_flow(&probe(&method, &headers))
        .expect("request flow opens (request codec matches)");

    let mut out = flow.push_bytes(Bytes::from_static(BODY));
    let tail = flow.finish();

    let mut wire = Vec::new();
    for b in out.bytes.iter().chain(tail.bytes.iter()) {
        wire.extend_from_slice(b);
    }
    out.side_effects.extend(tail.side_effects);
    (wire, out.side_effects)
}

/// fail-before: with no transform the engine request path is
/// byte-faithful (ADR 018 §8). Proves the encode seam is honest
/// and that any later difference is the enhancement, not codec
/// lossiness.
#[test]
fn request_flow_without_transform_is_byte_faithful() {
    let (wire, side) = run(false);
    assert_eq!(
        wire.as_slice(),
        BODY,
        "engine request round trip must be byte-exact when nothing mutates",
    );
    assert!(side.is_empty(), "no transform ⇒ no side effects");
}

/// pass-after: the enhancing transform's directive reaches the
/// re-encoded upstream bytes (in the `system` slot the codec maps
/// it to), and the enhancement is recorded. The seam works.
#[test]
fn request_flow_enhancement_reaches_upstream_bytes() {
    let (wire, side) = run(true);
    let v: serde_json::Value = serde_json::from_slice(&wire).expect("re-encoded body is JSON");

    assert_eq!(
        v["messages"][0]["content"], "hello world",
        "user message preserved verbatim",
    );
    let system = v["system"]
        .as_array()
        .expect("directive landed in the `system` block list");
    assert!(
        system.iter().any(|b| b["text"].as_str() == Some(DIRECTIVE)),
        "INJECTED directive must reach upstream bytes: {wire:?}",
    );
    assert!(
        side.iter().any(|e| matches!(
            e,
            SideEffect::Audit(a) if a.kind == AuditKind::Enhanced
        )),
        "enhancement must be recorded on the side channel",
    );
}
