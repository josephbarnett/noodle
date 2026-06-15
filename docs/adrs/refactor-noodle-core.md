# Refactor — `noodle-core`

**Status:** planning. Per-crate delta for `noodle-core`. Companion
to [`refactor-overview.md`](refactor-overview.md).

**Spec sources:** ADR 015 (layered codec, trait shapes), ADR 027
§2.1 (`WireSink` / `WireSource` duality), ADR 028 (SessionStore +
revised `RequestDetector`), ADR 030 (decoded layer fields).

---

## 1. Goal

The goal of this delta is to **extend** `noodle-core` with the
typed surfaces required by the ADRs without breaking the crate's
protocol-pure shape. Specifically: add `WireSource`, add
`SessionStore`, revise the `RequestDetector` contract, and refine
record-type fields to carry the decoded-layer additions.

`noodle-core` remains the foundational crate — pure traits and
types, no I/O, no async runtime, no HTTP framework, no
`noodle-domain` dependency.

---

## 2. Current state

Inspected at `crates/noodle-core/src/`:

```
audit.rs        codec.rs        detector.rs     endpoint.rs
engine.rs       event.rs        filter.rs       injector.rs
layered/        layered.rs      lib.rs          marker.rs
probe.rs        request.rs      resolver.rs     session.rs
store.rs        stream.rs       wire.rs
```

What's implemented today:

- `Codec` and `Transform` traits (ADR 015 §3) — present.
- `RequestDetector` (ADR 021) — present, currently stateless per
  the un-revised ADR 021.
- `NormalizedEvent` and supporting event types — present, but
  with the `TurnId` naming collision flagged in ADR 028 §7.
- `WireSink` trait (or equivalent) — present.
- `SessionStore` — present in some form (`store.rs`, `session.rs`)
  but not aligned with the typed-handle contract in ADR 028 §3.

What's missing per the ADRs:

- `WireSource` trait surface (ADR 027 §2.1).
- The typed `SessionStore` handle passed to detectors and
  transforms (ADR 015 §12 #4, refined by ADR 028 §3).
- `RoundTripId` type — currently `TurnId` carries
  `payload.message.id` (per-round-trip), which conflicts with
  the user-intent `TurnId` minted by the marking detector
  (ADR 028 §7).
- Record envelope types for the new envelope fields
  (`provider`, `agent_app`, `machine`, `collector_app`,
  `principal`, `subscription`, `usage`).
- Decoded-layer event/record types (`content.blocks[]`,
  `events[]`, `pairing` references).

---

## 3. Target state

The same modules, extended. No removals planned — additive
extension only, with one rename (`TurnId` → `RoundTripId` per
ADR 028 §7) accompanied by a new `TurnId` for the user-intent id.

```
crates/noodle-core/src/
├── audit.rs                  # ← extend: AuditEvent / AuditKind catalog (open)
├── codec.rs                  # unchanged
├── detector.rs               # ← revise: RequestDetector takes typed SessionStore handle
├── endpoint.rs               # unchanged
├── engine.rs                 # ← extend: WireSource registration
├── event.rs                  # ← revise: TurnId rename + new TurnId; record envelope fields
├── filter.rs                 # unchanged
├── injector.rs               # unchanged
├── layered/, layered.rs      # unchanged (codec layers stay)
├── lib.rs                    # public re-exports updated
├── marker.rs                 # unchanged
├── probe.rs                  # unchanged
├── request.rs                # ← extend: decoded request-content fields
├── resolver.rs               # unchanged
├── session.rs                # ← revise: SessionStore typed handle
├── store.rs                  # ← revise: SessionStore impl per ADR 028 §3
├── stream.rs                 # ← extend: parsed-event list type
└── wire.rs                   # ← extend: WireSource trait
```

---

## 4. Delta items

### 4.1 New trait: `WireSource` (`wire.rs`)

```rust
pub trait WireSource: Send {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Yields the next record from the source, blocking until one
    /// arrives. Returns Ok(None) at EOF (batch mode) or never
    /// returns Ok(None) (tail mode).
    fn next_record(&mut self) -> Result<Option<TapRecord>, Self::Error>;

    /// Optional: rewind to a known offset (file-backed sources).
    fn seek(&mut self, offset: u64) -> Result<(), Self::Error> {
        Err(/* unsupported by default */)
    }
}
```

The implementations live in `noodle-tap`; this is the trait surface.

### 4.2 Rename and add: `TurnId` / `RoundTripId` (`event.rs`)

```rust
// existing TurnId carries payload.message.id — rename
pub struct RoundTripId(pub String);

// new TurnId is the proxy-minted user-intent identifier
pub struct TurnId(pub Ulid);
```

`NormalizedEvent::TurnStart { turn_id, role }` and `TurnEnd { turn_id, finish }`
rename their `turn_id` field to `round_trip_id: RoundTripId`. The
new `turn_id: TurnId` lives on the record's marks block (added in
S3, not in `NormalizedEvent`).

### 4.3 Revise: `SessionStore` (`store.rs`, `session.rs`)

Per ADR 028 §3. The store's public surface:

```rust
pub trait SessionStore: Send + Sync {
    /// Read the current session state at flow open.
    fn read(&self, session_id: &SessionId) -> Option<SessionState>;

    /// Write the updated session state at flow close.
    fn write(&self, session_id: &SessionId, state: SessionState);

    /// Evict expired sessions per TTL policy.
    fn evict(&self);
}

pub struct SessionState {
    pub current_turn_id: TurnId,
    pub last_stop_reason: Option<StopReason>,
    pub open_spawn_stack: Vec<SpawnEntry>,
    pub last_seen: Instant,
    // ... per ADR 028 §3
}

pub struct SpawnEntry {
    pub spawn_tool_use_id: String,
    pub parent_turn_id: TurnId,
}
```

The proxy passes `SessionStore` as a typed handle (`TransformAttachment`,
ADR 015 §12 #4) to `RequestDetector` and transforms.

### 4.4 Revise: `RequestDetector` (`detector.rs`)

Per ADR 028 §6:

```rust
pub trait RequestDetector: Send + Sync {
    /// Runs at flow open. Reads the request envelope and the
    /// SessionStore. Produces the marks block.
    fn detect(
        &self,
        request: &RequestProbe,
        session_store: &dyn SessionStore,
    ) -> MarkOutput;
}

pub struct MarkOutput {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub parent_session_id: Option<SessionId>,
    pub spawn_tool_use_id: Option<String>,
    pub per_cell_fields: BTreeMap<String, serde_json::Value>,
}
```

`detect` becomes the marking-detector entry point. Existing
`RequestDetector` impls are updated to take the `session_store`
parameter; many will ignore it (cells that don't need cross-request
state).

### 4.5 Extend: record envelope types (`event.rs`, `request.rs`)

The record envelope as ADR 030 §1 specifies:

```rust
pub struct RecordEnvelope {
    pub request_id: Ulid,
    pub direction: Direction,
    pub ts_unix_ms: u64,
    pub provider: ProviderId,        // S4
    pub domain: String,
    pub endpoint: String,
    pub headers: Vec<HeaderPair>,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub parent_session_id: Option<SessionId>,
    pub agent_app: Option<AgentAppRef>,        // S6 — opaque from noodle-core; noodle-domain types
    pub machine: Option<MachineRef>,           // S6
    pub collector_app: CollectorAppRef,        // S6 — always present
    pub principal: Option<PrincipalRef>,       // S6
    pub subscription: Option<SubscriptionRef>, // S7
    pub usage: Option<UsageRef>,               // S8
    pub schema_version: u32,
}
```

`*Ref` types are `serde_json::Value` from `noodle-core`'s perspective
(it doesn't depend on `noodle-domain`). Consumers that have
`noodle-domain` in scope deserialize them into typed structs.

### 4.6 Extend: decoded content fields (`event.rs`, `stream.rs`)

```rust
pub struct DecodedContent {
    pub schema_version: u32,
    pub blocks: Vec<ContentBlock>,
}

pub struct ContentBlock {
    pub kind: ContentBlockKind,
    pub text: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub input: Option<serde_json::Value>,
    pub is_error: Option<bool>,
    pub content: Option<Vec<ContentBlock>>,
    pub annotations: serde_json::Value,   // domain-typed at the noodle-domain layer
    pub pairing: Option<BlockPairing>,
}

pub enum ContentBlockKind {
    Text, ToolUse, ToolResult, Thinking, Image, SystemReminder, Redacted, VendorSpecific(String),
}

pub struct BlockPairing {
    pub resolved_by_request_id: Option<Ulid>,
    pub resolves_tool_use_in_request_id: Option<Ulid>,
}

pub struct ParsedSseEvent {
    pub ts_offset_ms: u64,
    pub event_type: SseEventType,
    pub payload: serde_json::Value,
}
```

### 4.7 Extend: `engine.rs` for `WireSource` registration

The engine currently constructs `WireSink`s; add a parallel
registration mechanism for `WireSource`s (used by tools, tests,
and the embellishment processor). Trait-only addition — the
proxy doesn't instantiate a `WireSource`.

---

## 5. Delivery slices

| Slice | What lands in `noodle-core` |
|---|---|
| **S2** | `WireSource` trait surface (`wire.rs`). No implementation. |
| **S3** | `SessionStore` revision (`store.rs`, `session.rs`); `RequestDetector` revision (`detector.rs`); `TurnId` / `RoundTripId` rename (`event.rs`). |
| **S4** | `RecordEnvelope::provider` field (`event.rs`). |
| **S6** | `agent_app`, `machine`, `collector_app`, `principal` envelope fields. |
| **S7** | `subscription` envelope field. |
| **S8** | `usage` envelope field; `TokenUsage`-equivalent newtype kept simple (the rich struct lives in `noodle-domain`). |
| **S9** | `DecodedContent` + `ContentBlock` + `ContentBlockKind` (`event.rs`). |
| **S10** | `ParsedSseEvent` + `SseEventType` (`stream.rs`). |
| **S11** | `BlockPairing` (added to `ContentBlock`). |

S3 is the largest single change to this crate; the others are
small additive extensions.

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| `WireSource` trait shape compiles with mock impl | Trait object-safety, error type bounds | `src/wire.rs` inline |
| `SessionStore` read/write roundtrip | In-memory impl roundtrips state | `src/store.rs` inline |
| `RequestDetector` mock receiving `SessionStore` | Trait shape compiles; mock detect produces expected marks | `src/detector.rs` inline |
| `TurnId` / `RoundTripId` rename — compile fence | Existing call sites updated; rename is mechanical | compiler |
| `RecordEnvelope` serde roundtrip | Every new field round-trips through JSON | `src/event.rs` inline |
| `DecodedContent` serde roundtrip | Same | `src/event.rs` inline |
| `ParsedSseEvent` serde roundtrip | Same | `src/stream.rs` inline |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| `TurnId` rename touches every adapter and test | Rename is mechanical and compiler-checked. Land S3 in one commit with all call-site updates; no partial states. |
| `SessionStore` change breaks existing detectors | `RequestDetector::detect` signature changes; every existing impl updates in S3. Existing impls that don't need cross-request state ignore the new parameter. |
| Envelope-field churn (S4–S8) breaks `tap.jsonl` schema | Every new field is optional (`Option<...>`) at this layer. Required-ness can be tightened later. |
| `noodle-domain` types accidentally pulled into `noodle-core` | `*Ref` types are `serde_json::Value` — `noodle-core` stays decoupled. The richer struct lives in `noodle-domain`; consumers deserialize there. |

---

## 8. Out of scope

- Async-trait revision for `RequestDetector` (deferred per ADR 015 §12).
- `WireSource` concrete implementations (live in `noodle-tap`).
- `SessionStore` persistence (cold-cache recovery deferred per ADR 028 §10).
- Removing the old `TurnId` (the new `RoundTripId` carries the old semantics; no removal).
