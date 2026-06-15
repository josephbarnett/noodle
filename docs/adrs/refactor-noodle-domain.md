# Refactor — `noodle-domain` crate (NEW)

**Status:** planning. Per-crate delta for the new `noodle-domain`
crate. Companion to [`refactor-overview.md`](refactor-overview.md).

**Spec source:** ADR 029.

---

## 1. Goal

The goal of this delta is to **create** the `noodle-domain` crate
specified by ADR 029. The crate is pure types — twelve type
families across content semantics and operational context —
plus a `Classifier` trait surface and per-provider decoder
libraries that downstream consumers import.

The proxy does **not** depend on `noodle-domain`. The crate is
consumed by `noodle-viewer`, `noodle-embellish`, and any
downstream telemetry pipeline.

---

## 2. Current state

The crate does not exist. Vocabulary for content semantics is
scattered:

- `noodle-viewer` carries ad-hoc string classification in
  `crates/noodle-viewer/web/src/store/derived/ooda.ts`.
- `noodle-adapters` has codec-level types (`MarkerHit`,
  `MarkerKind`) that are wire-decoding concerns, not semantic
  vocabulary.
- No central `SpeechAct`, `ContentCategory`, `Capability`,
  `TrustLevel`, etc.

---

## 3. Target state

A new crate at `crates/noodle-domain/` with the layout pinned by
ADR 029 §7:

```
crates/noodle-domain/
├── Cargo.toml
├── src/
│   ├── lib.rs                    # public re-exports
│   │
│   │  # ─── Content families ───
│   ├── speech_act.rs
│   ├── content_category.rs
│   ├── capability.rs
│   ├── trust_level.rs
│   ├── citation_ref.rs
│   ├── reminder_subtype.rs
│   ├── task_plan.rs
│   ├── turn_end.rs
│   ├── envelope_metadata.rs
│   │
│   │  # ─── Operational-context families ───
│   ├── observation_context.rs    # AgentApp, Machine, CollectorApp
│   ├── principal_identity.rs
│   ├── usage.rs                  # TokenUsage, Latency, RetryCount
│   ├── subscription_context.rs   # ApiKeyFingerprint, OrganizationContext, SubscriptionTier
│   │
│   │  # ─── Vendor subtypes ───
│   ├── vendor/
│   │   ├── mod.rs                # VendorId, VendorTag
│   │   ├── anthropic.rs
│   │   ├── openai.rs
│   │   ├── google.rs
│   │   └── ...
│   │
│   │  # ─── Per-provider decoder libraries ───
│   ├── decoders/
│   │   ├── mod.rs                # ProviderDecoder trait surface
│   │   ├── anthropic.rs
│   │   └── ...
│   │
│   └── classifier.rs             # Classifier trait surface
└── tests/
    └── round_trip_serde.rs
```

Dependencies: `noodle-core` (identifier types), `serde`,
`serde_json`, `chrono` (for `DateTime<Utc>` fields). No async
runtime, no HTTP framework.

---

## 4. Delta items

All changes are **additive** (new crate). No modifications to
other crates in this delta — those land in companion slices.

### 4.1 Types to add (by family)

| Family | Types |
|---|---|
| `speech_act` | `SpeechAct` enum (8 canonical variants + `VendorSpecific`) |
| `content_category` | `ContentCategory` enum (11 canonical variants + `VendorSpecific`) |
| `capability` | `Capability` enum (8 canonical variants + `VendorSpecific`) |
| `trust_level` | `TrustLevel` enum (5 canonical variants + `VendorSpecific`) |
| `citation_ref` | `CitationRef` enum (5 canonical variants + `VendorSpecific`) |
| `reminder_subtype` | `ReminderSubtype` enum (6 canonical variants + `VendorSpecific`) |
| `task_plan` | `TodoItem`, `TodoStatus`, `PlanStep`, `Goal`, `Constraint` |
| `turn_end` | `TurnEnd` enum (5 canonical variants + `VendorSpecific`) |
| `envelope_metadata` | `ProviderId` enum, `Direction` enum, `RoundTripIndex` newtype, `EndpointPath` newtype |
| `observation_context` | `AgentApp { name: AgentAppName, version: Option<SemVer>, build_hash, build_date, source }`, `Machine { hostname, os_family, os_version, architecture, locale, timezone }`, `CollectorApp { name, version, build_hash, build_date, features }`. Supporting enums: `AgentAppName`, `OsFamily`, `Architecture`, `AgentAppSource`. |
| `principal_identity` | `PrincipalIdentity { device_id, machine_tag, account_role }`, `DeviceId` newtype, `AccountRole` enum |
| `usage` | `TokenUsage { input, output, cached_read, cached_creation, reasoning, vendor_extras }`, `Latency { time_to_first_byte_ms, total_ms }`, `RetryCount { attempts, last_error_kind }` |
| `subscription_context` | `ApiKeyFingerprint { prefix, kind, source }`, `ApiKeyKind` enum, `ApiKeySource` enum, `OrganizationContext { organization_id, parent_organization_id, account_type }`, `AccountType` enum, `SubscriptionTier { tier, source }`, `TierLabel` enum, `SubscriptionTierSource` enum |

### 4.2 Trait surfaces

```rust
// src/classifier.rs
pub trait Classifier: Send + Sync {
    fn classify_text(&self, text: &str, context: &ClassificationContext)
        -> ClassificationResult;
}

pub struct ClassificationContext { /* ... */ }
pub struct ClassificationResult {
    pub speech_act: Option<SpeechAct>,
    pub category: Option<ContentCategory>,
    pub citations: Vec<CitationRef>,
    pub plan_items: Vec<TodoItem>,
}

// src/decoders/mod.rs
pub trait ProviderDecoder: Send + Sync {
    fn target_provider(&self) -> ProviderId;
    fn decode_record<S: WireSource>(&self, source: &mut S)
        -> impl Iterator<Item = DecodedEvent>;
}
```

`WireSource` is imported from `noodle-core` (added in S2).

### 4.3 Vendor subtypes

Each `vendor/<name>.rs` exports `VendorTag` constants and
`VendorSpecific` payloads scoped to that vendor. `noodle-domain`
ships with Anthropic populated; other vendors as stubs (empty
modules with the file structure in place).

### 4.4 Per-provider decoders

`decoders/anthropic.rs` is the only populated decoder at S14. It
reads tap.jsonl records via `WireSource`, maps Anthropic-flavoured
fields to canonical types, surfaces vendor extras under
`Vendor­Specific`. Other providers stubbed.

---

## 5. Delivery slices

Per the overview document's slicing:

| Slice | What lands |
|---|---|
| **S0** | Empty crate stub with `Cargo.toml` and `lib.rs`. Workspace member added. `cargo build` green. |
| **S1** | All 12 type families implemented. Round-trip serde tests. No external consumers yet. |
| **S14** | `decoders/anthropic.rs` populated. `ProviderDecoder` trait implementation. Integration test consuming a captured `tap.jsonl`. |

---

## 6. Test coverage

| Test | Scope | Lives at |
|---|---|---|
| Round-trip serde per type | Every public enum / struct serialises and deserialises losslessly | `tests/round_trip_serde.rs` |
| `VendorSpecific` extensibility | Unknown vendors decode without panic; consumers' `_` arms handle | `tests/vendor_specific.rs` |
| Anthropic decoder against captures | `decoders::anthropic::decode_record` against each capture in `captures/api/` and `captures/enterprise/` produces expected events | `tests/anthropic_decoder.rs` |
| `Classifier` trait stub | Default classifier returns `None` for every classification (proves trait shape compiles) | `src/classifier.rs` inline |

---

## 7. Risks

| Risk | Mitigation |
|---|---|
| Premature type rigidity — types lock down vocabulary before downstream consumers prove what they need | Every type is open (`VendorSpecific` hatch). Promotion to first-class requires 3+ vendors (ADR 029 §3). Late-binding works. |
| Domain types accidentally pulled into `noodle-core` | Dependency-direction lint in CI: `noodle-core` must not depend on `noodle-domain`. |
| Decoder modules become noodle-adapter copies | Decoders read **decoded** records (post-codec); they're not codec implementations. The line: codec produces `NormalizedEvent`; decoder consumes `tap.jsonl` records. |

---

## 8. Out of scope

- Identity resolution (story 028; embellishment plane).
- ML classifiers — `Classifier` trait shape only; implementations are downstream.
- Classifier composition (ADR 029 §8 open question #3).
- Versioning the vocabulary formally (ADR 029 §8 open question #4).
