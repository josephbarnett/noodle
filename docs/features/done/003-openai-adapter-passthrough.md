# 003 — OpenAI adapter, pass-through

## Value

A request to `api.openai.com` is detected as OpenAI traffic and routed
through the OpenAI adapter. The adapter's `decode`/`encode` round-trips
the response unchanged. We can prove the L5 contract works end-to-end
without yet doing anything interesting with the events.

This is the story that establishes the adapter pattern. Every subsequent
provider follows the precedent it sets.

## Acceptance criteria

- `crates/noodle-core` defines `LlmAdapter`, `NormalizedEvent`,
  `ProviderChunk`, `Session`, `SessionStore` traits and types. No rama
  dependency in this crate.
- `crates/noodle-adapters` ships an `openai::OpenAiAdapter` that:
  - Matches on host `api.openai.com` (configurable suffix list).
  - Implements `inject_directive` as a no-op for now.
  - Decodes SSE bodies into `NormalizedEvent` based on the OpenAI
    `data: {...}\n\n` framing and the `[DONE]` sentinel.
  - Re-encodes events back, byte-faithfully, by emitting the
    `ProviderChunk::raw` bytes.
- `LlmInspectionLayer` in `noodle-proxy` selects the matching adapter
  and wires `decode → identity → encode` (the `policy_filter` in
  between is a no-op for this story).
- A streaming completion through noodle's proxy yields the same bytes
  the agent would have seen without the proxy. Verified by capturing
  with and without and `diff`ing.
- Golden-file tests live under
  `crates/noodle-adapters/fixtures/openai/` and replay captured SSE
  through `decode → encode` to assert byte-faithful round-trip.

## Dependencies

- 002 (we need plaintext HTTPS to even see the bytes).

## Implementation notes

- Keep `NormalizedEvent::Token` carrying both `text` (decoded) and
  `raw: ProviderChunk` (the original `data: ...\n\n` bytes). The text
  field is for policy; the raw bytes are for re-encoding.
- For unknown event variants (anything that's not `[DONE]` or a
  `data:` line we recognize), emit `NormalizedEvent::Metadata(raw)`
  and let the encoder re-emit verbatim. Log at WARN with a sample.
- Don't build the policy abstraction yet — story 006 introduces it.
  Wire `LlmInspectionLayer` with a hardcoded identity filter for now.
- Tests in `noodle-adapters` should compile in seconds: avoid pulling
  rama into this crate. Use plain `Stream`s and `Bytes`.
