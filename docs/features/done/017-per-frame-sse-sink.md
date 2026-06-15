# Story 017 — Per-frame SSE sink

**Value delivered:** The engine emits one JSONL line per SSE frame as
it arrives at the proxy, with monotonic arrival timestamps and the
parsed `event:` / `data:` fields. Today we only capture the full
SSE body once at end-of-stream — that's enough for the conversation
view but loses per-frame timing and per-event semantics. This story
adds a parallel capture stream that **preserves both**.

Unlocks:

- **Story 018** (SSE mode in the viewer) — visualize the stream
  frame-by-frame with relative arrival times.
- **Story 019** (codec on the hot path) — once the engine
  decomposes the response into events at capture time, filter /
  injector can act per-event rather than buffer-then-rewrite.
- Vendor-protocol debugging — comparing Anthropic's typed events
  against OpenAI's deltas frame-by-frame.

## Acceptance criteria

A user can:

1. Drive any SSE response through the proxy (e.g.
   `curl --no-buffer` with `stream: true`).
2. See a new JSONL file at `~/.noodle/frames.jsonl` (default; path
   configurable) where each line corresponds to one SSE frame, with:
   - `request_id` (`event_id` of the parent exchange — pairs to
     `tap.jsonl`'s line for the same response)
   - `frame_index` (0-based, monotonic within the response)
   - `ts_unix_ms` (when the frame's `\n\n` boundary was observed)
   - `event` (optional — the SSE `event:` field, e.g.
     `"message_start"`)
   - `data` (parsed JSON object when the `data:` payload is JSON;
     raw string otherwise)
3. Confirm that **non-streaming responses produce no frames file
   activity** (only `text/event-stream` content-types trigger
   frame emission).
4. Stop the proxy gracefully; the frames file flushes any in-flight
   buffered frames before close.

## Out of scope (deferred)

- A viewer "SSE mode" rendering. That's Story 018; it consumes the
  output produced here.
- Moving the existing tap.jsonl-format codec onto these frame events
  (Story 019: codec on the hot path).
- Backpressure/rate-limit policies beyond "drop-on-full counter,
  matching `TapJsonlLog`". If a real-world workload saturates the
  channel, that's a follow-up.
- Cross-frame correlation IDs (e.g., linking a `content_block_delta`
  to its `content_block_start`). The consumer can derive that from
  the frame stream.

## Implementation notes

### Where the parsing happens

`noodle-proxy::wirelog::WireLogLayer`'s `TeeBody` already sits on the
response body bytes as they stream through. It accumulates them and
emits a single `WireEvent::Response` on end-of-stream.

For per-frame, the same body needs to be parsed incrementally as
bytes flow:

- Maintain an SSE frame buffer (`Vec<u8>`).
- On every frame fed to `poll_frame`, append to the buffer **and**
  scan for `\n\n` (the SSE event-boundary).
- For each complete frame found:
  1. Parse the lines (split on `\n`, group by `event:` / `data:`).
  2. Stamp `ts_unix_ms = now()` — this is the *arrival* time, not
     the upstream emit time.
  3. Hand the parsed frame to the sink.
- Only do this work when `Content-Type` is `text/event-stream`.

### New types in `noodle-core`

```rust
// noodle-core/src/wire.rs (or a new sibling module)

pub struct FrameEvent {
    pub request_id: SmolStr,
    pub frame_index: u32,
    pub ts_unix_ms: u64,
    pub event: Option<SmolStr>,
    pub data: Bytes,   // raw bytes of the data lines, joined by '\n'
}

pub trait FrameSink: Send + Sync + 'static {
    /// Non-blocking. Implementations that need I/O must offload
    /// to a background task. Matches the contract of `WireSink`.
    fn record_frame(&self, event: FrameEvent);
}
```

The two traits stay separate by design — most sinks care about one
or the other, not both, and composition is cleaner via a fan-out
than via an enum-of-events trait.

### Pipeline plumbing in `noodle-proxy`

`WireLogLayer::new` gains an optional `frame_sink: Option<Arc<dyn FrameSink>>`.
When `None`, the TeeBody skips the SSE parse entirely (zero overhead
for production builds that don't tap). When `Some`, the SSE-aware
fast path engages.

A separate `TeeBody` variant — or a typestate on the existing one —
keeps the non-SSE path branch-free.

### New sink: `FramesJsonlLog` in `noodle-tap`

Mirrors `TapJsonlLog` exactly:
- `FramesJsonlLog::spawn(path, capacity)` spawns a writer task,
  returns a sink with an `Arc<AtomicBool>` enabled flag and a
  bounded `mpsc::Sender<Vec<u8>>` for serialized frame lines.
- `record_frame` serializes the line synchronously and `try_send`s.
- Drop-on-full counter exposed via `dropped_count()`.
- Graceful drain via `shutdown().await`.

Same writer task / channel architecture as `TapJsonlLog`; no
mid-task allocations on the hot path beyond serializing the line.

### Wiring in `tap_setup`

```rust
// crates/noodle-proxy/src/tap_setup/mod.rs

pub async fn install(
    mut cfg: ProxyConfig,
    tap_path: PathBuf,
    tap_capacity: usize,
) -> std::io::Result<(ProxyConfig, Arc<TapJsonlLog>, Arc<FramesJsonlLog>)>
```

Returns both sinks so `main.rs` can drain both on shutdown. The
existing `WireSink` composition unchanged; the frame sink threads
through to `WireLogLayer` separately.

Default frames path: `$HOME/.noodle/frames.jsonl`.

### File format

```jsonl
{"request_id":"nl-2","frame_index":0,"ts_unix_ms":1778430000123,"event":"message_start","data":{"type":"message_start","message":{"id":"msg_…"}}}
{"request_id":"nl-2","frame_index":1,"ts_unix_ms":1778430000125,"event":"content_block_start","data":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}
{"request_id":"nl-2","frame_index":2,"ts_unix_ms":1778430000310,"event":"content_block_delta","data":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}}
```

`data` is the parsed JSON object when `data:` is valid JSON; otherwise a string carrying the raw `data:` payload joined by `\n`.

## Test plan

**Rust** (`crates/noodle-tap/tests/frames_*.rs`):

- `frames_basic.rs`: feed a 5-event SSE stream into a mock pipeline,
  assert 5 JSONL lines with monotonic `frame_index` and ascending
  `ts_unix_ms`.
- `frames_non_blocking.rs`: hot-path `record_frame` returns in
  microseconds even under writer backpressure.
- `frames_shutdown.rs`: drain semantics — all in-flight frames
  flushed before close.
- `frames_non_sse_passthrough.rs`: non-SSE response → frames file
  receives no events (proxy unit test).

**Integration** (`crates/noodle-proxy/tests/`):

- e2e: real SSE response through `WireLogLayer` produces N frames
  in the expected order with sensible arrival timing.

**Live**:

- `make run-release` + Claude Code session + verify
  `~/.noodle/frames.jsonl` populates with parsed events;
  `tail -F` shows them arrive interactively.

## Dependencies

- Story 016 (parse-cache) — unrelated.
- Engine-side only; no viewer changes in this story.

## Why this is Story 017

The TAP debugger is currently visualizing the whole-response view
well. Per-frame timing is the next frontier — it unlocks both SSE
mode (a real-time waterfall of decoded events) and codec-on-hot-path
(filter/injector on `Token`, `ToolUse`, `TurnStart` events instead
of opaque bytes). Without this story, the viewer can't show frame
timings and the codec layer stays parked.
