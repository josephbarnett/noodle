# OpenAI Responses API streaming protocol

**Status:** Authoritative external knowledge. Captured 2026-05-10 from
OpenAI's public docs. Reference material for noodle's future
`OpenAiCodec` on the Responses-API path. Not a noodle design.

## How streaming is enabled

POST to `/v1/responses` with `stream: true`. The Responses API uses
**semantic events** over SSE — each event has a fixed schema, so
listeners subscribe to specific event types rather than parse a
generic stream.

This is **different** from the legacy `/v1/chat/completions` streaming
format (`data: {...}` deltas with `choices[].delta.content`). The
Responses API is the forward path; the Chat Completions streaming
format is still in wide use and codecs will likely need to handle
both.

## Event types

A non-exhaustive list of streaming events (from the OpenAI docs):

```
| Lifecycle                            | Notes                          |
|--------------------------------------|--------------------------------|
| response.created                     | Run started                    |
| response.in_progress                 | Active                         |
| response.completed                   | Run finished successfully      |
| response.failed                      | Run finished with an error     |

| Output items                         |                                |
|--------------------------------------|--------------------------------|
| response.output_item.added           | New output item opened         |
| response.output_item.done            | Output item closed             |
| response.content_part.added          | Sub-part inside an output item |
| response.content_part.done           |                                |

| Text                                 |                                |
|--------------------------------------|--------------------------------|
| response.output_text.delta           | Streaming text chunk           |
| response.output_text.annotation.added| Inline annotation              |
| response.text.done                   | Text complete                  |

| Refusals                             |                                |
|--------------------------------------|--------------------------------|
| response.refusal.delta               | Streaming refusal text         |
| response.refusal.done                |                                |

| Function calls                       |                                |
|--------------------------------------|--------------------------------|
| response.function_call_arguments.delta | Streaming arguments JSON     |
| response.function_call_arguments.done  |                              |

| File search                          |                                |
|--------------------------------------|--------------------------------|
| response.file_search_call.in_progress|                                |
| response.file_search_call.searching  |                                |
| response.file_search_call.completed  |                                |

| Code interpreter                     |                                |
|--------------------------------------|--------------------------------|
| response.code_interpreter.in_progress|                                |
| response.code_interpreter_call.code.delta |                           |
| response.code_interpreter_call.code.done  |                           |
| response.code_interpreter_call.interpreting |                          |
| response.code_interpreter_call.completed  |                            |

| Errors                               |                                |
|--------------------------------------|--------------------------------|
| error                                | Any error during streaming     |
```

(Refer to OpenAI's API reference for the complete and current list;
they may add event types under their versioning policy.)

## Common events when streaming text

The minimum a text-streaming consumer needs to handle:

- `response.created` — run started; capture the response id.
- `response.output_text.delta` — append chunk to running text.
- `response.completed` — run finished; finalize.
- `error` — terminal failure.

## Comparison with Anthropic SSE

| Aspect | Anthropic | OpenAI Responses |
|--------|-----------|------------------|
| Event naming | `event: message_start` + `data: {type: "message_start"}` (duplicated) | Semantic event names (`response.output_text.delta`) |
| Per-block lifecycle | `content_block_start` / `delta` / `stop`, keyed by `index` | `response.output_item.*` / `response.content_part.*`, also indexed |
| Text streaming | `text_delta` | `response.output_text.delta` |
| Tool-call args | `input_json_delta` (partial JSON string, parse on stop) | `response.function_call_arguments.delta` (partial JSON string, similar pattern) |
| Thinking / reasoning | `thinking_delta`, `signature_delta` | No direct equivalent on the Responses API surface |
| Terminator | `message_stop` | `response.completed` |
| Stop signal | `stop_reason` in `message_delta` | `response.completed` event itself |

Codecs that span both providers should share the per-index
accumulation pattern (string deltas → finalize at `*_stop` / `*.done`)
but treat the event names as distinct namespaces.

## Persistent-connection variant

OpenAI also offers a WebSocket mode (`previous_response_id`) for
incremental inputs. Not relevant to the proxy's MITM path today
since the wire is still HTTP for the request — but noted for
completeness; we may need a separate codec path if WS-mode usage
appears in captures.

## What this means for noodle

- The future `OpenAiCodec` (Responses-API path) should:
  - Treat event names as the discriminator (no need to peek inside
    `data` to know the type).
  - Per-index accumulation for `response.output_text.delta` and
    `response.function_call_arguments.delta`.
  - Recognize `response.completed` / `response.failed` as the
    terminator.
- The **Chat Completions streaming format** is a separate code path
  with `data: {...choices: [{delta: {content: "..."}}]}` plus a
  `data: [DONE]` terminator. Likely needs its own decoder.
- Moderation note from OpenAI: streaming can make content-policy
  enforcement harder because partials are visible before the final
  response. Worth keeping in mind for any redaction/filter we run on
  streamed assistant output.
