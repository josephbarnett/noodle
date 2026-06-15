# Refactor — `noodle-adapters`

**Status:** planning. Per-crate delta for `noodle-adapters`.
Companion to [`refactor-overview.md`](refactor-overview.md).

**Spec sources:** ADR 015 (codec/transform/detector trait shapes),
ADR 019 (per-cell dispatch), ADR 025 (provider field), ADR 027 §9
(prefix-preserving redaction), ADR 028 (per-cell marking
detectors), ADR 029 (envelope-field-producing detectors), ADR 030
(decoded-layer-producing codecs).

---

## 1. Goal

The goal of this delta is to **bring the per-cell adapters in
line** with the marks contract (ADR 028), the envelope-field
expectations (ADR 029 envelope_metadata + observation_context +
subscription_context), the decoded-layer output (ADR 030), and
the prefix-preserving redaction (ADR 027 §9).

`noodle-adapters` remains the single source of concrete
implementations against `noodle-core` trait surfaces. Per-cell
files are one-codec-one-file (already the convention).

---

## 2. Current state

Inspected at `crates/noodle-adapters/src/`:

```
codec.rs          detector.rs      dns/             filter.rs
injector.rs       lib.rs           log.rs           provider/
request/          request_detector.rs               sink.rs
sse/              store.rs         tls/             transform/
```

What's implemented today:

- Per-cell **codecs** for Anthropic and claude.ai (in `provider/`).
- **`MarkerStripTransform`** (in `transform/`).
- **`AttributionInjector`** (in `injector.rs`).
- **`UserAgentDetector`** (in `detector.rs`).
- Per-cell **marking detectors** — partial; the contract from
  ADR 028 is not yet implemented.
- **DNS rewrite transforms** (`strip_h3_alpn`, `strip_ech`).
- **TLS adapters**.
- **Side-effect sinks**.

What's missing per the ADRs:

- Marking detector contract per ADR 028 — currently the detectors
  don't read `SessionStore`; turn-id derivation is incomplete.
- Decoded-layer production — codecs produce `NormalizedEvent` but
  don't yet emit the full `DecodedContent` / `ParsedSseEvent`
  structure for the writer.
- Envelope-field-producing detectors — no `AgentAppDetector`,
  `MachineDetector`, etc.
- `ApiKeyExtractor` / `OrganizationContextExtractor` — to populate
  `envelope.subscription`.
- Redaction transform with prefix preservation (currently full
  opaque redaction).
- `provider` consumption from dispatch table.

---

## 3. Target state

Same module layout, extended:

```
crates/noodle-adapters/src/
├── codec.rs                      # unchanged
├── detector.rs                   # ← extend: new envelope-field detectors
├── detectors/                    # NEW subdirectory for organization
│   ├── agent_app.rs              # AgentAppDetector
│   ├── machine.rs                # MachineDetector
│   ├── collector_app.rs          # CollectorAppDetector (compile-time)
│   ├── api_key_fingerprint.rs    # ApiKeyExtractor
│   ├── organization_context.rs   # OrganizationContextExtractor
│   ├── user_agent.rs             # existing UserAgentDetector relocated
│   └── mod.rs
├── dns/                          # unchanged
├── filter.rs                     # unchanged
├── injector.rs                   # unchanged
├── lib.rs                        # re-exports updated
├── log.rs                        # unchanged
├── marking/                      # NEW subdirectory for per-cell marking detectors
│   ├── anthropic.rs              # AnthropicMarkingDetector (ADR 028 §5.1)
│   ├── claude_ai.rs              # ClaudeAiMarkingDetector
│   └── mod.rs
├── provider/                     # ← extend: codecs produce DecodedContent + ParsedSseEvent
├── redaction.rs                  # NEW — prefix-preserving redaction transform
├── request/                      # unchanged
├── request_detector.rs           # ← revise per ADR 028 §6
├── sink.rs                       # unchanged
├── sse/                          # ← extend: per-event parse to ParsedSseEvent
├── store.rs                      # ← align with SessionStore typed handle
├── tls/                          # unchanged
└── transform/                    # ← MarkerStripTransform emits BlockPairing on tool_use blocks
```

---

## 4. Delta items

### 4.1 Per-cell marking detectors (`marking/anthropic.rs`, `marking/claude_ai.rs`)

Implements ADR 028 §5 per-cell spec. Each detector:

1. Reads `request.headers[X-Claude-Code-Session-Id]` (Anthropic) or
   URL `conversation_uuid` (claude.ai) → `session_id`.
2. Reads `SessionStore[session_id]` at flow open.
3. Applies decision rule (ADR 028 §4.2): if last stop_reason is
   `end_turn` / `max_tokens` / absent → mint new `turn_id`; if
   `tool_use` → reuse current `turn_id`.
4. At flow close: writes back updated `SessionState` (new
   `last_stop_reason`, updated `open_spawn_stack` on `Agent`
   tool_use observation / `tool_result` matching).

```rust
pub struct AnthropicMarkingDetector;

impl RequestDetector for AnthropicMarkingDetector {
    fn detect(
        &self,
        request: &RequestProbe,
        session_store: &dyn SessionStore,
    ) -> MarkOutput {
        let session_id = extract_session_id(request);
        let state = session_store.read(&session_id);
        // ... decision rule ...
        MarkOutput { session_id, turn_id, parent_session_id, .. }
    }
}
```

A response-side transform observes `message_delta.stop_reason`
and `Agent` tool_use blocks during the stream, then commits the
updated state via the `SessionStore` write at flow close.

### 4.2 Envelope-field-producing detectors (`detectors/`)

| Detector | Reads | Produces |
|---|---|---|
| `AgentAppDetector` | `User-Agent` header, `X-Stainless-*` headers, `X-App` header | `AgentApp` envelope field |
| `MachineDetector` | `X-Stainless-Os`, `X-Stainless-Arch`, configured host inputs | `Machine` envelope field |
| `CollectorAppDetector` | compile-time env (`CARGO_PKG_VERSION`, `BUILD_HASH`, `BUILD_DATE`) | `CollectorApp` envelope field — same on every record |
| `ApiKeyExtractor` | `Authorization`, `X-Api-Key`, `Anthropic-Api-Key` headers (post-redaction; reads the preserved prefix) | `ApiKeyFingerprint` envelope subfield |
| `OrganizationContextExtractor` | URL path (`claude.ai/api/organizations/{uuid}/...`), `Anthropic-Organization-Id` response header | `OrganizationContext` envelope subfield |

Detectors are independent. Each runs in its own chain position
per the dispatch table. Existing `UserAgentDetector` is
generalised into `AgentAppDetector` (produces the richer
`AgentApp` struct).

### 4.3 Prefix-preserving redaction (`redaction.rs`)

A new transform implementing ADR 027 §9. Surface:

```rust
pub struct PrefixPreservingRedaction {
    pub per_header: BTreeMap<String, u32>,    // N per header; 0 = full opaque
    pub redaction_marker: String,             // default "<redacted>"
    pub default_n: u32,                       // default 12
}

impl Transform<RequestProbe> for PrefixPreservingRedaction {
    fn apply(&self, probe: &mut RequestProbe) {
        for header in probe.headers_mut() {
            if let Some(&n) = self.per_header.get(header.name) {
                header.value = redact_with_prefix(&header.value, n, &self.redaction_marker);
            }
        }
    }
}
```

Replaces or supersedes the existing full-opaque redaction. The
preserved prefix is what `ApiKeyExtractor` reads downstream.

### 4.4 Codec extension — decoded layer production (`provider/`)

The codec layer already parses content blocks during L4–L5
decode (ADR 015). The new requirement: emit the parsed structure
into `DecodedContent` and `ParsedSseEvent` types defined in
`noodle-core` (S9, S10).

Specifically:
- `provider/anthropic.rs` request codec: parse `messages[].content[]`
  into `DecodedContent` for the request record.
- `provider/anthropic.rs` response codec: emit each SSE event as a
  `ParsedSseEvent` AND accumulate `DecodedContent.blocks` for the
  response record.
- `provider/claude_ai.rs`: same shape, vendor-specific decoding.

Codecs already do this work internally; the change is exposing
it to the writer.

### 4.5 Tool-use pairing transform

A new transform (or addition to existing response transforms)
that maintains the back-patch table (ADR 030 §4.3):

```rust
pub struct ToolUsePairingTracker {
    pending: BTreeMap<(SessionId, String /*tool_use_id*/), Ulid /*request_id*/>,
}

// On response: for each tool_use block, store (session_id, tool_use_id) → request_id
// On request: for each tool_result block, look up the pair; produce a patch event
```

Patches are emitted as side-effect records the writer applies
(ADR 030 §7.3).

### 4.6 `request_detector.rs` revision

The cell-anchoring layer (`request_detector.rs`) updates to:
- Pass `&dyn SessionStore` to each detector.
- Compose multiple detectors (marking + envelope-field + extractors)
  into the chain order the dispatch table declares.

### 4.7 `store.rs` alignment

The existing `store.rs` aligns with the `noodle-core::SessionStore`
trait. The default impl is in-memory with TTL; concrete impl lives
here.

---

## 5. Delivery slices

| Slice | What lands in `noodle-adapters` |
|---|---|
| **S3** | `marking/anthropic.rs`, `marking/claude_ai.rs` — per-cell marking detectors per ADR 028. `request_detector.rs` revised. `store.rs` aligned. |
| **S4** | `provider` field consumed from dispatch and stamped onto records. (Mostly proxy-side; this crate just exposes the field on its outputs.) |
| **S5** | `redaction.rs` — prefix-preserving redaction transform. Default-table cell config. |
| **S6** | `detectors/agent_app.rs`, `detectors/machine.rs`, `detectors/collector_app.rs`. Existing `UserAgentDetector` generalised. |
| **S7** | `detectors/api_key_fingerprint.rs`, `detectors/organization_context.rs`. |
| **S8** | Response-side codec extension emitting `usage.tokens` from `message_delta.usage`. |
| **S9** | Codec L5 extension emitting `DecodedContent.blocks[]` to the writer. |
| **S10** | SSE codec emitting `ParsedSseEvent` list to the writer. |
| **S11** | Tool-use pairing transform with back-patch table; patch-event emission. |

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Marking detector decision rule per cell | Apply each row of ADR 028 §4 decision table; assert correct `turn_id` minted / reused | `marking/*.rs` inline + `tests/marking_e2e.rs` |
| Capture-driven marking | Replay `captures/enterprise/claude-code-cli-api.mitm`; assert `turn_id` stays stable across 8 RTs of one main agent turn | `tests/marking_capture_replay.rs` |
| Prefix-preserving redaction | Input `sk-ant-api03-wcq...pQAA` → output `sk-ant-api03-wcq…<redacted>` at N=12 | `redaction.rs` inline |
| Envelope-field detectors | Each detector against captured headers produces expected typed output | `detectors/*.rs` inline + `tests/envelope_capture.rs` |
| Tool-use pairing | Adjacent response (`tool_use_id = X`) and next request (`tool_result.tool_use_id = X`) produce paired records | `tests/tool_use_pairing.rs` |
| Codec round-trip with decoded layer | `encode(decode(bytes)) == bytes` for unmutated input (ADR 015 §2.1.1) | `provider/*.rs` inline |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Marking detector regression (silently wrong `turn_id`) | Capture-driven replay test asserting on the canonical 8-RT session in `claude-code-cli-api.mitm`. Any regression flags. |
| Redaction breaks billing reconciliation if N=12 isn't enough | Operator-configurable per-header. Default tracks the telemetry backend's existing format (`sk-ant-aaaa`). |
| `Agent` tool_use lineage tracking has edge cases | First implementation handles the common case (push on tool_use, pop on tool_result); ADR 028 §10 deferred for edge cases. Sub-agent stack is per-session. |
| Codec extension changes wire-observable bytes | Empty-on-error contract (ADR 015 §16) preserved. Codec round-trip property test catches regressions. |
| Concurrent `SessionStore` access from multiple chain positions | `SessionStore` is `Send + Sync`. Single-writer per session at flow close. |

---

## 8. Out of scope

- New providers (OpenAI, Google Gemini codecs) — separate feature work.
- Header rewriting / body model rewriting (Watchtower bundle — separate ADR).
- Cell-level config for `omit_content` (ADR 031 §8 open question #5 — deferred).
- Persistent session store across proxy restarts (ADR 028 §10 #1 — deferred).
