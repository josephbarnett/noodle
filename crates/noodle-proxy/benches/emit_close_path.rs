//! `emit()` close-path benchmark — ADR 049 §9.1.
//!
//! Compares the **legacy** close-time work (four byte-scans of
//! `accumulated_in`: `extract_stop_reason`, `extract_tool_uses`,
//! `extract_last_usage`, `extract_last_usage_envelope`) against the
//! **engine** close-time work (helper calls on the already-decoded
//! `Vec<ContentBlock>` and `Vec<ParsedSseEvent>` produced by the
//! streaming inspection engine).
//!
//! ## Methodology
//!
//! - Criterion handles warmup, sample size, and statistical
//!   confidence; do not hand-roll timing loops.
//! - The corpus is synthesized in-process (no I/O, no file parsing)
//!   so the bench measures close-path work only — not SSE framing
//!   or content-block accumulation.
//! - For the `engine` group, the body is decoded ONCE outside the
//!   bench loop into typed structures. That mirrors production:
//!   the engine's accumulators have already finished by the time
//!   `emit()` reaches its close-time work; the close path consumes
//!   the typed lists, not the byte buffer.
//! - For the `legacy` group, the byte scanners and JSON parsers
//!   are invoked over `accumulated_in` exactly as the pre-PR-#128
//!   `emit()` did — same call order, same call surface.
//!
//! ## Apples-to-apples
//!
//! Both paths produce the same four observable values for the
//! same response body. The `engine ↔ byte-scan` parity tests in
//! `wirelog::engine_byte_scan_parity_tests` are the regression
//! guard; this bench measures the wall-clock difference.

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use noodle_adapters::provider::anthropic_content_blocks::{
    ContentBlock, ContentBlocksAccumulator, tool_uses_in,
};
use noodle_adapters::provider::anthropic_events::{
    EventsAccumulator, ParsedSseEvent, last_usage_value_in, stop_reason_in,
};
use noodle_proxy::sse::SseParser;
use noodle_proxy::wirelog::{
    extract_last_usage, extract_last_usage_envelope, extract_stop_reason, extract_tool_uses,
    parse_usage_envelope, parse_usage_value,
};
use std::hint::black_box;

/// Build a ~5 KiB Anthropic SSE response body matching a realistic
/// multi-block sub-agent-spawning turn: `message_start` (with
/// nested usage), one short text block, two `tool_use` blocks (the
/// shape that drives ADR 049 lineage), then `message_delta` with
/// rolling usage + envelope + `stop_reason=tool_use`, and
/// `message_stop`.
///
/// This is the same shape the `wirelog::engine_byte_scan_parity_tests::realistic_sse`
/// fixture uses — parity tests and bench share a corpus.
fn realistic_sse(text_deltas: usize) -> Bytes {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str(
        "event: message_start\n\
         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01ABC\",\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":1024,\"cache_read_input_tokens\":512}}}\n\n",
    );
    s.push_str(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    );
    for i in 0..text_deltas {
        let _ = write!(
            s,
            "event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"token-{i:03} \"}}}}\n\n",
        );
    }
    s.push_str(
        "event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    s.push_str(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_012Y8jeMfYYbNWTHPS1Nujbw\",\"name\":\"Agent\",\"input\":{}}}\n\n",
    );
    s.push_str(
        "event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    );
    s.push_str(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_AAA\",\"name\":\"Bash\",\"input\":{}}}\n\n",
    );
    s.push_str(
        "event: content_block_stop\n\
         data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    );
    s.push_str(
        "event: message_delta\n\
         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":256,\"cache_read_input_tokens\":1024,\"service_tier\":\"standard\",\"inference_geo\":\"us-east-1\"}}\n\n",
    );
    s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Bytes::from(s)
}

/// Decode the SSE body into the typed structures the engine path
/// consumes. Mirrors the wiring inside `TeeBody::poll_frame` —
/// each accumulator has its own `SseParser` (ADR 030 §2 + §3).
fn decode_via_engine(body: &Bytes) -> (Vec<ContentBlock>, Vec<ParsedSseEvent>) {
    let mut blocks_parser = SseParser::new();
    let mut blocks_acc = ContentBlocksAccumulator::new();
    for parsed in blocks_parser.feed(body) {
        blocks_acc.feed(&parsed.raw);
    }
    let mut events_parser = SseParser::new();
    let mut events_acc = EventsAccumulator::new();
    let first_byte = 1_000;
    for (i, parsed) in events_parser.feed(body).into_iter().enumerate() {
        events_acc.feed_event(&parsed.raw, first_byte, first_byte + i as u64);
    }
    (blocks_acc.finish(), events_acc.finish())
}

/// Run the four legacy byte scanners + JSON parsers — the close
/// path before PR #128. Mirrors the `emit()` call surface for the
/// marks closure + usage assembly when the engine path is inactive
/// (now reachable only on the non-SSE / non-anthropic fallback).
fn run_legacy(body: &[u8]) -> (Option<&'static str>, usize, bool, bool) {
    let stop = extract_stop_reason(body);
    let tool_uses = extract_tool_uses(body);
    let tokens = extract_last_usage(body);
    let (tier, geo) = extract_last_usage_envelope(body);
    // Force every result to be observable so the optimizer can't
    // eliminate any of the four scans. The return shape is opaque
    // on purpose — we only care that every byte-scan result is
    // consumed before the iteration ends.
    (
        stop.map(|_| "stop"),
        tool_uses.len(),
        tokens.is_some(),
        tier.is_some() && geo.is_some(),
    )
}

/// Run the engine-decoded path — the close path after PR #128.
/// `blocks` and `events` are already finished by the streaming
/// accumulators when `emit()` is called in production; the bench
/// passes them in by reference so we measure only the
/// consumption work (helper calls + JSON parsing of the usage
/// value), not the decode work.
fn run_engine(
    blocks: &[ContentBlock],
    events: &[ParsedSseEvent],
) -> (Option<&'static str>, usize, bool, bool) {
    let stop = stop_reason_in(events);
    let tool_uses: Vec<_> = tool_uses_in(blocks).collect();
    let usage_val = last_usage_value_in(events);
    let tokens = usage_val.and_then(parse_usage_value);
    let (tier, geo) = usage_val.map_or((None, None), parse_usage_envelope);
    (
        stop.map(|_| "stop"),
        tool_uses.len(),
        tokens.is_some(),
        tier.is_some() && geo.is_some(),
    )
}

fn bench_paths(c: &mut Criterion) {
    // Three corpus sizes to expose body-size sensitivity. Real
    // anthropic responses span single-KB (haiku title-gen,
    // tool_use-only spawns) up to tens-of-KB (long text turns).
    for &deltas in &[0usize, 30, 200] {
        let corpus = realistic_sse(deltas);
        let corpus_len = corpus.len() as u64;
        let (blocks, events) = decode_via_engine(&corpus);

        // Sanity: both paths agree on every observable. If a
        // future change drifts them, the bench shows the
        // divergence in the println output before the timed
        // groups run.
        let legacy_obs = run_legacy(&corpus);
        let engine_obs = run_engine(&blocks, &events);
        assert_eq!(
            legacy_obs, engine_obs,
            "legacy ↔ engine observables drift at deltas={deltas}",
        );
        println!(
            "deltas={deltas} corpus_bytes={corpus_len} blocks={} events={} observables={:?}",
            blocks.len(),
            events.len(),
            legacy_obs,
        );

        let group_name = format!("emit_close_path_deltas_{deltas:03}");
        let mut group = c.benchmark_group(&group_name);
        group.throughput(Throughput::Bytes(corpus_len));
        group.bench_function("legacy", |b| {
            b.iter(|| {
                let _ = run_legacy(black_box(&corpus));
            });
        });
        group.bench_function("engine", |b| {
            b.iter(|| {
                let _ = run_engine(black_box(&blocks), black_box(&events));
            });
        });
        group.finish();
    }
}

criterion_group!(benches, bench_paths);
criterion_main!(benches);
