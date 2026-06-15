# Story 019 — Codec on the hot path (read-only)

**Value delivered:** The proxy decodes each SSE response on the
hot path through a provider-aware `ProviderCodec`, producing a
stream of typed `NormalizedEvent`s (`TurnStart`, `Token`,
`ToolCall`, `TurnEnd`, `Metadata`) that lands in a new file-backed
sink at `~/.noodle/events.jsonl`. Bytes to the client are
unchanged.

This is the foundation that **enables** the original story-017
promise of "filter / injector can act per-event rather than
buffer-then-rewrite," without itself rewriting bodies. Filter and
injector still operate as today on the buffered text path.
Per-event mutation is the follow-up story.

Unlocks:

- **Story 020** (OpenAI Responses codec) — a second `ProviderCodec`
  impl that drops straight in.
- **Future "filter on events"** — once `events.jsonl` is reliable,
  refactor the filter pipeline to consume `NormalizedEvent`s and
  re-encode, with confidence the codec preserves bytes faithfully.
- Downstream consumers (the viewer, external tools) gain a
  decoded-semantics stream alongside the existing wire/frame logs.

## Acceptance criteria

A user can:

1. Run `noodle` and drive any Anthropic SSE response through the
   proxy. Within ~1 second of the response completing,
   `~/.noodle/events.jsonl` contains one JSONL line per decoded
   `NormalizedEvent` in arrival order, each tagged with the parent
   `request_id`.
2. Inspect the events file:
   - `TurnStart` lines carry `turn_id` (from Anthropic's
     `message.id`) and `role: "assistant"`.
   - `Token` lines carry the decoded `text` (joined `text_delta`
     content; one event per `content_block_delta` frame).
   - `TurnEnd` lines carry the mapped `FinishReason`
     (`stop` / `length` / `tool_call` / etc.).
   - `Metadata` lines preserve frames the codec did not normalize
     (ping, content_block_start/stop, etc.).
3. Pass a non-Anthropic SSE response (or any response when no
   codec matches) and have `events.jsonl` show no entries for it.
4. Stop the proxy gracefully; in-flight events flush to disk
   before close.
5. Verify the client still sees the response body byte-for-byte
   identical — codec wiring is observation-only.

## Out of scope (deferred)

- **Per-event filter / injector mutation.** The codec produces
  events; the filter pipeline does NOT consume them yet. (Follow-up
  story.)
- **OpenAI Responses-API codec.** Story 020.
- **Body rewrite on the MITM path.** Currently filters run only
  on the plain-HTTP forward path; no change here.
- **`encode()` invocation.** We `decode` but never re-encode in
  this story; `encode` stays exercised only by codec unit tests.

## Implementation notes

### Trait extension in `noodle-core`

```rust
pub trait ProviderCodec: Send + Sync + 'static {
    // existing: name(), matches(), decode(BodyStream), encode(EventStream)

    /// Open a per-response streaming decoder. Returns `None` when
    /// this codec doesn't support streaming (callers fall back to
    /// buffer-then-`decode`). Default returns `None`.
    fn streaming_decoder(&self) -> Option<Box<dyn StreamingDecoder>> {
        None
    }
}

pub trait StreamingDecoder: Send + 'static {
    /// Feed one complete SSE event's raw bytes (already split on
    /// `\n\n` by the SseParser). Returns any NormalizedEvents
    /// emitted so far. Idempotent on caller side — the decoder
    /// owns state across calls.
    fn decode_frame(&mut self, raw_event: &Bytes) -> Vec<NormalizedEvent>;

    /// Called at end-of-stream. Codecs with trailing state (e.g.
    /// half-buffered tool-call JSON) flush it here.
    fn flush(&mut self) -> Vec<NormalizedEvent> {
        Vec::new()
    }
}
```

The existing `decode(BodyStream)` keeps working for buffer-once
consumers; the new path is opt-in.

### `AnthropicStreamingDecoder` in `noodle-adapters`

State: `{ turn_id: Option<TurnId> }` — set on `message_start`,
read on `message_delta` for `TurnEnd`. The existing
`parse_sse_buffered` is refactored to expose a per-event
`decode_one_event(raw_event_bytes, &mut state)`; the streaming
decoder calls it once per frame.

### `EventSink` port + `EventLogJsonl` adapter

```rust
// noodle-core
pub trait EventSink: Send + Sync + 'static {
    fn record_event(&self, request_id: &str, event: NormalizedEvent);
}
```

`EventLogJsonl` (in `noodle-tap`) mirrors `FramesJsonlLog`:
- spawned via `EventLogJsonl::spawn(path, capacity)`
- non-blocking `record_event` (atomic-load enabled flag + try_send)
- writer task + bounded mpsc + drop-on-full counter
- graceful `shutdown()` drains in-flight events

JSONL line shape:

```jsonl
{"request_id":"nl-7","ts_unix_ms":1778544000123,"event":"turn_start","turn_id":"msg_01","role":"assistant"}
{"request_id":"nl-7","ts_unix_ms":1778544000310,"event":"token","text":"Hello"}
{"request_id":"nl-7","ts_unix_ms":1778544001500,"event":"tool_call","call_id":"toolu_01","name":"Bash","args_json":"{...}"}
{"request_id":"nl-7","ts_unix_ms":1778544001700,"event":"turn_end","turn_id":"msg_01","finish":"stop"}
{"request_id":"nl-7","ts_unix_ms":1778544000150,"event":"metadata","bytes_len":52}
```

Metadata frames record their byte-length but not body — the
already-existing `frames.jsonl` carries the full content; this
file is for *typed semantics*.

### `WireLogLayer` plumbing

`ProxyConfig` gains:

- `codecs: Arc<dyn CodecRegistry>` — default `NoOpCodecRegistry`.
- `events: Option<Arc<dyn EventSink>>` — `None` skips the entire
  decode path (zero overhead).

`WireLogLayer::with_codec(wire, frames, events, codecs)` is the
production constructor. The TeeBody already has SSE parsing; the
streaming decoder threads alongside `SseState`. For every parsed
SSE event, both the `FrameEvent` AND a NormalizedEvent
side-channel fire.

The codec is selected once per response (cheap), using a probe
built from the inbound request parts.

### Default wiring

`tap_setup::install` returns `(ProxyConfig, TapJsonlLog,
FramesJsonlLog, EventLogJsonl)`. Default `--default-features` build
registers an `OrderedCodecRegistry` containing
`Arc::new(AnthropicCodec::new())`. Operators with custom needs can
override `cfg.codecs` after `install`.

## Test plan

**Rust (unit / e2e):**

- `noodle-adapters::provider::anthropic::tests` — extend with
  streaming-decoder cases: `message_start` → `TurnStart`;
  `content_block_delta` (text) → `Token` carrying decoded text;
  `message_delta` (stop_reason) → `TurnEnd` with the prior turn_id;
  `ping` → `Metadata`; cross-frame turn_id state preserved.
- `noodle-tap::events_sink` — order-preserving JSONL writes,
  non-blocking under saturation, graceful drain.
- `noodle-proxy::tests::e2e_wirelog_events` — feed a 5-event
  Anthropic SSE response through `WireLogLayer::with_codec` with
  a capturing `EventSink`; assert one `NormalizedEvent` per
  expected boundary, request_ids tagged correctly, client still
  receives byte-faithful body.
- Pre-existing `cargo test --workspace` keeps passing.

**Build:**

- `cargo build --no-default-features` clean (codecs feature-gated
  same as tap).
- `cargo build --release` clean.
- `cargo clippy --workspace --all-targets` clean.

**Live (manual):**

- `make run-release` + Claude Code interaction → `events.jsonl`
  populates with typed events for each completion; client output
  identical to without the codec wired.

## Dependencies

- Story 017 (per-frame SSE sink) — present in main. We reuse its
  `SseParser` to deliver per-event raw bytes to the streaming
  decoder.
- Story 018 (viewer SSE mode) — unrelated; the viewer doesn't
  consume `events.jsonl` in this story.
