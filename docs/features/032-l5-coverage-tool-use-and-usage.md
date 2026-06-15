# 032 — L5 coverage: `tool_use` → `ToolCall`, usage/billing fields
(backlog item 5)

**Status:** not started
**Depends on:** 031 (sink + Resolver + response-encode — usage
events need somewhere to land)
**Design refs:**
[`docs/adrs/009-anthropic-streaming-protocol.md`](../adrs/009-anthropic-streaming-protocol.md)
(authoritative Anthropic SSE event taxonomy: `content_block_*`,
`tool_use`, `message_delta.usage`),
[`docs/adrs/010-openai-responses-streaming.md`](../adrs/010-openai-responses-streaming.md)
(authoritative OpenAI Responses streaming events:
`response.output_item.added/done`, function_call_arguments deltas)
**Backlog row:** item 5 in
[`features/000-overview.md`](000-overview.md) — "L5 coverage:
`tool_use`→`ToolCall`, usage/billing fields, resolve Q5 envelope
shape."
**ADR:** **not yet written** — write one before code. Open
questions to pin: should `tool_use` `input_json` accumulate
into structured `ToolCall.arguments` (parse-at-stop) or remain
opaque bytes for v1; how usage fields map onto `NormalizedEvent`
(new variant, or fields on `TurnEnd`); how OpenAI's
`response.output_item.done` lifecycle aligns with Anthropic's
`content_block_stop`-with-final-input.

---

## 1. Value delivered

Without this story, the attribution loop can resolve *who* made
a request, but cannot attribute **cost** because token usage is
not extracted. After this story:

- `LayeredAnthropicCodec` emits a `NormalizedEvent::ToolCall`
  with accumulated `input_json` when a `tool_use` `content_block`
  closes. Tool calls become first-class attribution facts.
- `message_delta.usage` (cumulative input/output tokens, cache
  read/write hits) is surfaced as a typed usage event the
  Resolver can consume to attach cost to the `Resolved` record.
- The OpenAI Responses codec, when it ships, emits the same
  shapes from its semantic-event stream
  (`response.function_call_arguments.delta/done`,
  `response.completed.usage`) — keeping vendor codecs distinct
  but the `NormalizedEvent` surface uniform.

This is the data the attribution product needs to answer "what
did this cost?" — without it, the ledger has actors and tools
but no dollars.

## 2. Acceptance criteria

1. `NormalizedEvent::ToolCall { id, name, arguments, source }`
   is emitted by `LayeredAnthropicCodec` on every `tool_use`
   block close. `arguments` is accumulated from
   `input_json_delta` events (parse-at-stop per ADR 009).
2. `source: EventSource::Upstream(ProviderChunk)` carries the
   raw frames so encode can replay verbatim when unmutated
   (ADR 017 invariant).
3. Usage event shape decided in ADR + implemented:
   either `NormalizedEvent::Usage { input_tokens, output_tokens,
   cache_read, cache_write }` or fields-on-`TurnEnd`. Resolves
   the overview's "Q5 envelope shape" question.
4. Emitted on `message_delta.usage` (cumulative; the codec
   reports the final value at `message_stop`, not deltas).
5. `LayeredOpenAiCodec` (when it ships — backlog item 20) is
   reviewed against the same `NormalizedEvent` shapes; the
   codec is responsible for translating its semantic events
   into the unified surface.
6. End-to-end test against a recorded Anthropic capture
   (a tool-use turn): assert the `ToolCall` event has the
   right name + arguments, and a `Usage` event carries the
   expected token counts.
7. Sink integration: `Usage` and `ToolCall` events route
   through the side-effect mechanism if/when transforms emit
   them as hints (e.g. a `UsageCostDetector` mapping tokens to
   dollars). The decode-side emission itself is data on the
   `NormalizedEvent` stream, not a side-effect.

## 3. Abstractions introduced or refined

- `NormalizedEvent::ToolCall` — refined (currently sparse;
  fill in `id`, `name`, `arguments` per the codec's
  accumulation).
- New `NormalizedEvent::Usage` variant **or** new fields on
  `TurnEnd` — Q5, resolved in the ADR.
- Per-block accumulation buffers in `LayeredAnthropicCodec`
  for `input_json_delta` — uses ADR 016 `CacheAndRelease`
  primitive if available; otherwise inline with bounded policy.

## 4. Patterns applied

- **Accumulator** (ADR 016 `CacheAndRelease` family) for partial
  tool-arg JSON.
- **Memento** (provenance) — `ToolCall.source` retains raw
  chunks for byte-faithful encode.
- **Adapter** — vendor codecs map vendor-specific events to the
  unified `NormalizedEvent` surface.

## 5. Test plan

- Unit tests for `input_json_delta` accumulation across
  multi-chunk tool calls (parse-at-stop, partial-JSON
  handling).
- Unit tests for `message_delta.usage` extraction across the
  multiple `message_delta` events that may appear (final
  cumulative value).
- Replay tests against recorded captures
  (`captures/api/` — multi-turn, tool-use, extended-thinking).

## 6. PR scope

Likely two PRs:

- **032.a** — `ToolCall` accumulation + emission in
  `LayeredAnthropicCodec`.
- **032.b** — `Usage` shape (Q5 decision) + emission across
  Anthropic. OpenAI parity tracked under item 20 once it
  lands.

## 7. Out of scope

- OpenAI codec on the layered stack — backlog item 20 (parked).
- Web-search / code-interpreter / file-search server-side tool
  events — preserved as opaque metadata blocks per ADR 009;
  not parsed into structured shapes.
- Cost-attribution math (tokens × rate-card) — this lives
  downstream of `Resolver` in the embellishment plane
  (story 028, deferred).
