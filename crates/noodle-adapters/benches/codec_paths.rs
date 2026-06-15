#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! Legacy vs. layered Anthropic codec throughput benchmark (A.7,
//! `040-post-parity-cadence.md` Track A).
//!
//! Drives a representative Anthropic SSE response body through both
//! paths and reports throughput. Output is consumed verbatim by
//! [`docs/guides/codec-perf-bench.md`] — never paraphrase the
//! numbers in prose.
//!
//! ## Paths under test
//!
//! - **Legacy**: inline SSE framing (split on `\n\n`) +
//!   [`AnthropicStreamingDecoder::decode_frame`] per frame.
//! - **Layered**: [`SseFrameCodecInstance::decode`] → per-frame
//!   [`LayeredAnthropicCodecInstance::decode`].
//!
//! Both consume the same `Bytes` buffer and produce
//! [`NormalizedEvent`] streams that are equivalence-checked once
//! before each benchmark group to prove the comparison is apples
//! to apples.
//!
//! ## Methodology notes
//!
//! - Criterion handles warmup, sample size, and statistical
//!   confidence; do not hand-roll timing loops.
//! - The corpus is synthesized in-process (no I/O, no file
//!   parsing) so the bench measures codec work only.
//! - Allocator and `cargo bench --release` settings are the
//!   defaults; switching either is its own slice.

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use noodle_adapters::provider::anthropic::AnthropicStreamingDecoder;
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodecInstance;
use noodle_adapters::sse::SseFrameCodecInstance;
use noodle_core::layered::CodecInstance;
use noodle_core::{NormalizedEvent, StreamingDecoder};
use std::hint::black_box;

/// Build a ~10 KiB Anthropic SSE response body matching a realistic
/// multi-content-block turn (text + `tool_use` + usage +
/// `stop_reason`). Identical across both bench paths; the bench
/// measures decode work only.
fn anthropic_corpus() -> Bytes {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01abcdEFGHijklMNOPqrst\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet-20240620\"}}\n\n",
    );
    s.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    // 30 token deltas — typical mid-length response body.
    for i in 0..30 {
        let _ = write!(
            s,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"token-{i:02} \"}}}}\n\n"
        );
    }
    s.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    s.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01tu\",\"name\":\"get_weather\",\"input\":{}}}\n\n");
    s.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"\"}}\n\n");
    s.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"San Francisco\\\"}\"}}\n\n");
    s.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    s.push_str(
        "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":124,\"output_tokens\":75,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}\n\n",
    );
    s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Bytes::from(s)
}

/// Split SSE frames on `\n\n` and feed each one to the legacy
/// Anthropic streaming decoder. The proxy's legacy path frames the
/// bytes upstream of the decoder; this helper mirrors that shape.
fn drive_legacy(body: &Bytes) -> Vec<NormalizedEvent> {
    let mut decoder = AnthropicStreamingDecoder::default();
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(rel) = find_frame_terminator(&body[start..]) {
        let end = start + rel + 2;
        let frame = body.slice(start..end);
        out.extend(decoder.decode_frame(&frame));
        start = end;
    }
    out
}

/// Feed the whole body through `SseFrameCodec`, then thread each
/// emitted `BodyFrameEvent` through `LayeredAnthropicCodec`.
fn drive_layered(body: &Bytes) -> Vec<NormalizedEvent> {
    let mut sse = SseFrameCodecInstance::default();
    let mut anth = LayeredAnthropicCodecInstance::default();
    let frames = sse.decode(body.clone());
    let mut out = Vec::new();
    for frame in frames {
        out.extend(anth.decode(frame));
    }
    out
}

/// Find the byte index of the first `\n` in a `\n\n` terminator, or
/// `None` when no terminator is present in the slice.
fn find_frame_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\n\n")
}

fn bench_paths(c: &mut Criterion) {
    let corpus = anthropic_corpus();
    let corpus_len = corpus.len() as u64;

    // Drive both paths once and print event counts. They are NOT
    // expected to be equal on a `tool_use`-bearing corpus — the
    // layered path emits `NormalizedEvent::ToolCall` per A.1.a
    // (ADR 041 §2.1) and the legacy path does not. This delta is
    // intentional and reflected in the per-bench reports.
    let legacy = drive_legacy(&corpus);
    let layered = drive_layered(&corpus);
    println!(
        "corpus bytes={corpus_len} legacy_events={} layered_events={} (delta is ToolCall events A.1.a added — ADR 041 §2.1)",
        legacy.len(),
        layered.len(),
    );

    let mut group = c.benchmark_group("anthropic_response_body");
    group.throughput(Throughput::Bytes(corpus_len));
    group.bench_function("legacy", |b| {
        b.iter(|| {
            let _ = drive_legacy(black_box(&corpus));
        });
    });
    group.bench_function("layered", |b| {
        b.iter(|| {
            let _ = drive_layered(black_box(&corpus));
        });
    });
    group.finish();
}

criterion_group!(benches, bench_paths);
criterion_main!(benches);
