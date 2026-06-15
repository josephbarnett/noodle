//! End-to-end: real Anthropic SSE bytes → `InspectionEngine`
//! (layered core) → `NormalizedEvent`s.
//!
//! This is the test that proves the layered architecture
//! actually processes real provider traffic — not fakes. It
//! wires the production `SseFrameCodec` (L4, story 028) and
//! `LayeredAnthropicCodec` (L5, story 029) into the
//! `InspectionEngine` (story 030) and feeds the exact SSE shape
//! Anthropic emits, including across arbitrary chunk boundaries.

use bytes::Bytes;
use http::{HeaderMap, Method};
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::sse::SseFrameCodec;
use noodle_core::event::NormalizedEvent;
use noodle_core::layered::{BodyFrameEvent, CodecProbe, CodecRegistry, InspectionEngine};

fn anthropic_probe<'a>(method: &'a Method, headers: &'a HeaderMap) -> CodecProbe<'a> {
    CodecProbe {
        host: "api.anthropic.com",
        path: "/v1/messages",
        method,
        request_headers: headers,
        response_status: Some(http::StatusCode::OK),
        response_content_type: Some("text/event-stream"),
    }
}

fn build_engine() -> InspectionEngine {
    InspectionEngine::builder()
        .l4_codecs(
            CodecRegistry::<Bytes, BodyFrameEvent>::builder()
                .with_codec(SseFrameCodec)
                .build(),
        )
        .l5_codecs(
            CodecRegistry::<BodyFrameEvent, NormalizedEvent>::builder()
                .with_codec(LayeredAnthropicCodec)
                .build(),
        )
        .build()
}

/// The exact SSE shape Anthropic emits for a short streamed
/// completion: `message_start` → `content_block_delta` ×2 →
/// `message_delta`(stop) → `message_stop`.
const ANTHROPIC_STREAM: &[u8] = b"event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01XYZ\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet-20241022\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

#[test]
fn engine_decodes_anthropic_stream_in_one_chunk() {
    let engine = build_engine();
    let method = Method::POST;
    let headers = HeaderMap::new();
    let mut flow = engine
        .open_response_flow(&anthropic_probe(&method, &headers))
        .expect("engine selects SSE + Anthropic codecs");

    let mut out = flow.push_bytes(Bytes::from_static(ANTHROPIC_STREAM));
    let tail = flow.finish();
    out.events.extend(tail.events);
    out.side_effects.extend(tail.side_effects);

    // Expected NormalizedEvent sequence:
    //   message_start  → TurnStart, Metadata
    //   content_block_delta "Hello"   → Token
    //   content_block_delta ", world" → Token
    //   message_delta(stop)           → TurnEnd, Metadata
    //   message_stop                  → Metadata
    assert!(matches!(
        out.events.first(),
        Some(NormalizedEvent::TurnStart { .. })
    ));

    let tokens: Vec<&str> = out
        .events
        .iter()
        .filter_map(|e| match e {
            NormalizedEvent::Token { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tokens, vec!["Hello", ", world"], "token stream reassembled");

    let turn_ends: Vec<_> = out
        .events
        .iter()
        .filter(|e| matches!(e, NormalizedEvent::TurnEnd { .. }))
        .collect();
    assert_eq!(turn_ends.len(), 1, "exactly one TurnEnd");

    // No transforms registered → no side effects.
    assert!(out.side_effects.is_empty());
}

#[test]
fn engine_reassembles_stream_split_across_arbitrary_chunks() {
    // Real wire arrives in arbitrary TCP segments. Split the
    // stream mid-frame, mid-JSON, mid-terminator and confirm the
    // engine still produces the identical NormalizedEvent
    // sequence — the L4 SSE codec's cross-chunk buffering must
    // hold under the engine.
    let engine = build_engine();
    let method = Method::POST;
    let headers = HeaderMap::new();
    let mut flow = engine
        .open_response_flow(&anthropic_probe(&method, &headers))
        .expect("flow opens");

    let mut events = Vec::new();
    // 7-byte chunks — guaranteed to split frames, JSON, and the
    // \n\n terminators.
    for chunk in ANTHROPIC_STREAM.chunks(7) {
        events.extend(flow.push_bytes(Bytes::copy_from_slice(chunk)).events);
    }
    events.extend(flow.finish().events);

    let tokens: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            NormalizedEvent::Token { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tokens,
        vec!["Hello", ", world"],
        "chunk-split stream yields the same tokens as one-shot",
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::TurnStart { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, NormalizedEvent::TurnEnd { .. }))
    );
}

#[test]
fn engine_declines_non_anthropic_host() {
    // The L5 vendor codec only matches *.anthropic.com. For an
    // OpenAI host the engine returns None — the caller passes
    // the bytes through untouched (transparent-unless-understood).
    let engine = build_engine();
    let method = Method::POST;
    let headers = HeaderMap::new();
    let probe = CodecProbe {
        host: "api.openai.com",
        path: "/v1/chat/completions",
        method: &method,
        request_headers: &headers,
        response_status: Some(http::StatusCode::OK),
        response_content_type: Some("text/event-stream"),
    };
    assert!(engine.open_response_flow(&probe).is_none());
}

#[test]
fn engine_declines_non_sse_content_type() {
    // L4 SseFrameCodec only matches text/event-stream. A JSON
    // response to an Anthropic host still declines (no L4
    // match), so a non-streaming call isn't mis-routed through
    // the SSE pipeline.
    let engine = build_engine();
    let method = Method::POST;
    let headers = HeaderMap::new();
    let probe = CodecProbe {
        host: "api.anthropic.com",
        path: "/v1/messages",
        method: &method,
        request_headers: &headers,
        response_status: Some(http::StatusCode::OK),
        response_content_type: Some("application/json"),
    };
    assert!(engine.open_response_flow(&probe).is_none());
}
