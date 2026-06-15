# Codec perf bench — legacy vs. layered

Runs the [`crates/noodle-adapters/benches/codec_paths.rs`](../../crates/noodle-adapters/benches/codec_paths.rs)
benchmark. Numbers below are **verbatim criterion output** from the
machine listed in §1 — never paraphrase the values in prose.

This is the A.7 slice of
[`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md).
The bench is the precondition the cadence pins before A.8 deletes
the legacy codec path.

---

## 1. Methodology

### Corpus

A realistic single-turn Anthropic SSE response body synthesized
in-process by `anthropic_corpus()`. Shape:

- `message_start` (1)
- `content_block_start` (text) + 30 `content_block_delta`
  (text_delta) + `content_block_stop` (32)
- `content_block_start` (tool_use) + 2 `content_block_delta`
  (input_json_delta) + `content_block_stop` (4)
- `message_delta` (stop_reason=tool_use + usage)
- `message_stop`

**Corpus size:** 4,853 bytes (~5 KiB).

### Paths under test

- **legacy** — inline SSE framing (split on `\n\n`) +
  [`AnthropicStreamingDecoder::decode_frame`] per frame. Mirrors
  what the proxy does on the legacy path.
- **layered** — [`SseFrameCodecInstance::decode`] →
  [`LayeredAnthropicCodecInstance::decode`] per emitted
  `BodyFrameEvent`. Production layered path with A.1 wiring
  (tool_use accumulation + TurnUsage extraction).

Both drive bare-method `decode` (no side channel) so the bench
measures the same code path the production engine drives (the
audit-emitting variants delegate to the bare ones via
default-impl per ADR 042 §2.1; on this corpus no overflow path
fires).

### Event-count delta

`legacy_events=41 layered_events=42`. The delta is intentional:
the layered path emits `NormalizedEvent::ToolCall` on each
`tool_use` block close (ADR 041 §2.1). The legacy path does not
synthesize tool-call events. The throughput numbers are still
apples-to-apples — both consume the same bytes — but the layered
path is doing strictly more work per byte.

### Machine

- **CPU:** Apple M1
- **Kernel:** Darwin 25.5.0 (arm64, T8103)
- **rustc:** 1.95.0 (59807616e 2026-04-14)
- **Profile:** criterion default (3s warmup, 100 samples,
  5s estimated measurement window)
- **Run date:** 2026-05-31

Other machines will produce other numbers. Re-running the bench
on the target hardware is the contract — never copy these
numbers as the answer for a different environment.

## 2. Results

```
corpus bytes=4853 legacy_events=41 layered_events=42 (delta is ToolCall events A.1.a added — ADR 041 §2.1)

anthropic_response_body/legacy
                        time:   [18.821 µs 18.852 µs 18.887 µs]
                        thrpt:  [245.05 MiB/s 245.50 MiB/s 245.90 MiB/s]
Found 2 outliers among 100 measurements (2.00%)
  1 (1.00%) high mild
  1 (1.00%) high severe

anthropic_response_body/layered
                        time:   [27.370 µs 27.492 µs 27.641 µs]
                        thrpt:  [167.44 MiB/s 168.34 MiB/s 169.10 MiB/s]
Found 33 outliers among 100 measurements (33.00%)
  22 (22.00%) low mild
  7 (7.00%) high mild
  4 (4.00%) high severe
```

## 3. Interpretation

The layered path is **slower than the legacy path on this
corpus** — mean throughput 168.34 MiB/s vs. 245.50 MiB/s,
mean per-iteration time 27.492 µs vs. 18.852 µs.

That ratio (~1.46× slower, layered/legacy mean times) is the
honest cost of:

- emitting `ToolCall` events on `tool_use` blocks (A.1.a);
- buffering tool_use `input_json_delta` chunks across SSE
  frames (per-block `HashMap<u32, ToolUseAcc>` accumulator);
- extracting cumulative usage from `message_delta` and stamping
  on `TurnEnd` (A.1.b);
- the layered codec's `Box<dyn CodecInstance>` virtual dispatch
  per `BodyFrameEvent`;
- one additional intermediate `BodyFrameEvent` per frame
  (allocation + clone).

Whether this is acceptable for the A.8 delete-legacy decision is
not a measurement question — it's a value question this doc
surfaces, not answers.

**33% outliers on the layered run** vs 2% on legacy is also worth
noting. The layered side allocates more per iteration (per-frame
`BodyFrameEvent`s, per-block `ToolUseAcc`); the allocator is
likely the source of the higher variance. A follow-up could pin
this with `dhat` or `heaptrack`.

## 4. Reproducing

```bash
cargo bench --bench codec_paths -p noodle-adapters
```

Output streams to stdout in the format above; the bench is
self-checked (`assert!`-free, prints the event-count delta).

## 5. What this bench does NOT do

- Does not benchmark request-side codecs.
- Does not exercise the audit-emitting variants
  (`decode_with_audit`); those default-delegate on this corpus
  because no overflow path fires.
- Does not include the engine's transform chain, side-channel
  drain, sink writes, or any production-shape wiring.
- Does not measure end-to-end proxy throughput. The proxy adds
  rama, TLS, hyper, network I/O — those are a different
  benchmark.

A.8 (delete legacy) is a separate decision; this bench informs
it but does not gate it.
