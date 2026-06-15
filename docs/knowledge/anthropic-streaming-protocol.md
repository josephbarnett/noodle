# Anthropic streaming protocol (`/v1/messages` SSE)

**Status:** Authoritative external knowledge. Captured 2026-05-10 from
Anthropic's public API docs. Reference material for noodle's
`AnthropicCodec` and the viewer's `anthropic_sse.ts` parser. Not a
noodle design; this is what we observe on the wire.

## How streaming is enabled

Set `"stream": true` on a POST to `/v1/messages`. The HTTP response is
an SSE stream (`text/event-stream`); each server-sent event includes
a named event type and a JSON payload.

## Event-type lifecycle

Each round trip's response is an SSE stream with this fixed sequence:

1. `message_start` — initial `Message` envelope with `content: []`.
2. For each content block, in `index` order:
   - `content_block_start`
   - One or more `content_block_delta`
   - `content_block_stop`
3. `message_delta` — top-level updates (final `stop_reason`, cumulative `usage`).
4. `message_stop` — terminator.

`ping` events can appear anywhere; clients ignore them.
`error` events can appear at any point (e.g.
`{"type":"overloaded_error"}` for HTTP 529 in non-streaming).

> **The `usage` token counts in `message_delta` are cumulative.**

The doc also notes: new event types may be added under the versioning
policy — code should handle unknown event types gracefully.

## Content-block delta types

`content_block_delta` events carry a typed delta that updates the
block at the given `index`. The codec must accumulate per-index and
finalize at `content_block_stop`.

### `text_delta`
Appends `delta.text` to a `text` block's running text.
```sse
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ello frien"}}
```

### `input_json_delta`
Appends `delta.partial_json` (a **partial JSON string**, not a JSON
object) to a `tool_use` block's accumulated raw input. Parse the
accumulated string at `content_block_stop` to recover the final
`tool_use.input` object.
```sse
event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\": \"San Fra"}}
```
**Pacing:** current models emit one complete key/value at a time;
there may be delays between deltas while the model works. Future
models may emit finer granularity.

### `thinking_delta` (extended thinking)
Appends `delta.thinking` to a `thinking` block. Only present when
`thinking: {display: "summarized"}` (or default `"adaptive"`)
streaming is enabled.
```sse
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to find the GCD..."}}
```

### `signature_delta`
Emitted **once per thinking block**, just before its
`content_block_stop`. Carries an opaque signature used to verify the
thinking block's integrity. Codecs should preserve it on the block
but not surface it as user-visible text.
```sse
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM..."}}
```

When `thinking: {display: "omitted"}` is set, no `thinking_delta`
events fire — the block opens, receives `signature_delta`, closes.

## `stop_reason` values (in `message_delta`)

| Value | Meaning |
|-------|---------|
| `end_turn` | Model is done. Final round trip of this turn. |
| `tool_use` | Model wants to call a tool. Claude Code will run it and POST the next round trip. |
| `max_tokens` | Token limit hit. Turn ends; response is truncated. |
| `stop_sequence` | Hit a configured stop sequence. |

`stop_reason` is the **turn-boundary signal** — see
`docs/adrs/008-session-hierarchy.md`.

## Worked example — basic streaming

```sse
event: message_start
data: {"type":"message_start","message":{"id":"msg_1nZdL2...","type":"message","role":"assistant","content":[],"model":"claude-opus-4-7","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}
```

## Worked example — tool_use stream

```sse
event: message_start
data: {"type":"message_start","message":{...}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Okay, let's check the weather"}}
event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01...","name":"get_weather","input":{}}}
event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\":"}}
event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":" \"San Francisco, CA\"}"}}
event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":89}}
event: message_stop
data: {"type":"message_stop"}
```

## Worked example — extended thinking + final text

```sse
event: message_start
data: {"type":"message_start","message":{...}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I need to find the GCD..."}}
event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgIYAhIM..."}}
event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}
event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"The greatest common divisor of 1071 and 462 is **21**."}}
event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}
event: message_stop
data: {"type":"message_stop"}
```

## Server-side tool variants

Some content-block types reflect server-tool execution (e.g.
`server_tool_use`, `web_search_tool_result`). Treat them as opaque
metadata blocks — preserve verbatim for replay; do not attempt to
strip or transform their content.

## Error recovery

- **Claude 4.5 and earlier**: capture partial response, restart with
  the partial assistant message prepended to a new request.
- **Claude 4.6**: add a user message that asks the model to continue
  from where it left off.

> Tool use blocks and thinking blocks cannot be partially recovered.
> You can resume from the most recent text block.

## What this means for noodle

- **The codec must accumulate `text_delta` / `input_json_delta` /
  `thinking_delta` per `index`** and finalize at `content_block_stop`.
  `input_json_delta` requires JSON-parsing the accumulated string.
- **`signature_delta`** is preserved on the thinking block but not
  rendered as user-visible content.
- **`ping` and unknown events** are ignored — never error on them.
- **`stop_reason` in `message_delta`** is the canonical turn-boundary
  signal (see session-hierarchy doc).
- **`usage` is cumulative**; the final value is on the last
  `message_delta`.
- **Round-trip pairing**: every round trip has its own complete SSE
  lifecycle (`message_start` ... `message_stop`); they don't merge
  across round trips.
