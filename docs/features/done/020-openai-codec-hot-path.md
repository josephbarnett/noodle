# Story 020 — OpenAI codec on the hot path

**Value delivered:** `events.jsonl` covers OpenAI streaming completions
the same way it covers Anthropic today. The proxy decodes OpenAI's
`chat.completions` SSE format (`data: {…}\n\n` framing,
`data: [DONE]` terminator) into typed `NormalizedEvent`s as frames
arrive — `TurnStart` / `Token` / `TurnEnd` / `Metadata` — and lands
them in the existing `~/.noodle/events.jsonl` sink.

This is a small story by design: Story 019 built the codec hot-path
plumbing; this just plugs OpenAI into it.

## Acceptance criteria

A user can:

1. Drive an OpenAI streaming completion through the proxy
   (`HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` set;
   `https://api.openai.com/v1/chat/completions` with
   `"stream": true`).
2. See `~/.noodle/events.jsonl` populate with one `turn_start` line,
   N `token` lines (one per non-empty `delta.content`), and one or
   two `turn_end` lines (one from `finish_reason`, one from
   `data: [DONE]` — matches the buffered codec's behaviour).
3. Drive a non-OpenAI / non-Anthropic SSE response and see no
   events emitted (codec registry returns `None`).
4. Confirm the response bytes the client receives are
   byte-faithful — codec is observation-only, same contract as
   Story 019.

## Out of scope (deferred)

- **OpenAI Responses API** (`/v1/responses`) — that's the newer
  format with typed `event:` envelopes, not the
  `chat.completions` SSE we already parse. Different codec, future
  story.
- **Tool-call streaming** — OpenAI emits tool-call args incrementally
  via `delta.tool_calls[].function.arguments`. Today the buffered
  codec preserves these as Metadata. A dedicated `ToolCall` event
  decoder is a follow-up.

## Implementation notes

### `OpenAiStreamingDecoder`

Mirrors `AnthropicStreamingDecoder`. State:

```rust
#[derive(Default)]
pub struct OpenAiStreamingDecoder {
    turn_id: Option<TurnId>,
    started: bool,
}
```

`turn_id` is minted from the FNV-1a hash of the **first** raw event
bytes (deterministic per-stream-start; we can't hash the whole body
in streaming mode without buffering, but a per-stream prefix is
distinct enough for the JSONL log).

`started` flags whether `TurnStart` has fired. We hold it back until
the first **actionable** chunk (non-empty `delta.content` or
`finish_reason`) so role-only role-only deltas (`delta:{"role":"assistant"}`)
don't trigger a premature `TurnStart`. Matches the buffered behaviour.

The per-event logic is factored out of `parse_sse_buffered` into a
free function `decode_one_event(raw, &mut state)` so both the
buffered and streaming paths share one source of truth — same
pattern Story 019 used for Anthropic.

### Registry wiring

`tap_setup::install` already constructs an `OrderedCodecRegistry`.
Story 020 just appends `OpenAiCodec` after `AnthropicCodec`:

```rust
cfg.codecs = Some(Arc::new(OrderedCodecRegistry::new(vec![
    Arc::new(AnthropicCodec::new()) as Arc<dyn ProviderCodec>,
    Arc::new(OpenAiCodec::new()) as Arc<dyn ProviderCodec>,
])));
```

Order doesn't matter semantically — the two codecs match on disjoint
hostnames — but stable alphabetical order makes reading the
registry list at a glance easier.

## Test plan

**Unit (in `noodle-adapters`):**
- `streaming_decode_matches_buffered_decode` — feed the same SSE
  bytes through `OpenAiStreamingDecoder` (one event at a time) and
  `parse_sse_buffered` (single call); assert identical output.
- `streaming_decoder_text_delta_emits_token` — single
  `delta.content` chunk → one `Token` carrying the content.
- `streaming_decoder_done_emits_turn_end` — `data: [DONE]` →
  `TurnEnd` with `FinishReason::Stop`.
- `streaming_decoder_finish_reason_emits_turn_end_with_mapped_reason`
  — `finish_reason: "length"` → `TurnEnd` carrying
  `FinishReason::Length`.
- `streaming_decoder_role_only_delta_is_metadata` — role-only
  deltas don't promote to `TurnStart` until an actionable chunk
  arrives.

**Integration (in `noodle-proxy`):**
- `openai_sse_decodes_to_typed_events_in_order` — feed a fixture
  matching real OpenAI streaming through `WireLogLayer::with_codec`
  with the registry produced by `tap_setup::install`; assert the
  event sequence + token texts.

## Dependencies

- Story 019 (codec on hot path) — present in main. We reuse
  `WireLogLayer::with_codec`, `EventSink`, `StreamingDecoder`,
  `EventsJsonlLog`.
