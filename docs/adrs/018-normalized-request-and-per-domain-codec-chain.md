# 018 — Normalized request model + per-domain (host+path) codec chain

**Status:** current. The shipped request pipeline is single-stage
`Bytes → NormalizedRequest` per domain; the response pipeline remains the
two-stage L4 + L5 shape from ADR 015.

**References:** ADR 015 §2 / §6 / §14 (layered stack, request pipeline,
codec selection), ADR 017 (`EventSource` — the L5 response-side
provenance this mirrors on the request side), ADR 019 (the dispatch
contract this populates), the mitm captures in `captures/` (gitignored,
secret-bearing).

---

## 1. Context — the finding (from real bytes)

Item 3 — "wire the attribution-directive injector onto the request path"
— surfaced that **no request decode path exists** and that the request
envelope is **not one shape**. Evidence from the captures:

**`api.anthropic.com`** — public Messages API (SDK / Claude Code / our
own forward proxy / OpenWhispr voice-cleanup). Request is JSON
`{model, system, messages:[{role,content}], stream}`. Response is either
streaming SSE *or*, for non-streaming callers like OpenWhispr, a **single
JSON body** (one POST → one JSON response). Injection point: `system`
(or a system turn in `messages`).

**`claude.ai`** — Claude Desktop / web backend, HTTP/2, cookie-auth,
`anthropic-client-*` headers. The chat turn is
`POST /api/organizations/{org}/chat_conversations/{conv}/completion`
with JSON `{"prompt":"<user text>","personalized_styles":[{…,"prompt":"Normal\n"}],"model","tools":[…],"locale","timezone"}`.
Response is SSE with a **different L5 taxonomy** (`conversation_ready`,
`message_start` with a `chatcompl_` envelope + `parent_uuid` /
`trace_id`, `content_block_start` with `flags` / `citations`) but the
**same `text_delta` shape**. Injection point: `prompt` or
`personalized_styles[].prompt` — *not* `system` / `messages`.

**A single host carries many unrelated schemas — both hosts.**
`claude.ai` in the chat capture: HTML shell, statsig / growthbook
telemetry, i18n, MCP-over-SSE (`/mcp/v2/bootstrap`), JSON-RPC
(`/v1/toolbox/shttp/mcp/…`), plain JSON (`/v1/code/…`), and the chat
completion. `api.anthropic.com` is **also multi-path**: it hosts the
documented public Messages API (`/v1/messages`, streaming + non-streaming
— SDK, Claude Code, OpenWhispr, our own forward proxy) **and** non-model
paths (e.g. `/api/desktop/…/update` app-update checks). A
`*.anthropic.com` host match would attach the vendor codec to telemetry,
app updates, and MCP — on *both* hosts. Host + path is required.

Two invariant facts across all of it:

- **L4 SSE frame grammar is identical** everywhere (`event:` / `data:`
  / blank line) → one `SseFrameCodec` covers the response side.
- **L5 vendor semantics and request envelopes diverge per domain** →
  per-domain L5 codecs; the injector cannot be vendor-aware or it
  re-implements this matrix.

## 2. Decision

### 2.1 Codec selection is host + path + accept / content-type, never host alone

`CodecProbe` carries `path`, `request_headers`, `response_content_type`;
the matching contract is a narrow predicate (host + path prefix + accept).
`*.anthropic.com`-style host-only matching is forbidden — proven wrong by
the captures.

### 2.2 `NormalizedRequest` — the request-side analog of `NormalizedEvent`

A per-domain request codec decodes the wire envelope into
`NormalizedRequest`; transforms mutate it; the same codec encodes it
back. The engine and the injector never name a vendor (ADR 015's central
point), exactly as the response side already works.

### 2.3 `NormalizedRequest` carries an abstract `SystemDirective` slot

Plus the user turn / message list. The per-domain *encoder* maps
`SystemDirective` to the right wire field. The mapping by domain:

- **`api.anthropic.com`** → the `system` field, which the Claude Code
  CLI capture (`claude-code-cli-multi-turn-capture.mitm`) proved is a
  **list of typed blocks** `[{"type":"text","text":…}]`, *not* a bare
  string. The directive maps to **appending a `{type:text,text}` block**
  to that array. Stateless: the client resends the full `messages[]` +
  `system` every turn (turn N carries all prior turns), so the injector
  writes `system` every request — idempotent-by-replacement (§4 holds).
  Auth is `Authorization` (OAuth Bearer) for the CLI; `x-api-key` for
  SDK / curl — does not affect host + path selection.
- **`claude.ai`** → `personalized_styles[].prompt`. Across nine turns
  of the multi-turn capture the request body carries **no history**
  (`history_array_present=no`); state is server-side keyed by the
  conversation UUID; `personalized_styles` is **client-rebuilt and
  resent on every completion POST** (identical `Normal\n` each turn).
  The directive therefore goes in that steering slot, **not** the
  top-level `prompt` (which is stored verbatim as the user's turn in
  server history → would leak + accumulate across turns).
- **Generic fallback** → prepend to the first user turn.

Both domains reduce to the same normalized concept — "per-request
steering, resent every turn" — different wire fields. The abstraction
holds; the injector is vendor-agnostic.

The **`AttributionInjector` is one vendor-agnostic
`Transform<NormalizedRequest>`** that only ever sets `SystemDirective`.
All wire-format knowledge lives in the per-domain codecs.

### 2.4 Request-side codecs are single-stage `Bytes → NormalizedRequest`

The request direction uses **single-stage** per-domain codecs:
`Codec<Input = Bytes, Output = NormalizedRequest>`. Request bodies are
bounded (unlike streaming responses), so the codec receives the complete
request body in one `decode` call. There is no L4 / L5 split on the
request side.

The response direction is unchanged from ADR 015: L4 `SseFrameCodec`
(or `JsonChunkCodec` for non-streaming) produces `BodyFrameEvent`, and
per-domain L5 codecs decode that into `NormalizedEvent`.

### 2.5 Non-streaming response codec coverage is a separate concern

A `JsonChunkCodec` L4 for single-JSON response bodies (the OpenWhispr
case: one POST → one JSON response from `api.anthropic.com`) is **not**
delivered by ADR 018. It remains open as a response-direction concern
tracked separately.

### 2.6 Request-body byte fidelity — raw replay when un-injected

The request codec carries provenance exactly as `EventSource` does on
the response side (ADR 017), applied to the request body:

- The codec instance retains the **raw request bytes** plus the parsed
  `serde_json::Value`.
- `encode`, **un-injected** (`!system.is_directive_set()`) → replay the
  retained raw bytes **verbatim** (byte-identical; the ADR 015 §2.1.1
  invariant, met by construction — identical to `EventSource::Upstream`).
- `encode`, **injected** → set the active
  `personalized_styles[].prompt` (claude.ai) / `system`
  (api.anthropic.com) on the retained `Value` to
  `SystemDirective::composed()` and re-serialise. Byte identity is
  neither required nor meaningful here — the request was deliberately
  modified; key order is irrelevant to the API. Every un-modelled field
  is preserved because we mutate the retained `Value`, not a
  reconstruction.

This requires no new dependency and keeps request / response fidelity
on one shared principle: *raw when unmodified, re-serialise when
mutated.*

## 3. Alternatives rejected

- **Coarse L4 JSON transform that edits `system` / `messages`.** Cannot
  express ≥ 8 schemas on one host or two different request envelopes
  without host-conditional branching *inside a transform* — the
  parallel-path slop the layered design exists to prevent. Killed by
  the captures.
- **HTTP-layer body rewrite in the proxy, bypass the engine.** Fastest,
  but a second non-layered path = debt; discards the `RequestFlow` seam
  the engine already provides.
- **Host-only codec matching.** Attaches the vendor codec to telemetry
  / app-update / MCP traffic. Empirically wrong.
- **Two-stage `Bytes → JsonChunk → NormalizedRequest`.** Re-litigates
  shipped, tested single-stage slices for no benefit on bounded
  non-streaming request bodies; more churn, larger blast radius.
- **`parse Value → mutate → re-serialise` for un-injected requests.**
  `serde_json` reorders map keys (the workspace has no
  `preserve_order`) and reformats numbers / whitespace, so an
  un-injected request would not round-trip byte-identically. The retain-
  raw approach in §2.6 is simpler and strictly equivalent for the gate.

## 4. Consequences

- **New:** `NormalizedRequest` + `SystemDirective` in `noodle-core`;
  per-domain request codecs in `noodle-adapters` (anthropic Messages;
  claude.ai chat-completion); a formalized `CodecProbe` matching
  contract; `AttributionInjector` transform; `noodle-proxy` wiring of
  `open_request_flow` on the **outbound** path.
- **Engine shape:** `InspectionEngine` carries **dedicated request
  registries**: `req_codecs: CodecRegistry<Bytes, NormalizedRequest>`
  and `req_transforms: TransformRegistry<NormalizedRequest>`. Response
  registries (`l4` / `l5`) and `ResponseFlow` are **untouched**.
- **`RequestFlow` shape:** one decode `CodecInstance<Bytes,
  NormalizedRequest>`, a `Vec<TransformInstance<NormalizedRequest>>`
  chain, and a separate encode instance of the same codec (raw-replay
  when `!is_directive_set()`, re-serialise when injected per §2.6).
- **Reused:** `SseFrameCodec` unchanged. `EventSource` / `Mutated`
  provenance (ADR 017) — the request encoder re-serialises only when
  the injector set `SystemDirective` (same mutate ⇒ re-serialise
  contract).
- **Decompression must precede decode.** `claude.ai` uses br / zstd /
  gzip; mutating compressed bytes corrupts the request. The proxy
  decodes raw bytes only; if an outbound request carries a
  `content-encoding` the proxy does not model, `open_request_flow`
  declines (passthrough verbatim) rather than risk corrupting a billed
  request. Request re-compression is out of scope until a capture
  proves a compressed request exists.
- **Codec dispatch order.** `AnthropicMessagesRequestCodec`
  (`api.anthropic.com` + `/v1/messages`) and `ClaudeAiChatRequestCodec`
  (`claude.ai` + `…/chat_conversations/…/completion`) have
  non-overlapping host + path predicates; registration order is
  immaterial.
- **Side effects + flow id.** `RequestOutput.side_effects` are emitted
  to the same sink the response path uses, stamped with the per-request
  id at the proxy seam (before forwarding).

## 5. Security considerations

Directive injection **mutates the user's outbound request** — higher
blast radius than the response-side redaction:

- **Idempotency.** The steering channel (`personalized_styles` /
  `system`) is **client-rebuilt and resent on every request** and is
  **never echoed back** into the next request (server-side history is
  referenced by `parent_message_uuid`, not resent). The injector is a
  stateless per-request rewrite — "ensure the steering slot carries the
  directive" — applied on every POST. There is no accumulation to
  dedupe and **no marker tracking is needed**: we never read our own
  prior output.
- **No leak into stored history.** `claude.ai` persists `prompt` into
  conversation history; injecting there would surface the scaffolding
  to the user and into future turns. The directive goes in the steering
  slot (`personalized_styles` / `system`), which is not echoed back as
  user content.
- **Conversation integrity.** The injector only *adds* a system
  directive; it never edits the user's message text, tool list, or
  model selection. A decode / encode that is not byte-faithful for the
  un-injected case is a flow-fatal bug; §2.6 makes the contract
  enforceable by construction.
- **Secrets.** Captures carry live cookies / `x-api-key`. Never logged,
  never in audit `detail`, never committed (`captures/` is gitignored).
- **Compression.** Mutating compressed bytes corrupts the request;
  decode operates on decompressed bytes only.

## 6. Scope boundary

ADR 018 delivers the request model + per-domain request codecs + the
injector + proxy wiring. It does **not** include the side-effect sink /
`Resolver` (ADR 020), `tool_use` / usage cost coverage, or
`JsonChunkCodec` (the non-streaming response-side L4 codec).
