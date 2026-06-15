# 033 — `JsonChunk` `BodyFrame` variant for non-streaming
Anthropic responses (backlog item 6)

**Status:** not started
**Depends on:** done/026 (`Codec` + `Transform` traits)
**Design refs:**
[`docs/adrs/015-layered-codec-architecture.md`](../adrs/015-layered-codec-architecture.md)
§2 (L4 codec catalog — `JsonChunkCodec` listed alongside
`SseFrameCodec` for response-side single-JSON bodies),
[`docs/adrs/018-normalized-request-and-per-domain-codec-chain.md`](../adrs/018-normalized-request-and-per-domain-codec-chain.md)
§2.5 + §9 (correction: §2.5's "subsumed" claim covers the
request side only and §9 then went single-stage there; the
**response** side is still open and this story is what fills
it)
**Backlog row:** item 6 in
[`features/000-overview.md`](000-overview.md) — "`JsonChunk`
`BodyFrame` variant" (**response side only** per the row's
clarification).

---

## 1. Value delivered

Today the layered path can decode SSE streaming responses
(via `SseFrameCodec` → `LayeredAnthropicCodec`). It cannot
decode **non-streaming** responses — a single POST to
`api.anthropic.com/v1/messages` with `"stream": false` returns
one JSON object as the entire response body, with no SSE framing.
The layered path sees those bytes and falls through to
passthrough; the codec catalog has nothing that matches.

After this story, `JsonChunkCodec` ships as an L4 codec that
treats "the whole response body" as a single
`BodyFrameEvent::JsonChunk(Bytes)`. `LayeredAnthropicCodec` adds
a non-streaming `match`/`decode` path that consumes one
`JsonChunk` and emits the synthetic
`TurnStart → Token … → TurnEnd` sequence. Non-streaming usage
(OpenWhispr is the named consumer; CI/scripted callers more
broadly) becomes visible to the layered path. Attribution works
the same way it does for streaming — directive injected on
request, markers stripped on response, usage extracted from
`response.usage`.

## 2. Acceptance criteria

1. `BodyFrameEvent` variant `JsonChunk(Bytes)` added to the
   typed L4 envelope.
2. `JsonChunkCodec: Codec<Input = Bytes, Output = BodyFrameEvent>`
   in `noodle-adapters::body`:
   - Buffers bytes until `flush()` then emits a single
     `JsonChunk(complete_body_bytes)` event.
   - Round-trip faithful: `encode(JsonChunk(b))` returns `b`
     verbatim.
   - Empty-on-error per ADR 015 §16 if the buffered bytes are
     non-UTF-8 or exceed a configurable cap (default ~32 MiB —
     non-streaming response bodies are bounded).
3. `JsonChunkCodec::matches(probe)` returns true when:
   - The request was for a documented non-streaming endpoint
     **and** the response `content-type` is `application/json`
     (or absent with JSON heuristic), **and** the response is
     not chunked/SSE.
4. `LayeredAnthropicCodec` gains a `JsonChunk → NormalizedEvent`
   path that:
   - Parses the buffered body as an Anthropic Messages response.
   - Emits `TurnStart`, one or more `Token` events covering the
     concatenated `content[]` text blocks, any `ToolCall` from
     `content[].type=="tool_use"`, `TurnEnd` with stop_reason +
     usage. Provenance: every event carries `EventSource::Upstream`
     with the original chunk so encode replays the body verbatim
     when unmutated.
5. Mutated re-encode round-trip: when a transform mutates a
   token, the codec re-serialises the body from structured
   fields (per ADR 017 §2.2) so the marker-strip actually
   removes bytes from the response.
6. End-to-end: a recorded non-streaming Anthropic capture
   replays through the layered path; assertions on (a) the
   client-visible bytes (markers stripped, usage preserved), and
   (b) the `Resolved` record landing in the sink with the
   correct tool/team/usage.
7. Existing streaming path (SSE) is untouched — additive blast
   radius.

## 3. Abstractions introduced or refined

- `BodyFrameEvent::JsonChunk(Bytes)` — first non-SSE L4
  envelope variant.
- `JsonChunkCodec` — L4 codec for single-body responses;
  parallel to `SseFrameCodec`.
- `LayeredAnthropicCodec` non-streaming branch — same vendor
  semantics, different L4 framing input.

## 4. Patterns applied

- **Adapter** — `JsonChunkCodec` adapts "no framing" to the
  `BodyFrameEvent` shape.
- **Strategy** — codec selection at L4 by response
  `content-type` / streaming hint.
- **Decorator** — `LayeredAnthropicCodec` gracefully handles
  both L4 input shapes; the same vendor codec works for both.

## 5. Test plan

- Unit tests for `JsonChunkCodec` (buffering, flush, error
  paths, round-trip).
- Codec-selection tests: streaming response goes through
  `SseFrameCodec`; non-streaming response goes through
  `JsonChunkCodec`.
- Integration test against a recorded non-streaming Anthropic
  capture: full request/response round trip, with and without
  injection.

## 6. PR scope

Two PRs:

- **033.a** — `BodyFrameEvent::JsonChunk` variant +
  `JsonChunkCodec` + tests.
- **033.b** — `LayeredAnthropicCodec` non-streaming branch +
  e2e proof.

## 7. Out of scope

- Request-side `JsonChunk` — explicitly **not** applicable.
  The request direction is single-stage per ADR 018 §9; there
  is no L4 split on the request side and this story does not
  add one.
- Non-streaming OpenAI / Codex / other vendors — those land
  with each vendor codec's individual story.
- WebSocket and other persistent-connection shapes — story
  009 (open) + future codec work; `JsonChunk` does not cover
  them.
