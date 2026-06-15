# 015 — Layered codec architecture

**Status:** current. The trait surface (`Codec` + `Transform`) and the
six-layer event stack defined here are the structural spec for the
inspection layer.
**Companion:** [`../diagrams/015-layered-codec-architecture.drawio`](../diagrams/015-layered-codec-architecture.drawio)

---

## 1. Premise — one rule

> **Every layer is a codec. Every inspector is a transform.**

Both are typed stream transducers of the shape `Stream<I> → Stream<O>`.
What changes from layer to layer is the event type `I/O`; what
changes from inspector to inspector is the *value* of `I = O`. The
inspection pipeline is a directed graph of those transducers,
composed from a small set of well-typed traits.

Three consequences fall out:

1. **HTTP version multiplicity disappears upward.** h1, h2, h3 are
   three implementations of "wire-framing codec" with the same
   output type. Inspectors above don't know which framing produced
   the bytes.
2. **DNS is a sibling, not a sidecar.** A DNS codec at L2 emits
   `DnsMessage` the same way an HTTP codec emits `HttpRequest`. Any
   `Transform<DnsMessage>` (e.g. strip `alpn=h3` from `HTTPS` RRs)
   is the structural twin of any `Transform<NormalizedEvent>` (e.g.
   strip `<noodle:*>` from Token text).
3. **The 005 three-role split (Detector / Injector /
   Filter) collapses to one trait** because all three are
   transformations on a typed event stream with a side-effect
   channel.

The rest of this document defines the layers, the two traits, the
pipelines, the side-channels, and walks one end-to-end use case —
LLM cost attribution — to make the abstractions concrete.

---

## 2. The codec stack

Six layers, top of the application to bottom of the wire. The
"event type" column is the **typed stream shape** that flows
between this layer and the next. Codecs at this layer transform
*between* the type at this row and the type at the row above.

| L | Layer | Event type at this altitude | Codec implementations | Where it lives |
|---|---|---|---|---|
| L5 | Vendor semantics | `NormalizedEvent` (`TurnStart` / `Token` / `ToolCall` / `TurnEnd` / `Metadata`) | `AnthropicCodec`, `OpenAiCodec`, future `CodexCodec`, future `DeclarativeCodec<Spec>` | `noodle-adapters` |
| L4 | Body framing | `BodyFrameEvent` (typed envelope around one wire frame) | `SseFrameCodec` (`event:`+`data:` lines), `OpenAiSseFrameCodec` (`data:` only + `[DONE]`), `JsonChunkCodec` (single JSON body — **response side only**; the request direction uses single-stage `Bytes → NormalizedRequest` per-domain codecs per ADR 018 §9), future `WsMessageCodec` | `noodle-adapters` |
| L3 | Application protocol | `HttpRequest` / `HttpResponse` (full, streaming body) — or `DnsMessage` on a parallel branch | (semantic shape; produced by L2 codecs, no separate transformation) | rama (HTTP) · `noodle-adapters` (DNS) |
| L2 | Wire framing | bytes (plaintext stream or datagrams) | `HttpH1Codec`, `HttpH2Codec` (rama); future `HttpH3Codec`; `DnsWireCodec` (ours); future `WsFrameCodec` | rama + `noodle-adapters` |
| L1 | TLS | bytes (ciphertext) | `TlsMitmRelay` with cached self-signed CA (rama-tls-boring) | rama |
| L0 | Transport | TCP/UDP datagrams | OS / rama | rama |

L0-L1-L2 belong to rama (we don't re-implement TLS or HTTP/2
framing). Our codec stack starts at L2's *output* — we consume the
typed events rama produces and add codecs upward.

DNS is unusual: it's a parallel L2 branch with a different output
type. Its L3-equivalent is `DnsMessage` directly; there's no
distinct "DNS semantics" layer because the wire format already
carries structured records.

### 2.1 Layer invariants

Each codec layer commits to two invariants:

1. **Round-trip faithfulness.** `encode(decode(bytes)) == bytes` for
   any input not mutated by transforms. Concrete consequence: a
   `Metadata(raw_chunk)` `NormalizedEvent` whose `raw` field is
   unchanged must re-emit the original bytes at L5 → L4 encode.
2. **Per-flow statefulness, no cross-flow state.** A `CodecInstance`
   owns whatever state it needs for one flow (partial JSON, current
   `turn_id`, half-parsed frame buffer). Two flows never share an
   instance. The codec *type* is `Send + Sync`; instances are `Send`
   only.

These invariants are load-bearing for:
- Byte-faithful pass-through when no transform fires (clients see
  what they would have seen).
- Independence of concurrent flows (no surprising shared mutex).

---

## 3. The `Codec` trait

```rust
/// A protocol or framing codec at one layer in the stack.
/// Factory shape — `Codec` configures the codec; `open` produces
/// a per-flow stateful instance.
pub trait Codec: Send + Sync + 'static {
    /// The event type this codec consumes (the layer below).
    type Input: Send + 'static;
    /// The event type this codec produces (the layer above).
    type Output: Send + 'static;

    /// Stable name for logging / config / metrics.
    fn name(&self) -> &'static str;

    /// Cheap routing predicate. The engine picks the first
    /// registered codec whose `matches` fires for a given flow.
    /// Must not consume the input stream.
    fn matches(&self, probe: &CodecProbe<'_>) -> bool;

    /// Open a per-flow stateful instance.
    fn open(&self) -> Box<dyn CodecInstance<Input = Self::Input, Output = Self::Output>>;
}

pub trait CodecInstance: Send + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;

    /// Decode one input item (response pipeline). Returns the
    /// stream of output events produced by THIS item — typically
    /// 0-N. State advances; consecutive frames may interact.
    /// Errors do not appear in the signature: see §13 for the
    /// empty-on-error + audit-channel contract.
    fn decode(&mut self, item: Self::Input) -> Vec<Self::Output>;

    /// Encode one output item (request pipeline). Inverse of
    /// `decode`. Round-trip: `encode(decode(x)) == x` for inputs
    /// no transform mutated. Same error contract as `decode` (§13).
    fn encode(&mut self, item: Self::Output) -> Vec<Self::Input>;

    /// End-of-stream drain (response). Codecs that buffer trailing
    /// state (half-assembled tool calls, partial close markers)
    /// release here. Default is empty. Same error contract as
    /// `decode` (§13).
    fn flush(&mut self) -> Vec<Self::Output> { Vec::new() }
}
```

### 3.1 What `CodecProbe` carries

`CodecProbe` is the routing input. It exposes enough to make a
cheap matching decision without buffering the body:

```rust
pub struct CodecProbe<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub method: &'a Method,
    pub request_headers: &'a HeaderMap,
    pub response_status: Option<StatusCode>,   // None on request side
    pub response_content_type: Option<&'a str>,
}
```

Matching is allowed to inspect headers, host, path, and content
type. Matching is **not** allowed to buffer body bytes or run any
async work. First-match-wins; registration order is the contract.

---

## 4. The `Transform` trait

```rust
/// An inspector that attaches at one altitude in the stack.
/// Like `Codec`, factory + instance split.
pub trait Transform: Send + Sync + 'static {
    /// The event type this transform consumes AND produces. A
    /// transform never changes the type — that's a codec's job.
    type Event: Send + 'static;

    fn name(&self) -> &'static str;

    /// Open a per-flow stateful instance.
    fn open(&self, attachment: &TransformAttachment) -> Box<dyn TransformInstance<Event = Self::Event>>;
}

pub trait TransformInstance: Send + 'static {
    type Event: Send + 'static;

    /// Apply to one event. Returns the (possibly empty, possibly
    /// modified, possibly multi-valued) output stream produced by
    /// this input. Side-effects (hints, artifacts, audit events)
    /// go on the side channel. Errors are emitted as
    /// `AuditEvent { kind: Errored, ... }` on the side channel
    /// and the method returns `Vec::new()` — see §13.
    fn apply(&mut self, event: Self::Event, side: &mut SideChannelTx<'_>) -> Vec<Self::Event>;

    /// End-of-stream drain. Same error contract as `apply` (§13).
    fn flush(&mut self, side: &mut SideChannelTx<'_>) -> Vec<Self::Event> { Vec::new() }
}

#[derive(Clone, Debug)]
pub struct TransformAttachment {
    /// Which event type this transform attaches to. Matches the
    /// codec layer that produces / consumes it.
    pub layer: Layer,
    /// Which pipeline this transform runs on. A transform may be
    /// registered against both (e.g. a header redactor running on
    /// request AND response).
    pub pipeline: Pipeline,
    /// Deterministic order within the same (layer, pipeline) slot.
    /// Lower runs earlier.
    pub order: u32,
    /// Optional flow predicate — only apply when this matches the
    /// flow's `CodecProbe`. Lets transforms be scoped to one
    /// vendor without baking the vendor into the transform itself.
    pub guard: Option<Arc<dyn Fn(&CodecProbe<'_>) -> bool + Send + Sync>>,
}

#[derive(Clone, Copy, Debug)]
pub enum Layer {
    Tls,
    WireFraming,
    AppProtocol,
    BodyFraming,
    VendorSemantics,
}

#[derive(Clone, Copy, Debug)]
pub enum Pipeline {
    Request,
    Response,
    Both,
}
```

### 4.1 The four old roles, restated

| Old (`005`) | New, as a `Transform` |
|---|---|
| `Detector::detect` | `apply` returns input unchanged; emits `Hint`(s) on side channel |
| `Injector::inject` | `apply` returns mutated event (request pipeline); emits `Audit::Injected` |
| `Injector::extract` | `apply` returns input unchanged (response pipeline); emits `Artifact{name, value}` |
| `Filter::process` | `apply` returns modified / dropped / inserted events (response pipeline) |

Three traits collapse to one shape because all three are
specializations of "transform a typed stream, emit typed side
effects."

---

## 5. Side-effect channels

Three typed buses run alongside the event pipelines. Every
`Transform` can emit on any of them:

```rust
pub enum SideEffect {
    /// A confidence-ranked opinion about one attribution category.
    /// Consumed by `Resolver` to produce a `Resolved` map.
    Hint(ContextHint),

    /// A captured named value (e.g. `<noodle:work_type>` content).
    /// Carries the chain of custody: which transform captured it,
    /// from which layer, at what timestamp.
    Artifact(Artifact),

    /// Operational events — Inject fired, Redact fired, Filter
    /// dropped a frame, etc. Distinct from the wire-log layer,
    /// which records pre-transform raw traffic.
    Audit(AuditEvent),
}

pub struct ContextHint {
    pub category: SmolStr,        // "tool", "session", "model", …
    pub value: SmolStr,
    pub confidence: f32,          // [0.0, 1.0]
    pub source: SmolStr,          // transform name
}

pub struct Artifact {
    pub name: SmolStr,
    pub value: SmolStr,
    pub source_layer: Layer,
    pub source_transform: SmolStr,
    pub flow_id: FlowId,
    pub captured_at_unix_ms: u64,
}

pub struct AuditEvent {
    pub kind: AuditKind,           // Injected, Redacted, Filtered, Errored
    pub layer: Layer,
    pub transform: SmolStr,
    pub flow_id: FlowId,
    pub at_unix_ms: u64,
    pub detail: serde_json::Value, // structured per-kind payload
}
```

The buses are **layer-agnostic**: a hint emitted at L3 from an HTTP
header detector is structurally identical to a hint emitted at L5
from a NormalizedEvent inspector. The `Resolver` collects all hints
in one pass at flow-end and produces the final
`Resolved{category → value}` map.

---

## 6. The two pipelines

Each flow has two strict pipelines:

```
   request           response
 ──────────►       ◄──────────
 ┌──────────────────────────┐
 │ L5  Vendor semantics     │   transforms on Request fire here BEFORE encode
 ├──────────────────────────┤
 │ L4  Body framing         │   on Request: encode body → bytes
 ├──────────────────────────┤   on Response: decode bytes → frames
 │ L3  Application protocol │
 ├──────────────────────────┤
 │ L2  Wire framing         │
 ├──────────────────────────┤
 │ L1  TLS                  │
 ├──────────────────────────┤
 │ L0  Transport            │
 └──────────────────────────┘
```

- **Request pipeline** (top → down): the engine accepts a request
  at L3 (rama hands us a parsed `HttpRequest`), runs L3 request
  transforms, asks L5 codec (if any) to mutate the body events, then
  L4 codec encodes them back to bytes, and rama sends the request
  upstream.
- **Response pipeline** (bottom → up): bytes arrive at L2, rama
  decodes h1/h2/h3 framing into `HttpResponse`, the engine runs L3
  response transforms, L4 codec decodes the body into
  `BodyFrameEvent`s, L4 response transforms run, L5 codec decodes
  into `NormalizedEvent`s, L5 response transforms run, and the
  re-encoded result is sent to the client.

Transforms registered as `Pipeline::Both` run on both pipelines
(e.g., a header redactor that hides `Authorization` in both
directions).

### 6.1 What guarantees encode preserves

The two pipelines are joined by the round-trip invariant
(§2.1.1). For any event passing through unchanged by transforms,
encode(decode(x)) == x at the byte level. This is the contract
that lets noodle sit transparently in front of a client without
affecting any byte the client sees, *until* a transform explicitly
chooses to mutate.

---

## 7. The engine

`InspectionEngine` is a typed-event router. Per flow:

1. Build a `CodecProbe` from the incoming HTTP request.
2. Walk the registered codec layers (L2 DNS branch OR L2 HTTP →
   L3 → L4 → L5) and select the first-matching codec at each.
3. `open()` a `CodecInstance` for each selected codec.
4. For each `TransformAttachment` whose `layer` matches a selected
   codec's output AND whose `guard` (if set) accepts the probe,
   `open()` a `TransformInstance`.
5. Spawn the request pipeline (top-down) and response pipeline
   (bottom-up) as two async streams.
6. On flow end: `flush()` every codec instance and transform
   instance in order; collect side-effects; pass `Hint`s to the
   `Resolver`; emit `Artifact`s and `AuditEvent`s to their sinks.

The engine has no knowledge of vendors, protocols, or use cases.
Vendors are codecs, use cases are transforms, protocols are wire
codecs.

### 7.1 Resolver and `Resolved`

The `Resolver` is unchanged from 005's design. It takes the full
list of hints emitted by all transforms during a flow and produces
`Resolved{category → value}` by:

1. Group hints by `category`.
2. Take the max-confidence hint in each category.
3. Tie-break by registered detector priority order.
4. Validate against the category's `values:` allow-list.
5. Apply category defaults.

Resolving doesn't know which layer the hints came from. A hint
from a Transform<DnsMessage> ("saw HTTPS RR with `alpn=h3`") and a
hint from a Transform<HttpRequest> ("UA matches Claude Code") and
a hint from a Transform<NormalizedEvent> ("model field reads
claude-3.5-sonnet") all funnel into the same `category → value`
output.

---

## 8. Worked example — LLM cost attribution

The use case Joe described: the system modifies outbound LLM
requests to embed a directive, captures the model's classification
of its work, and emits a record attributing cost to a category.

Implementation entirely in terms of the abstractions above.

### 8.1 Components

| Layer | Pipeline | Transform | Behavior |
|---|---|---|---|
| L3 | Request | `SessionDetector` | Reads `X-Claude-Code-Session-Id` header; falls back to `sha256(request_body.system)[:12]`. Emits `Hint{category: "session", value, source: "session_detector", confidence: 0.95-1.0}`. |
| L3 | Request | `IdentityDetector` | Reads UA, request body's `model` field. Emits `Hint`s for `tool`, `model`. |
| L3 | Request | `AttributionInjector` | Mutates `request.body.system`: appends a directive instructing the model to emit `<noodle:work_type>CATEGORY</noodle:work_type>` near end-of-response. Emits `Audit{kind: Injected, detail: { directive_id, categories }}`. Gated per-turn via session store: only fires on the first request in a turn (subsequent in-turn requests already carry the directive in the model's conversation context). |
| L5 | Response | `MarkerScannerTransform` | Stateful FSM. Watches `NormalizedEvent::Token { text, raw }` events. When it finds `<noodle:work_type>X</noodle:work_type>` in the token stream, drops the markers from the stream the client sees, and emits `Artifact{name: "work_type", value: X, source_layer: L5, source_transform: "marker_scanner"}`. The FSM crosses event boundaries (a marker can straddle multiple Tokens). |

### 8.2 Flow walk-through

```
1. Client → HTTP POST /v1/messages (Claude Code dials Anthropic).
   rama terminates TLS, decodes h2, produces HttpRequest at L3.

2. L3 request transforms run in order:
   ├─ SessionDetector reads X-Claude-Code-Session-Id → Hint(session, "abc123def4", conf=1.0)
   ├─ IdentityDetector reads UA "claude-cli/0.4.2" → Hint(tool, "Claude Code", conf=0.95)
   ├─ IdentityDetector reads body.model → Hint(model, "claude-haiku-4-5", conf=1.0)
   └─ AttributionInjector mutates body.system, appends directive.
                                Emits Audit(kind=Injected)
   Mutated HttpRequest re-encoded by L4/L3/L2 codecs and forwarded upstream.

3. Anthropic returns SSE response.
   L2 (rama) decodes h2 → HttpResponse.
   L4 SseFrameCodec decodes body → BodyFrameEvent::Sse{event_type, data, raw}.
   L5 AnthropicCodec decodes → NormalizedEvent (TurnStart, Token×N, TurnEnd, …).

4. L5 response transforms run per NormalizedEvent:
   └─ MarkerScannerTransform watches Tokens. When the model emits
      "<noodle:work_type>code-review</noodle:work_type>", the FSM:
        a. drops those bytes from the Token stream forwarded to the client,
        b. emits Artifact{name="work_type", value="code-review", source_transform=…}.

5. L5 codec re-encodes the (now markers-stripped) NormalizedEvent stream
   back through L4 (SseFrameCodec) → L3 → L2 → bytes to client.

6. Flow ends.
   Engine calls flush() on every CodecInstance and TransformInstance.
   Engine drains side channels.

7. Resolver collapses Hints:
     Resolved = { session: "abc123def4", tool: "Claude Code",
                  model: "claude-haiku-4-5" }

8. Audit sink receives the unified record:
     { flow_id, ts_unix_ms, resolved: Resolved,
       artifacts: [{name: "work_type", value: "code-review", …}],
       audits:    [{kind: Injected, …}],
       cost_basis: { input_tokens, output_tokens, model }   ← from L5 too }
```

### 8.3 Why this generalizes

Nothing in the architecture knows that "work_type" is the
classification axis. Swap `AttributionInjector`'s directive for
one that asks the model to emit `<noodle:customer_id>` or
`<noodle:project>` or `<noodle:risk_score>`, swap
`MarkerScannerTransform`'s capture set, and the same pipeline
produces a different attribution. The architecture is
classifier-agnostic; cost attribution is one configuration of it.

The same architecture also supports:

- **Compliance redaction**: a `Transform<NormalizedEvent>` that
  drops Tokens matching a PII regex.
- **Per-tenant quotas**: a `Transform<HttpRequest>` on the request
  pipeline that consults a counter and emits
  `AuditEvent::Throttled`.
- **DNS-based provider routing**: a `Transform<DnsMessage>` that
  rewrites answers for managed devices.
- **Cross-vendor latency comparison**: a `Transform<BodyFrameEvent>`
  that timestamps every SSE frame and emits Artifacts the viewer
  groups by vendor codec.

The classifier-driven attribution Joe is targeting is one
instance.

---

## 9. Attachment cheat sheet — what attaches where

This table is the practical answer to "where do I plug in?"

| If you want to … | Attach a `Transform<E>` at | Pipeline | Emits |
|---|---|---|---|
| Decide whether to MITM by SNI | L1 (TLS hello peek — see Story 011) | Request | `Hint(sni)` or `AuditEvent::Skipped` |
| Strip `alpn=h3` from DNS | L2 (DNS branch) | Response | mutated `DnsMessage` |
| Log resolved-IP → hostname pairs | L2 (DNS branch) | Response | `Artifact(ip_to_host, …)` |
| Pick the right vendor codec for routing | L3 (HttpRequest) | Request | `Hint(provider)` consumed by L5 codec selection |
| Extract session ID from headers | L3 | Request | `Hint(session)` |
| Inject a directive into `system` | L3 | Request | mutated `HttpRequest`, `Audit::Injected` |
| Redact `Authorization` header | L3 | Both | mutated `HttpRequest`/`HttpResponse`, `Audit::Redacted` |
| Drop SSE keepalive frames from telemetry | L4 | Response | filtered `BodyFrameEvent` |
| Timestamp per-frame arrival | L4 | Response | `Artifact(frame_ts)` |
| Capture `<noodle:*>` markers from model output | L5 | Response | filtered `NormalizedEvent`, `Artifact` |
| Count tokens per turn | L5 | Response | `Hint(tokens_out)`, `Hint(tokens_in)` |
| Detect tool name from `tool_use` block | L5 | Response | `Hint(tool, "<name>")` |

Every row is the same trait. The cells differ only in which event
type and which pipeline.

---

## 10. The `DeclarativeCodec` extension

The codec abstraction makes a config-driven vendor codec
straightforward. A `DeclarativeCodec<Spec>` is an `impl
Codec<Input = BodyFrameEvent, Output = NormalizedEvent>` whose
behavior is fully described by a `Spec` value:

```rust
pub struct CodecSpec {
    pub name: SmolStr,
    pub match_predicate: MatchPredicate, // host glob, path glob, content-type
    pub frame_grammar: FrameGrammar,     // SseTyped | SseData(terminator) | Json
    pub events: Vec<EventMapping>,       // event_name → NormalizedEvent emission
}

pub struct EventMapping {
    pub when: JsonPathExpr,              // selector / guard
    pub emit: EmitTarget,                // TurnStart | Token | ToolCall | TurnEnd | Metadata
    pub fields: HashMap<&'static str, JsonPathExpr>,   // turn_id, text, role, finish, …
}
```

Specs live on disk at `config/codecs/*.codec.json`. Adding
Perplexity, Cohere, Mistral, Groq, Gemini, Bedrock — each becomes a
file. No Rust recompile. The same grammar inverted gives request-side
injectors (mutate `messages` or `system` via JSONPath).

Two hand-written codecs (Anthropic, OpenAI) earn the abstraction
once they share enough structure. Hand-written first, declarative
third — never declarative before the trait shape is grounded by two
real impls.

---

## 11. What the architecture deliberately does NOT do

- **It does not absorb rama's TCP / TLS / h1 / h2 framing.** Those
  are L0-L2. We consume rama's output; we don't reproduce it.
- **It does not commit to OSI layer numbering.** The number of
  layers emerges from the protocols, not from imposed structure.
  L0–L5 are descriptive of where things sit, not a contract.
- **It does not unify `Codec` and `Transform` into one trait.**
  They are intentionally separate. Codec changes types (encode +
  decode, round-trip invariant); Transform preserves types
  (mutate + side-effect, no round-trip). Merging them was tested
  in design and discarded — the round-trip invariant is too
  important to make optional.
- **It does not centralize ordering.** Transform order within a
  `(layer, pipeline)` slot is per-registration; cross-slot order is
  determined by the codec stack (response: L2 → L3 → L4 → L5;
  request: reverse). Engines that need different ordering would
  build their own slot policy; the trait shape doesn't require it.
- **It does not require all flows to traverse all layers.** A flow
  with no body (HEAD request) skips L4 + L5. A DNS flow uses the L2
  DNS branch and never visits L3/L4/L5. The codec selector at each
  layer gracefully returns nothing.

---

## 12. Decisions

1. **Codec selection across layers — independent per layer.** Each
   layer's `CodecRegistry` is autonomous. Cross-layer constraints
   surface as `Hint` emissions on the side channel — a
   `Transform<HttpRequest>` may emit `Hint("provider", "anthropic")`
   which the L5 selector consumes — not as coupled selection logic.
2. **Async vs sync `apply` — sync only for v1.** `Transform::apply`
   is `&mut self` and returns `Vec<E>` (in practice
   `SmallVec<[E; 2]>`). An `AsyncTransform` variant lands when the
   first real classifier-driven L5 transform forces it. The hot path
   (L2/L4) never needs async.
3. **Backpressure — bounded `mpsc` channels between layers.** Default
   channel capacity 64 events; per-flow override at `CodecRegistry`
   registration time. A slow L5 transform applies backpressure to L4
   by way of `channel-full → L4 awaits send`, propagating to L3 → L2
   → transport.
4. **Cross-flow state — typed handle to `SessionStore`.** Session
   correlation lives on `SessionStore`. Transforms read/write through
   a typed handle; the `Transform` trait does not change. The handle
   is part of `TransformAttachment`.
5. **Error model — empty-on-error with side-channel audit.** See §13
   for the contract and the verification machinery.

**Open.** Spec language for `DeclarativeCodec` (JSON with JSONPath,
YAML, or a small DSL): decide after the second hand-written codec
forces the grammar.

**Buffering primitives.** Stateful streaming buffers across multiple
events are a separate concern. Codec and transform implementations
that need bounded buffering with release decisions, deadlines, and
overflow audit use the `CacheAndRelease` and `Extractor` primitives
specified in
[`016-cache-and-release-primitives.md`](016-cache-and-release-primitives.md).

---

## 13. Error model — empty-on-error with audit-channel enforcement

This section pins the error semantics for every `Codec` and
`Transform` method that can fail. The full contract follows; the
verification machinery in §13.3 makes it safe to use.

### 13.1 The decision

`CodecInstance::decode`, `CodecInstance::encode`,
`CodecInstance::flush`, and `TransformInstance::apply` all return
`Vec<T>` directly. Their signatures do **not** carry `Result`. No
associated `Error` type. No `From<E1> for E2` shims at layer
boundaries.

On failure — parse error, malformed input, exhausted buffer,
unrecognized vendor envelope, anything the implementation
considers an error — the method:

1. Emits one `AuditEvent { kind: AuditKind::Errored, ... }` on the
   side channel, carrying enough structured `detail` for an
   operator to diagnose without re-running the flow (the failed
   input bytes if small, the parser state, the codec name, the
   layer, the flow id).
2. Returns `Vec::new()` (or `SmallVec::new()`).

The flow continues. Downstream layers see an empty event slot for
that input. The audit sink and `Resolver` observe the error
uniformly with other side effects.

### 13.2 Why this shape, not `Result`

Three forces drove it:

1. **Errors in this architecture are observable events, not
   control flow.** They sit alongside `Inject`, `Redact`, and
   `Filtered` audits on the same side channel. The audit sink is
   already load-bearing; routing errors through it keeps one
   path, one consumer model.
2. **The round-trip invariant (§2.1.1) is trivially preserved on
   error paths.** `Vec::new()` round-trips to nothing — same as if
   no input had arrived. With `Result`, every codec would have to
   re-pin "what bytes do I emit on error?" and the contract gets
   ambiguous.
3. **`Result` proliferates `Error` enums across five layers.**
   Each codec would carry its own `CodecError`. Every transform
   would carry `TransformError<E>`. Every layer boundary would
   need `From<...>` shims to converge errors back into the
   engine's vocabulary. The surface area cost is large and the
   ergonomic loss is felt by every codec author.

### 13.3 The verification contract — load-bearing

Empty-on-error is dangerous *only* if errors silently disappear.
The contract that makes it safe:

**C-1. Every empty return on a failure path emits exactly one
`AuditEvent { kind: Errored, ... }`.** Codecs that return empty
because the input legitimately contains no output (e.g. a
line-framer buffering a partial line) do **not** emit. The audit
sink is for *errors*, not for *successful zero-output*.

**C-2. Every codec and transform ships with property tests that
prove C-1.** For an arbitrary malformed-input strategy, the test
asserts:
- the method returns `Vec::new()`, and
- the side channel received at least one `AuditEvent::Errored`
  attributable to that codec.

`proptest` is the recommended harness. The test pattern is
trivial and uniform across codecs — a shared helper in the test
support crate can wrap the "feed malformed input + assert audit"
check.

**C-3. The engine compares "empty returns" to "audit emissions"
per flow, at flush time, and logs a warning when they diverge.**
This is the runtime backstop for C-1. If a codec returns empty
without emitting an audit (a bug), the engine surfaces it
operationally. The warning is a `WARN`-level log with the codec
name and the empty-vs-audit delta; production deployments
should alert on it.

**C-4. The audit sink is non-optional.** A flow with no audit
sink configured is a configuration error, not a deployment mode.
The engine refuses to start without one. Operators may pick the
sink (JSON-lines file, stdout, OTLP, structured tracing) but
must pick one.

**C-5. Certain errors halt the flow, not just audit.** Some
failures are not "operational events" — they're flow-fatal. The
canonical list, pinned here:

| Failure | Why flow-fatal | Action |
|---|---|---|
| TLS cert minting failure (MITM path) | The client cannot complete the handshake; tunneling would leak plaintext we can't see | Emit `Errored` audit + return error to engine + drop the flow |
| Round-trip invariant violation (encode produces bytes that do not match decoded input on a frame the codec marked `FrameSource::Upstream`) | The client would see modified bytes it did not authorize | Emit `Errored` audit + halt the flow + record `AuditKind::InvariantViolation` |
| `flush()` returns events the codec cannot encode | The engine cannot drain cleanly; bytes would be lost | Emit `Errored` audit + halt the flow |

Flow-fatal errors are the exception, not the rule. They use a
narrow back-channel — a `Result<(), FatalFlowError>` return from
the **engine's** wrapper, not from the codec trait method itself.
The codec/transform methods stay `Vec<T>`; the engine recognises
specific audit kinds (`InvariantViolation`, etc.) and converts
them to flow termination. This keeps the common case ergonomic
while still allowing flows to fail closed when correctness
demands it.

### 13.4 What this means for codec authors

- Don't bubble errors. Emit an `Errored` audit and return empty.
- Don't `panic!` on bad input. Emit an audit and return empty.
- Don't silently return empty without an audit — write the audit
  every time, and let property tests prove it.
- Use `AuditKind::Errored` for recoverable parse failures.
  Reserve `AuditKind::InvariantViolation` for round-trip /
  correctness failures the engine should escalate.
- For flow-fatal conditions (cert minting, invariant violation),
  emit the audit and let the engine's flow wrapper handle the
  halt — codec methods themselves still return `Vec<T>`.

### 13.5 What this means for the engine

- Wire an audit sink before the first flow. Refuse to start
  otherwise (C-4).
- Track per-flow `empty_return_count` and `errored_audit_count`.
  At flush, log `WARN` if they diverge (C-3).
- Recognize `AuditKind::InvariantViolation` (and the per-failure
  kinds listed in C-5) and convert to flow termination.
- Surface `Errored` audit rates as a flow-level metric the
  operator can alert on.

### 13.6 What this means for tests

- Every codec test module includes a property test asserting C-1.
- The test support crate exposes a `feed_malformed_and_assert_audit`
  helper to keep the property body trivial.
- Integration tests for the engine verify C-3 (warning on divergence)
  and C-4 (refusal to start without a sink).
- Round-trip invariant tests treat any non-empty `Errored` audit
  emission for a `FrameSource::Upstream` frame as a test failure —
  if the codec audits an error while processing an upstream-tagged
  frame, the invariant assertion that follows is meaningless and
  the test must surface that.

### 13.7 Open

This decision is binding. Two narrow questions remain:

1. **The exact `AuditEvent::detail` schema for `Errored`.** What
   structured fields beyond `codec_name`, `layer`, `flow_id`,
   `input_byte_snippet`? Lands when the first real codec (027 or
   028) emits one.
2. **Should `InvariantViolation` be a separate `AuditKind`, or a
   `severity` field on `Errored`?** Separate kind reads cleaner;
   severity field generalizes. Punt to 027 implementation; the
   first real engine wiring forces the choice.

---

## 14. Cross-references

- [`../architecture/architecture.md`](../architecture/architecture.md) — the layered model this doc's trait surface codifies.
- [`004-attribution-model.md`](004-attribution-model.md) — conceptual attribution model. §8 implements its ideas on this architecture.
- [`../knowledge/quic-and-http3-primer.md`](../knowledge/quic-and-http3-primer.md) — why DNS interception matters; what L2 DNS codec inspectors do.
- [`021-detector-vs-transform-two-tier.md`](021-detector-vs-transform-two-tier.md) — `RequestDetector`, the second read-only interface alongside `Transform`.
- [`../diagrams/015-layered-codec-architecture.drawio`](../diagrams/015-layered-codec-architecture.drawio) — companion diagram.
- [`../diagrams/noodle-component-object-model.drawio`](../diagrams/noodle-component-object-model.drawio) — the broader component model.
- [`016-cache-and-release-primitives.md`](016-cache-and-release-primitives.md) — buffering primitives both codecs and transforms need.
