# 007 — Anthropic adapter

## Value

A request to `api.anthropic.com` is routed through the Anthropic
adapter, which decodes the typed-event SSE format (`event:
message_start`, `content_block_delta`, `message_stop`) into the same
`NormalizedEvent` shape, runs the same policy, and re-encodes
faithfully. Adding a second provider validates the L5 abstraction.

If this story requires changes to `noodle-core` or `noodle-policy`,
the abstraction was wrong and we should pause to fix it before
continuing.

## Acceptance criteria

- `anthropic::AnthropicAdapter` matches `api.anthropic.com`.
- Anthropic's typed events are decoded:
  - `message_start` → `TurnStart`
  - `content_block_delta` (text) → `Token`
  - `tool_use` blocks → `ToolCall`
  - `message_stop` → `TurnEnd`
  - other events → `Metadata` with raw bytes preserved
- `inject_directive` correctly writes to Anthropic's `system` field
  (top-level, not in `messages` — Anthropic's API differs from
  OpenAI's here).
- The default tag policy from story 006, **unmodified**, redacts
  markers from Anthropic streams.
- Golden-file round-trip tests pass for captured Anthropic streams.
- An end-to-end test confirms the marker is redacted from a real
  Anthropic stream the same way as from an OpenAI stream.

## Dependencies

- 006 (the policy must exist for this story to be a real test of
  generality).

## Implementation notes

- Anthropic's `tool_use` is delivered as input-JSON-deltas under
  `content_block_delta`. The adapter accumulates these and emits one
  `ToolCall` event when the block closes — do not emit per-delta.
- Anthropic uses ALPN h2 by default; verify the egress connector
  honors `TargetHttpVersion::HTTP_2`.
- If story 006's marker-detection breaks under Anthropic tokenization
  (Claude's tokenizer differs from OpenAI's, so the same marker text
  can fragment differently), that's signal that the marker format
  needs revisiting — escalate, don't paper over.
