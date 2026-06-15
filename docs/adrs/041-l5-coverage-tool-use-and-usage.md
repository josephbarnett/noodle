# ADR 041 — L5 coverage: `tool_use` accumulation and usage on `TurnEnd`

**Status:** current.
**Audience:** Engineers extending the L5 (semantic) layer of the
layered codec stack (ADR 015) — Anthropic today; OpenAI, Bedrock
and others when their codecs land.
**Related:** ADR 015 (layered codec stack — the per-layer
mechanism), ADR 017 (provenance + multi-block fidelity),
ADR 018 (per-domain request codecs — the request-side analogue),
ADR 023 (round-trip records — where usage lands after extraction),
ADR 029 (`noodle-domain` vocabulary — `usage::TokenUsage` is the
typed downstream shape).

---

## 1. Context

The L5 (semantic) layer normalises vendor SSE / chunked events
into a uniform [`NormalizedEvent`][crates/noodle-core/src/event.rs]
stream the Resolver and downstream sinks consume. Today the
[`LayeredAnthropicCodec`][crates/noodle-adapters/src/provider/anthropic_layered.rs]
covers `message_start` → `TurnStart`,
`content_block_delta(text_delta)` → `Token`, `message_delta` →
`TurnEnd`. Two gaps remain that block cost attribution and
tool-execution fidelity on the layered path:

- **`tool_use` content blocks are emitted as `Metadata`.** The
  codec recognises `input_json_delta` chunks but does not
  accumulate them into structured `NormalizedEvent::ToolCall` —
  so the Resolver cannot see which tools were invoked or with
  what arguments without re-parsing the raw bytes.
- **Token usage is not extracted.** `message_delta.usage` and
  the terminal `message_stop` carry cumulative
  `input_tokens` / `output_tokens` / `cache_creation_input_tokens` /
  `cache_read_input_tokens`; today these land nowhere typed. The
  attribution ledger therefore has actors and tools but no
  dollars.

The `NormalizedEvent::ToolCall` variant already exists with the
right field shape (`call_id`, `name`, `args_json`, `index`,
`source`); the gap is the codec's accumulation behaviour, not the
type. This ADR pins:

1. how the accumulation works (and stays bounded),
2. where usage lives on the `NormalizedEvent` surface, and
3. how OpenAI parity will shape into the same surface when its
   L5 codec lands.

## 2. Decisions

### 2.1 `tool_use` accumulation — per-block buffer keyed on content-block index

A `tool_use` content block on the Anthropic SSE stream emits
three event categories the codec must thread:

| Event | Carries | Codec action |
|---|---|---|
| `content_block_start { type: "tool_use", id, name, input: {} }` | `call_id` (= `id`), `name` | Open a fresh accumulator entry at the block's `index`. |
| `content_block_delta { delta: { type: "input_json_delta", partial_json } }` | A partial JSON fragment for the tool args | Append `partial_json` to the entry at `index`. |
| `content_block_stop { index }` | (terminator) | Close the entry: emit `NormalizedEvent::ToolCall { call_id, name, args_json: accumulated, index, source }` and drop the buffer. |

**`args_json` is the accumulated string, not a parsed `Value`.**
v1 emits the raw concatenated JSON. Parsing-at-stop is deferred —
downstream consumers (Resolver, telemetry mapper) can parse if
they need the structured shape; the codec stays in the L5 lane
of "normalise events," not "validate payloads."

**Bounded accumulation:** the per-block buffer is capped at
**256 KiB**. On overflow the codec increments a counter
(`LayeredAnthropicCodecInstance::tool_use_overflows()`), logs a
`tracing::warn!` with the offending `index`, drops the buffer, and
continues streaming. No `ToolCall` is emitted for the overflowing
block. The 256 KiB number is a **defensive default, not a
measurement** — tighten or loosen once we have a tool-input size
distribution from production captures. Per-block is intentionally a
small fraction of the per-stream SSE cap (A.4: 4 MiB) because a
single round-trip can hold many concurrent `tool_use` blocks.

**Audit emission on overflow:** when the engine drives the codec
through `CodecInstance::decode_with_audit` (the path ADR 042 §2.1
pins for engine-driven invocations), the codec emits a single
`AuditEvent { kind: Errored, layer: VendorSemantics, transform:
"anthropic", detail: { reason: "tool_use_accumulator_overflow",
index, cap, overflow_total } }` via `SideChannelTx::emit_errored`.
Bare `decode` callers (tests, isolated round-trip checks) still
log via `tracing::warn!` and increment the counter — same shape
A.4 set for the SSE-parser overflow.

**Multiple concurrent `tool_use` blocks:** Anthropic's SSE
multiplexes them by `index`; the accumulator is a
`HashMap<u32, ToolUseAcc>` keyed on `index`. No ordering
assumption beyond "the `index` is stable within a `message_start`
/ `message_stop` envelope."

**Provenance (ADR 017):** the emitted `ToolCall.source` is
`EventSource::Upstream(ProviderChunk)` carrying the **final**
`content_block_stop` frame for that index — sufficient for
verbatim replay because the accumulated text isn't a frame the
upstream sent on its own (it's a synthetic projection). Encoders
that don't mutate the call can therefore drop back to byte-faithful
replay; encoders that mutate it re-serialize per ADR 017 §2.

### 2.2 Usage — fields on `TurnEnd`, not a new variant

[`NormalizedEvent::TurnEnd`][crates/noodle-core/src/event.rs:238]
gains an optional `usage` field of type `Option<TurnUsage>`:

```rust
pub enum NormalizedEvent {
    // ... existing variants ...
    TurnEnd {
        round_trip_id: RoundTripId,
        finish: FinishReason,
        /// Token usage extracted from the terminal envelope event
        /// (Anthropic: `message_delta.usage` accumulated through
        /// `message_stop`). None when the vendor stream did not
        /// carry usage on this turn.
        usage: Option<TurnUsage>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens that hit the prompt-cache read path. Absent
    /// pre-cache-protocol or when the vendor does not surface it.
    pub cache_read: Option<u64>,
    /// Tokens written into the prompt cache this turn. Same
    /// nullability rule as `cache_read`.
    pub cache_write: Option<u64>,
}
```

**Why fields-on-`TurnEnd`, not a separate `Usage` variant:**

| Concern | Fields-on-`TurnEnd` | Separate `Usage` variant |
|---|---|---|
| Emission cardinality | One per turn (matches both Anthropic and OpenAI Responses — both terminal) | Same in practice; redundant event |
| Downstream consumers | Read at turn-end time anyway (Resolver, RoundTripSink) | Need to correlate two events back to one turn |
| Future incremental-usage providers | Can extend `TurnUsage` with deltas if needed; or add a sibling `IncrementalUsage` variant later | Already the variant shape; but no current provider needs it |
| Schema additivity | `usage` is `Option<…>` — old captures + decoders unaffected | Net-new variant on the enum; pattern-match exhaustiveness churn everywhere |
| Codec complexity | Buffer cumulative value, stamp at `message_stop` | Same buffering plus a separate emission point |

Fields win on every axis given no current provider emits
incremental token usage. If a future provider does, the path
forward is a sibling `IncrementalUsage` variant feeding the same
`TurnUsage` shape — not a retrofit of this decision.

**Accumulation timing:** the codec buffers the *latest* `usage`
block from each `message_delta` and stamps the cumulative value
on the `TurnEnd` emitted at `message_stop`. Pre-stop `message_delta`
events do **not** emit a `TurnEnd` — they merely refresh the
buffered usage. The Anthropic protocol guarantees the
final `message_delta` carries the cumulative final value.

### 2.3 OpenAI Responses parity — translation table for the L5 codec when it ships

The `LayeredOpenAiCodec` (backlog item 20, parked) targets the
same `NormalizedEvent` surface. Translation table the codec will
honour:

| OpenAI event | Codec action |
|---|---|
| `response.created` | `TurnStart` |
| `response.output_text.delta` | `Token` |
| `response.output_item.added { type: "function_call", id, name }` | Open accumulator at the item's position |
| `response.function_call_arguments.delta { item_id, delta }` | Append to the matching accumulator |
| `response.output_item.done { item: { type: "function_call", … } }` | Close + emit `ToolCall { call_id, name, args_json, index, source }` |
| `response.completed { usage: { input_tokens, output_tokens, … } }` | Emit `TurnEnd { …, usage: Some(TurnUsage { … }) }` |

The mapping is intentionally pinned here in ADR 041 — when the
OpenAI codec lands it should not need to re-litigate. Bedrock,
Google, and other future codecs follow the same
`(start, delta, stop, completed)` shape; their per-vendor frame
names are codec-level details below this contract.

## 2.4 Applicability to the plugin topology

The `tool_use` accumulator (§2.1) and the `TurnUsage` extraction
(§2.2) are pure logic. They are part of `LayeredAnthropicCodec` in
`noodle-adapters::provider::anthropic_layered`, which the
`noodle-detect` facade re-exports for plugins
(ADR 039 §2.3). A plugin host that runs an Anthropic round trip
through `detect()` receives the same `ToolCall` events and
`TurnUsage`-stamped `TurnEnd` events as the proxy host does. No
plugin-specific shim required.

Vendor codecs that plugin authors write for other providers follow
the same translation table (§2.3) — start, delta-accumulator,
stop, completed — and stamp `TurnUsage` on the terminal `TurnEnd`
event regardless of host.

## 3. Patterns applied

- **Accumulator** — per-block `input_json_delta` buffers; cumulative
  `usage` buffer. Both bounded.
- **Adapter** — vendor codecs translate vendor-specific event names
  into the uniform `NormalizedEvent` surface.
- **Memento** (provenance, ADR 017) — `ToolCall.source` and the
  re-stamped `TurnEnd` carry frame-level identity for byte-faithful
  replay on unmutated rows.

## 4. Open questions

- **Bedrock parity** — Bedrock's tool-use shape mirrors Anthropic's
  closely (it *is* an Anthropic-shaped channel); when its codec
  ships it will likely consume this ADR's contract directly. No
  decision needed today.
- **`server_tier`, `service_tier`, cache TTLs** — these are
  attribution-relevant but not L5-codec data; they ride on
  `TapUsage` (proxy-level capture, S20) and on `ai-telemetry`
  rows downstream. Not pulled into `TurnUsage`; the codec's job is
  *content semantics*, not *billing metadata*.

## 5. Out of scope

- Parsing `args_json` into structured types — downstream concern.
- Cost-attribution math (tokens × rate-card) — embellishment plane
  (ADR 031 / 022).
- Web-search / code-interpreter / file-search server-tool events
  — preserved as opaque `Metadata` per ADR 009; not modelled here.
- Streaming-incremental usage for hypothetical future providers —
  see §2.2 sibling-variant note.

## 6. Acceptance signals

This ADR is "honoured" when:

1. `LayeredAnthropicCodec` emits `ToolCall` events on every
   `tool_use` block close, with `call_id`, `name`, `args_json`,
   and `index` populated against recorded captures
   (`captures/api/`).
2. `LayeredAnthropicCodec` emits a single `TurnEnd { usage:
   Some(TurnUsage{…}) }` per turn, with cumulative token counts
   matching the final `message_delta` of the turn.
3. The cap (256 KiB per-block accumulation) is enforced and
   exercises a unit test that drops the buffer + emits the
   `AuditKind::Errored` audit on overflow.
4. Schema additivity holds: existing tests against `NormalizedEvent`
   shapes that pre-date this ADR keep passing (the new `usage`
   field defaults to `None`).
5. The OpenAI translation table in §2.3 is pinned for the codec
   author whenever item 20 reopens.
