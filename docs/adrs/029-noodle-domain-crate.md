# ADR 029 — `noodle-domain` crate specification

**Status:** current. Specifies the boundary, type-family organisation,
and extensibility model of the `noodle-domain` crate.

**Resolves:** the gap flagged in `doc-gaps-status.md` §2 — ADR 001
§3.2 and `coverage-roadmap.md` name the crate and the vocabulary
source, but no ADR pins the crate's shape, the type families it
contains, or how new types are added. Without this, every downstream
consumer of `noodle-domain` depends on an unspecified contract, and
the `tap.jsonl` schema (next deliverable) cannot reference typed
fields with confidence.

**Related:** ADR 001 §3.2 (`noodle-domain` shape), ADR 001 §3.6
(viewer depends on `noodle-domain`), ADR 027 (`tap.jsonl` boundary —
the primary consumer of these types), ADR 028 §1 (per-cell wire
facts that domain types annotate), `docs/features/agent-protocol-coverage-roadmap.md`
(provider / client priority and corpus methodology).

---

## Goal

The goal of this ADR is to specify the `noodle-domain` crate so that:

1. Downstream consumers (`noodle-viewer`, telemetry shippers,
   evaluators, security tooling, the embellishment plane) read
   typed fields from `tap.jsonl` and reason about both **agent
   content** (speech act, content category, capability, citation,
   plan items, turn termination) **and agent operational context**
   (which agent app is in the field, on which host, observed by
   which collector build, against which provider, with what token
   usage and operator identity) — without re-parsing wire data.
2. The vocabulary is **observed**, not invented: content categories
   trace to cross-vendor patterns in the public system-prompt
   corpus (`coverage-roadmap.md` §4); operational-context types
   trace to facts the proxy already knows at observation time.
3. The crate has a single owner of meaning: when "what kind of
   thing is this content?" *or* "what was the operational context
   of this observation?" is asked anywhere downstream of the
   proxy, the answer is a `noodle-domain` type.

### Why

`tap.jsonl` (ADR 027) is the evidence boundary. Records on it carry
both an HTTP projection (request line, headers, body) and an OODA
projection (decoded content blocks, tool-use lineage, turn
membership). The OODA projection is only useful if the content
blocks are **typed at the semantic level** — knowing a block is
text is not enough; consumers need to know whether the text is a
speech act of `Instruction`, `Claim`, or `Question`, whether the
content category is `Code` or `Credential`, whether the embedded
action is a `Read` or an `Execute` capability invocation.

Today this vocabulary is implicit, scattered across ADRs and code.
Without a single ratifying ADR, three problems compound:

1. **Drift.** Each consumer reinvents the classification with
   slight variations. The viewer's `ooda.ts` uses one taxonomy;
   the telemetry shipper would use another. They disagree on
   edge cases.
2. **Coupling.** Without a crate, classification logic lives in the
   consumer. A new consumer adds its own version. There is no
   single place to fix a misclassification.
3. **Schema fragility.** `tap.jsonl` cannot pin field types without
   a stable type vocabulary. Field names drift, downstream parsers
   break.

`noodle-domain` solves all three by being the single crate that
owns the semantic vocabulary. Every consumer reads the same types.
A new vendor is added by extending the crate in one place.

### What this ADR specifies

1. The crate's **boundary** (§1) — what's in, what's out, what it
   depends on.
2. The **type-family taxonomy** (§2) — nine families that organise
   every type in the crate.
3. The **recurrence rule** (§3) — how a candidate type is promoted
   from observed-once to first-class.
4. The **extensibility model** (§4) — open enums, vendor subtypes,
   how new types are added.
5. The **connection to `noodle-core`** (§5) — what `noodle-core`
   knows about `noodle-domain` (nothing) and what carries domain
   types across the boundary (`tap.jsonl` records, not event
   structs).
6. The **connection to `tap.jsonl`** (§6) — which families surface
   on which record fields. Forward-references the next ADR.
7. **Per-provider decoder libraries** (§7) — reusable per-provider
   modules every `tap.jsonl` consumer imports rather than
   reinventing. Anchored by `ProviderId` (an `envelope_metadata`
   type) carried as a first-class field on every record.

### Non-goals

- **Identity resolution.** Turning `device_id` / `session_id` into
  "this person on this team" is the embellishment plane (story
  028). `noodle-domain` types describe content, not actors.
- **Wire codec types.** SSE event shapes, content-block grammars,
  framing — all of this is `noodle-core` and `noodle-adapters`.
  `noodle-domain` annotates the *decoded* content, not the wire
  encoding.
- **Policy.** "Should this content be blocked / rewritten / audited?"
  is a policy concern. `noodle-domain` types are the inputs to
  policy, not policy itself.
- **Implementation details.** Exact `enum` discriminants, JSON
  serialization specifics, derive macros. Code review-time concerns
  governed by the type families specified here.

---

## 1. Crate boundary

`noodle-domain` is a **pure type crate**. The boundary is:

| In | Out |
|---|---|
| Enum definitions, struct definitions, type aliases | Any I/O (file, network, time) |
| Classification trait surfaces (e.g. `Classifier`) | Trait implementations that need I/O |
| `serde` derives for serialization | `tokio`, `rama`, async runtime types |
| Lookup tables, vocabulary catalogues (vendor-specific tag tables) | Side-effects, mutation, state |
| Documentation of where each type originates in the corpus | Wire-decoding logic (lives in `noodle-adapters`) |

Dependencies:

- `noodle-core` — for `RoundTripId`, `TurnId`, and other identifier
  types that domain records carry.
- `serde`, `serde_json` — serialization.
- No other workspace crates. No async runtime. No HTTP framework.

Consumers:

- `noodle-viewer` (reads `tap.jsonl`, classifies for display).
- `noodle-tap` does **not** depend on `noodle-domain` — it writes
  records whose domain-typed fields are populated upstream by
  classifiers that themselves consume the crate.
- The embellishment plane and any downstream telemetry shippers.

The proxy (`noodle-proxy`) does not depend on `noodle-domain`.
Classification is a downstream concern.

---

## 2. Type-family taxonomy

`noodle-domain` organises every type under one of twelve families.
Nine cover **agent content** (what the bytes mean); three cover
**operational context** (where, what, and how observed). This
section pins their canonical names and what each contains.

### 2.1 Content families

| # | Family | What it classifies | Example types |
|---|---|---|---|
| 1 | **`speech_act`** | The pragmatic intent of a text block | `Instruction`, `Claim`, `HedgedClaim`, `Question`, `Suggestion`, `Acknowledgement`, `Refusal`, `Clarification` |
| 2 | **`content_category`** | What the bytes of a content block contain | `Code`, `Command`, `Credential`, `Pii`, `Secret`, `Prose`, `StructuredData`, `Path`, `Url`, `Reasoning`, `Plan` |
| 3 | **`capability`** | The kind of action a tool call performs | `ReadFile`, `WriteFile`, `Execute`, `NetworkRequest`, `NetworkListen`, `SpawnAgent`, `SystemQuery`, `EnvironmentRead` |
| 4 | **`trust_level`** | How much the harness trusts the source of a content block | `SystemTrusted` (host program), `UserTrusted` (human), `ModelOutput` (assistant), `ToolOutput` (external), `InjectedReminder` (auto-injected) |
| 5 | **`citation_ref`** | References to external sources / files / URLs the content cites | `FilePath`, `UrlReference`, `LineRange`, `CommitHash`, `IssueRef` |
| 6 | **`reminder_subtype`** | The kind of system / system-reminder injection | `SkillCatalogue`, `ToolAvailability`, `ContextRefresh`, `WorkingDirState`, `SafetyClassifier` (server-side), `LongConversation` (server-side) |
| 7 | **`task_plan`** | Primitives the agent's planning channel emits | `TodoItem`, `TodoStatus`, `PlanStep`, `Goal`, `Constraint` |
| 8 | **`turn_end`** | Wire-level turn-termination signals normalised across vendors | `EndTurn`, `MaxTokens`, `ToolUsePending`, `StopSequence`, `ContentFiltered` |
| 9 | **`envelope_metadata`** | Per-record dispatch facts indexed on by every consumer | `ProviderId`, `EndpointPath`, `Direction`, `RoundTripIndex` |

### 2.2 Operational-context families

| # | Family | What it carries | Example types |
|---|---|---|---|
| 10 | **`observation_context`** | The operational picture of where and by what this round-trip was observed | `AgentApp` (the agent harness in the field), `Machine` (the host the agent ran on), `CollectorApp` (the noodle build that observed the round-trip) |
| 11 | **`principal_identity`** | Non-PII identifiers for the actor / device / role context | `DeviceId`, `MachineTag`, `AccountRole`, `WorkstationName` |
| 12 | **`usage`** | Vendor-emitted quantitative facts about a round-trip | `TokenUsage` (with vendor-extras hatch), `Latency`, `RetryCount` |
| 13 | **`subscription_context`** | Identifiers that let downstream consumers reconcile observed traffic against billed traffic | `ApiKeyFingerprint`, `OrganizationContext`, `SubscriptionTier` |

Every type in the crate belongs to exactly one family. Cross-family
relationships (e.g. a `ToolCall` carries a `capability` and may
also produce content with a `content_category`) are expressed by
composition in the record structs that aggregate them, not by
overloading the family taxonomy.

### 2.3 Family vs subtype

Each family is an **open enum** with a fixed canonical-case set
plus a `VendorSpecific(VendorTag)` variant for single-vendor
subtypes. The recurrence rule (§3) determines which case a new
classification lands in.

### 2.4 Concrete struct shapes for operational-context families

The content families are typically open enums with simple variants.
The three operational-context families carry **structs** with
multi-field payloads. The canonical shapes:

```rust
// Family 10 — observation_context

pub struct AgentApp {
    pub name: AgentAppName,              // open enum:
                                         //   ClaudeCode | OpenCode | Cursor |
                                         //   ChatGptDesktop | ClaudeDesktop |
                                         //   CodexCli | Warp | Zed |
                                         //   VendorSpecific(String) | Unknown
    pub version: Option<SemVer>,
    pub build_hash: Option<String>,
    pub build_date: Option<DateTime<Utc>>,
    pub source: AgentAppSource,          // how the proxy learned this:
                                         //   UserAgentHeader | BillingHeader |
                                         //   InferredFromPath | Unknown
}

pub struct Machine {
    pub hostname: Option<String>,        // operator chooses whether to include
    pub os_family: OsFamily,             // Macos | Linux | Windows | Unknown
    pub os_version: Option<String>,
    pub architecture: Architecture,      // X86_64 | Aarch64 | Unknown
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

pub struct CollectorApp {
    pub name: &'static str,              // always "noodle"
    pub version: SemVer,                 // compile-time embedded
    pub build_hash: &'static str,        // compile-time embedded
    pub build_date: DateTime<Utc>,       // compile-time embedded
    pub features: Vec<&'static str>,     // cargo features active in this build
}

// Family 11 — principal_identity

pub struct PrincipalIdentity {
    pub device_id: Option<DeviceId>,     // stable opaque id; not PII
    pub machine_tag: Option<String>,     // operator-assigned label
    pub account_role: Option<AccountRole>, // Admin | StandardUser |
                                           // ServiceAccount | Unknown
}

// Family 12 — usage

pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached_read: Option<u64>,
    pub cached_creation: Option<u64>,
    pub reasoning: Option<u64>,          // o-series / thinking-token vendors
    pub vendor_extras: BTreeMap<String, serde_json::Value>,
}

pub struct Latency {
    pub time_to_first_byte_ms: Option<u64>,
    pub total_ms: Option<u64>,
}

pub struct RetryCount {
    pub attempts: u32,
    pub last_error_kind: Option<String>,
}

// Family 13 — subscription_context

pub struct ApiKeyFingerprint {
    pub prefix: String,                   // operator-configured visible length
                                          // default 12 chars: "sk-ant-aaaa" / "sk-ant-sid02"
    pub kind: ApiKeyKind,                 // ApiKey | Session | Oauth | Unknown
    pub source: ApiKeySource,             // AuthorizationHeader | XApiKey |
                                          // SessionCookie | UrlParam
}

pub enum ApiKeyKind {                     // the telemetry backend's api_key_type semantics
    ApiKey,                               // long-lived API key (sk-ant-api03-*, sk-*)
    Session,                              // session token (sk-ant-sid02-*)
    Oauth,                                // OAuth bearer token
    Unknown,
}

pub struct OrganizationContext {
    pub organization_id: Option<String>,        // wire-observable in URL / header
    pub parent_organization_id: Option<String>, // hierarchical accounts
    pub account_type: AccountType,
}

pub enum AccountType {                    // the telemetry backend's organization_type
    Enterprise,
    Personal,
    Api,
    Team,
    Free,
    Pro,
    Other(String),
    Unknown,
    VendorSpecific(String),
}

pub struct SubscriptionTier {
    pub tier: Option<TierLabel>,
    pub source: SubscriptionTierSource,   // Header | UrlPath | ResponseMetadata |
                                          // EmbellishmentPlane | Unknown
}

pub enum TierLabel {
    Free,
    Pro,
    Team,
    Enterprise,
    Custom(String),
    Unknown,
}
```

The structs are open for additive extension under the same rules
as enums (§4): new optional fields are allowed without a version
bump; renames and removals are forbidden.

**One fingerprint, two slots.** `api_key_prefix` and
`session_key_prefix` in the telemetry backend's `ai-telemetry` schema are the
same value placed in two locations for self-interpretability. In
`noodle-domain` they're one `ApiKeyFingerprint`; the embellishment
plane is responsible for fanning it out to whichever target-schema
slots the consumer expects. The `kind` field discriminates between
key types so the embellishment plane can populate
`api_key_type = "session"` correctly without re-parsing the prefix
string.

**Wire-observable vs enrichment-plane.** Of the four
`subscription_context` types, only `ApiKeyFingerprint` is fully
wire-observable on every round-trip (extracted from the
`Authorization` / `X-Api-Key` header by the proxy's redaction
pass — see ADR 027 §9). `OrganizationContext.organization_id` is
sometimes wire-observable (URL path on `claude.ai`,
`Anthropic-Organization-Id` response header on `api.anthropic.com`).
`account_type` and `SubscriptionTier` typically require
embellishment-plane enrichment from out-of-band sources (Console
API, IT-provisioned config).

### 2.5 The PII boundary

`principal_identity` is deliberately bounded to non-PII identifiers
(device tags, machine tags, role tags). Resolving these to specific
humans, teams, or organisations is the **embellishment plane** (ADR
001; story 028). `noodle-domain` provides the keys the embellishment
plane uses to look up identity, but never the identity itself.

A consumer that wants "this round-trip came from Joe's laptop"
joins `PrincipalIdentity.device_id` against an embellishment-plane
directory; it does not read PII from `tap.jsonl`.

### 2.6 Why operational-context types live in `noodle-domain`

These types are not classification outputs — they are facts the
proxy knows at observation time (build hash, host OS, agent app
user-agent). They live in `noodle-domain` rather than `noodle-core`
because:

- Consumers (`noodle-viewer`, telemetry shippers, evaluators) need
  the same vocabulary as for content typing. One crate, one
  vocabulary.
- The vendor / agent / OS namespaces are open (new agent apps
  arrive). `noodle-domain`'s extensibility model (§4) handles open
  enums; `noodle-core` cannot.
- The proxy itself does not need these types at runtime — it
  populates them during write but doesn't reason about them.
  Keeping them out of `noodle-core` preserves the proxy's
  protocol-pure shape.

---

## 3. The cross-vendor recurrence rule

ADR 001 §3.2 states the rule informally; this ADR makes it the
formal admission criterion for first-class status:

> A type is **first-class** in `noodle-domain` when the underlying
> pattern is documented in the system-prompt corpus for **three or
> more vendors** at the time of admission. Patterns observed in one
> or two vendors are admitted as **vendor-specific subtypes** under
> the relevant family's `VendorSpecific` variant, with a vendor tag.

| Recurrence | Admission |
|---|---|
| 3+ vendors | First-class variant on the family enum. Stable name. |
| 2 vendors | Vendor-specific subtype under each vendor's tag. Re-evaluated when a third vendor surfaces the same pattern. |
| 1 vendor | Vendor-specific subtype under that vendor's tag. No promotion expected. |

Recurrence is judged from the corpus survey (`coverage-roadmap.md`
§4). When a new vendor is added to the corpus, every two-vendor
pattern is re-evaluated and may be promoted.

### 3.1 Vendor-specific subtype shape

```rust
pub enum SpeechAct {
    Instruction,
    Claim,
    HedgedClaim,
    Question,
    // ... canonical cases ...
    VendorSpecific(VendorSpeechAct),
}

pub struct VendorSpeechAct {
    pub vendor: VendorId,
    pub tag: String,           // vendor's own term, verbatim
    pub closest_canonical: Option<&'static str>, // best-effort mapping for consumers that don't know the vendor
}
```

The `closest_canonical` field is a hint, not a guarantee. A
consumer that only understands canonical cases falls back on it; a
consumer that knows the vendor reads `tag` directly.

---

## 4. Extensibility model

`noodle-domain` grows. The model:

1. **New canonical case (first-class promotion).** Requires
   corpus evidence for three or more vendors. Adds an enum variant
   in the relevant family. Backward-compatible (additive on an open
   enum) for consumers that pattern-match exhaustively only over
   `match … { _ => …, … }` — which is the documented requirement
   for consumers.
2. **New vendor-specific subtype.** Add a `VendorTag` constant and
   a vendor entry under the family's `VendorSpecific` carrier.
   Free addition — no consumer code changes.
3. **New family.** Requires an ADR. The twelve families in §2
   are not assumed permanent, but a new family is a significant
   architectural addition (every consumer learns it).
4. **Renames.** Forbidden in published versions. The crate's enum
   names and variants are stable identifiers downstream consumers
   key on.

### 4.1 Consumer obligation

Every consumer that pattern-matches on a `noodle-domain` enum
**must** include a `_` arm that handles unknown variants
gracefully (typically: pass through as opaque text, log a
"new-variant" hint, do not crash). The crate ships with a lint
example showing the required shape.

---

## 5. Connection to `noodle-core`

`noodle-core` does **not** depend on `noodle-domain`. The proxy
runs without domain classification.

The relationship in the other direction:

- `noodle-domain` re-exports a small set of identifier types from
  `noodle-core` (`SessionId`, `TurnId`, `RoundTripId`,
  `ToolUseId`) so domain records that reference round-trips and
  turns carry the same identifier types the proxy uses.
- `noodle-core::NormalizedEvent` is **not** parameterised by
  domain types. The proxy emits semantically untyped events;
  classification happens downstream of the proxy, against
  `tap.jsonl`.

This separation is the keystone of the hexagonal layering: the
proxy is protocol-pure; semantic meaning is added by readers of
its output. If domain types appeared on `NormalizedEvent`, the
proxy would have to choose a classifier at compile time, which
defeats the swappable-classifier shape.

---

## 6. Connection to `tap.jsonl`

This ADR pins the type vocabulary. ADR 030 (next, not yet written)
will pin the `tap.jsonl` schema and which fields carry which
domain-typed values. Forward-references:

| `tap.jsonl` field | Domain family | Notes |
|---|---|---|
| `content.blocks[*].speech_act` | `speech_act` | per text-bearing block |
| `content.blocks[*].category` | `content_category` | per content block |
| `content.blocks[*].trust` | `trust_level` | per content block (where determinable) |
| `content.blocks[*].capability` | `capability` | per tool_use block |
| `content.blocks[*].citations[*]` | `citation_ref` | per detected reference |
| `content.blocks[*].reminder_subtype` | `reminder_subtype` | per detected reminder |
| `content.blocks[*].plan_items[*]` | `task_plan` | parsed from agent's planning channel |
| `envelope.provider`, `envelope.endpoint`, `envelope.direction` | `envelope_metadata` | record-level dispatch facts |
| `envelope.agent_app`, `envelope.machine`, `envelope.collector_app` | `observation_context` | record-level operational facts |
| `envelope.principal` | `principal_identity` | record-level non-PII actor info |
| `usage.tokens`, `usage.latency`, `usage.retries` | `usage` | per round-trip quantitative facts |
| `turn_end_reason` | `turn_end` | from `stop_reason` normalisation |

The forward-reference above is a sketch. The authoritative schema
will be in ADR 030.

---

## 7. Module layout

The crate is organised one module per family:

```
noodle-domain/
├── src/
│   ├── lib.rs                    # re-exports; the public surface
│   │
│   │  # ─── Content families (§2.1) ───
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
│   │  # ─── Operational-context families (§2.2) ───
│   ├── observation_context.rs    # AgentApp, Machine, CollectorApp
│   ├── principal_identity.rs
│   ├── usage.rs                  # TokenUsage, Latency, RetryCount
│   │
│   │  # ─── Vendor subtypes (§3.1) ───
│   ├── vendor/
│   │   ├── mod.rs                # VendorId, VendorTag
│   │   ├── anthropic.rs          # vendor-specific subtypes for any family
│   │   ├── openai.rs
│   │   ├── google.rs
│   │   └── ...
│   │
│   │  # ─── Per-provider decoder libraries (§ ↓) ───
│   ├── decoders/
│   │   ├── mod.rs                # ProviderDecoder trait surface
│   │   ├── anthropic.rs          # decodes Anthropic-flavoured tap.jsonl records
│   │   ├── openai.rs
│   │   ├── google.rs
│   │   └── ...
│   │
│   └── classifier.rs             # Classifier trait surface
```

`classifier.rs` defines the trait shape downstream consumers
implement to classify content. The crate ships no classifier
implementations — those live in the consumer that needs them.

`decoders/<provider>.rs` is the per-provider decoder library
referenced in "What this ADR specifies" item 7. Each provider
module exports a `ProviderDecoder` impl that consumers use to
read records from that provider — interpreting vendor extras,
mapping vendor-specific tags to canonical types where the
recurrence rule permits, and surfacing per-provider quirks
consistently.

**Decoders are source-agnostic.** A `ProviderDecoder` takes a
`WireSource` (ADR 027 §2.1) — the read-side dual of `WireSink` —
not a file path or `tap.jsonl` reader specifically. The same
decoder operates against:

- The default file-based `WireSource` tailing `tap.jsonl`.
- A live TCP `WireSource` streaming from a network-connected
  collector.
- An in-memory `WireSource` in tests.
- A message-queue `WireSource` consuming a topic.
- Any future implementation that emits the record schema.

Consumers dispatch on `envelope.provider` to select the right
decoder; the `WireSource` implementation is orthogonal.

---

## 8. Open questions

1. **Whether `trust_level` survives.** The trust families overlap
   with what the proxy can observe (who emitted the block) and
   what only a policy layer can decide (whether to actually
   trust). May fold into `envelope_metadata`. Deferred until a
   downstream consumer needs it.
2. **Multilingual content classification.** `speech_act` and
   `content_category` are language-aware. The crate currently
   assumes English-language tagging maps the same way to other
   languages. Likely needs a `language` envelope field; deferred
   to a future ADR.
3. **Classifier composition.** When two classifiers disagree
   (rule-based vs ML), the resolution rule is consumer-side
   today. May need a `ClassificationConfidence` envelope field.
   Deferred.
4. **Versioning the vocabulary.** As types are promoted from
   `VendorSpecific` to canonical, the crate's enum surface
   grows. Consumers compile against a specific version; runtime
   compatibility is via the `_` arm rule (§4.1). Whether a more
   formal versioning scheme is needed (e.g. capability sets)
   depends on consumer evolution. Deferred.
