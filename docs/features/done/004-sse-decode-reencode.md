# 004 — SSE decode + re-encode (still pass-through)

## Value

A streaming SSE response is parsed event-by-event, normalized, and
re-emitted as it streams. Latency between an upstream event arriving
and the agent seeing the corresponding output is bounded by parse +
re-encode time (target: <1ms p99 per event on commodity hardware), not
by buffering the full response.

Functionally this is still pass-through, but architecturally we now
have a working `decode → policy → encode` pipeline operating on a live
stream. Story 006 only has to swap the no-op policy for a real one.

## Acceptance criteria

- An end-to-end test issues a streaming chat completion through noodle
  and asserts that:
  - The first event reaches the test client within 100ms of the
    upstream emitting it (test uses a controllable mock upstream).
  - Each event's bytes are identical to the upstream's bytes.
  - Connection close on either side propagates promptly to the other
    (no hung half-streams).
- A property test feeds randomized valid SSE chunkings into
  `decode → encode` and asserts byte-equality with the input.
- An adversarial test sends a single `data:` line split across many
  TCP-sized chunks; the re-emitted output is byte-equal to the input.

## Dependencies

- 003 (the adapter and inspection layer scaffolding).

## Implementation notes

- The SSE parser is rama's
  `rama_http_types::body::sse::EventStream` — don't write our own.
- The re-encoder for byte-faithful pass-through is *not* rama's SSE
  server module; that re-serializes from typed events. We need a
  trivial encoder that emits `ProviderChunk::raw` bytes directly. Keep
  it in `noodle-core`.
- Backpressure: the body sink must yield to the runtime between events
  so a slow client cannot OOM noodle. `Body::from_stream` over a
  `Stream` of `Bytes` handles this naturally; verify with a
  bounded-channel test.
- This is the right story to wire backpressure-aware metrics:
  events/sec, bytes/sec, decode latency histogram. Don't gold-plate;
  one tracing span per request and per-event counters is enough.
