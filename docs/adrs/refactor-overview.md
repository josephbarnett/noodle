# Refactor overview — code-level delta from current state to ADR-aligned target

**Status:** planning. The umbrella document for the code-level
refactor that brings the Rust workspace into alignment with ADRs
027 through 031. Per-crate detail lives in companion documents
listed in §3.

**Companion documents:**

- [`refactor-noodle-core.md`](refactor-noodle-core.md) — pure traits and types
- [`refactor-noodle-domain.md`](refactor-noodle-domain.md) — type vocabulary (NEW crate)
- [`refactor-noodle-adapters.md`](refactor-noodle-adapters.md) — codecs, transforms, detectors, sinks
- [`refactor-noodle-proxy.md`](refactor-noodle-proxy.md) — driving adapter (rama + tokio)
- [`refactor-noodle-tap.md`](refactor-noodle-tap.md) — file-based WireSink and WireSource
- [`refactor-noodle-viewer.md`](refactor-noodle-viewer.md) — debug UI
- [`refactor-noodle-embellish.md`](refactor-noodle-embellish.md) — embellishment processor (NEW crate)

---

## Goal

The goal of this plan is to specify the **smallest set of code
changes** that bring the noodle workspace into alignment with the
ADRs landed in PR #54 (ADRs 027 through 031), broken into
**delivery slices** that each ship a working, demonstrable
increment.

The plan is **value-driven**, not crate-driven. Each slice is a
unit a reviewer can run, validate, and merge. Per-crate documents
describe **where** each change lands; this overview describes
**when** and **why** in delivery order.

A follow-on epic for **ADR 034 (enterprise CA + external
signing)** is tracked separately as slices **S17–S20** in §9
below. The CA epic has no dependency on S0–S16 and can be picked
up in parallel.

### Why

Two architectural shifts are now specified but not yet
implemented:

1. **`tap.jsonl` is a complete evidence boundary.** ADR 027 + ADR
   030 specify the envelope, the decoded layer, and the
   cross-record pairing. Today, code emits a thinner record
   shape; consumers re-parse SSE and re-pair tool_use_id chains.
2. **`noodle-domain` is a typed vocabulary crate.** ADR 029
   specifies twelve type families across content and operational
   context. Today, classification is scattered across consumer
   code with no central crate.

Two new components are specified but do not yet exist:

3. **`noodle-domain` crate** (ADR 029) — pure types.
4. **`noodle-embellish` crate** (ADR 031) — the embellishment
   processor that produces `ai-telemetry` v0.0.2 events from
   `tap.jsonl`.

The refactor lands all four shifts incrementally. No big-bang
rewrite, no week-long branches.

### Non-goals

- **No semantic changes to the proxy's protocol behaviour.** The
  proxy still observes the same wire facts, applies the same
  cells, writes the same flows. The refactor adds typed metadata
  and decoded structure; it does not change what the proxy
  does on the wire.
- **No new wire codecs.** Adding new providers (OpenAI, Google
  Gemini) is downstream of this refactor.
- **No async-trait revision.** The async-transform question
  (ADR 015 §12 deferred, ADR 024 §8 deferred) stays deferred.
- **No control-plane bundle.** Watchtower / header rewrite / body
  model rewrite / cost feedback are pinned for a separate ADR
  bundle (see `doc-gaps-status.md`).

---

## 1. Current state → target state at a glance

| Concern | Today | Target | Delta location |
|---|---|---|---|
| Type vocabulary for content / operational context | scattered ad-hoc strings | `noodle-domain` crate with 12 families | new crate (`refactor-noodle-domain.md`) |
| `tap.jsonl` envelope | minimal (envelope + marks + body bytes + extractions) | ADR 027 envelope + ADR 030 decoded layer + ADR 029 envelope-typed fields | `refactor-noodle-tap.md`, `refactor-noodle-core.md` |
| Decoded layer on records | absent — body bytes only | `content.blocks[]`, `events[]`, `pairing` fields with typed annotations | `refactor-noodle-tap.md`, `refactor-noodle-adapters.md` |
| Marks block | partial (session_id present; turn_id incomplete) | full ADR 028 contract: marking detector reads SessionStore at flow open, writes back at flow close | `refactor-noodle-core.md`, `refactor-noodle-adapters.md` |
| `WireSink` / `WireSource` duality | `WireSink` exists; `WireSource` absent | both as named trait surfaces in `noodle-core` | `refactor-noodle-core.md`, `refactor-noodle-tap.md` |
| Provider declaration | inferred from `domain` at consumer | `provider: ProviderId` declared per cell in dispatch table; carried on every record | `refactor-noodle-adapters.md`, `refactor-noodle-proxy.md`, `refactor-noodle-tap.md` |
| Sensitive-header redaction | full opaque `<redacted>` | configurable prefix preserved (default N=12) | `refactor-noodle-adapters.md` |
| Per-provider decoder libraries | absent | `noodle-domain::decoders::<provider>` modules, source-agnostic | `refactor-noodle-domain.md` |
| Embellishment processor | absent | `noodle-embellish` crate consuming WireSource, emitting SQLite | new crate (`refactor-noodle-embellish.md`) |
| Viewer alignment | reads internal events directly | reads `tap.jsonl` via WireSource | `refactor-noodle-viewer.md` |

---

## 2. Delivery slices — value-ordered

Each slice is a self-contained PR-sized increment. Slices are
**ordered by dependency**, not by crate. A reviewer merging slices
in order keeps the workspace green at every commit.

| Slice | Scope | Demonstrable outcome | Blockers |
|---|---|---|---|
| **S0 — Workspace bookkeeping** | Add `noodle-domain` and `noodle-embellish` to the workspace as empty crate stubs with `lib.rs` placeholders. Update AGENTS.md crate list. | `cargo build --workspace` green with two new (empty) crates. | none |
| **S1 — `noodle-domain` types** | Implement the 12 type families from ADR 029 §2. Open enums with `VendorSpecific` hatch. Struct shapes for operational-context families (`AgentApp`, `Machine`, `CollectorApp`, `PrincipalIdentity`, `ApiKeyFingerprint`, `OrganizationContext`, `SubscriptionTier`, `TokenUsage`, `Latency`, `RetryCount`). serde derives throughout. | `cargo test -p noodle-domain` validates round-trip serde for every type. No proxy changes. | S0 |
| **S2 — `WireSource` trait in `noodle-core`** | Define `WireSource` as the read-side dual of `WireSink` (ADR 027 §2.1). Trait surface only — implementations come in later slices. | `cargo build` green; trait definition in place; no concrete impls yet. | S0 |
| **S3 — `SessionStore` and revised `RequestDetector`** | Implement the typed `SessionStore` handle (ADR 028 §3) and revise `RequestDetector` to read it (ADR 028 §6). | Marking detector for `api.anthropic.com` produces correct `turn_id` across multi-RT sessions. Existing tests pass. | S0 |
| **S4 — Provider field on dispatch and records** | Add `provider: ProviderId` to cell entries (ADR 025 §3.7) and propagate through `WireSink` to records. | A `tap.jsonl` line carries `provider: "anthropic"` correctly. | S1 (for `ProviderId`), S0 |
| **S5 — Prefix-preserving sensitive-header redaction** | Implement ADR 027 §9 redaction policy with configurable N. `Authorization`, `X-Api-Key`, `Anthropic-Api-Key` default to N=12. | A request to `api.anthropic.com` produces a `tap.jsonl` record with `api_key_prefix = "sk-ant-api03-wcq…"` (not `<redacted>`). | S0 |
| **S6 — Envelope-level operational-context fields** | Plumb `AgentApp` (from `User-Agent` header + `X-Stainless-*`), `Machine` (proxy-host facts), `CollectorApp` (compile-time build info) onto every record envelope. | A `tap.jsonl` line carries `envelope.agent_app`, `envelope.machine`, `envelope.collector_app` correctly. | S1, S4 |
| **S7 — Subscription context on envelope** | Plumb `ApiKeyFingerprint` (from §S5 prefix), `OrganizationContext` (from URL on `claude.ai`, headers on `api.anthropic.com`) onto every record. | `envelope.subscription.api_key.prefix` and `envelope.subscription.organization.organization_id` populated. | S5, S6 |
| **S8 — `TokenUsage` and `Latency` on records** | Extract `usage` from `message_delta.usage` events and from response headers; populate `usage.tokens` and `usage.latency`. | A response record carries `usage.tokens.input_tokens`, `output_tokens`, `cache_read_input_tokens`, etc. | S1 |
| **S9 — Decoded `content.blocks[]` on records** | Codec layer (already L0–L5) emits parsed content blocks; `WireSink` writes them as `content.blocks[]` per ADR 030 §2. | A response record carries `content.blocks[*].kind` populated as `text` / `thinking` / `tool_use`. | S0 |
| **S10 — Parsed `events[]` on response records** | Persist the SSE event stream as a typed list per ADR 030 §3. | A response record carries `events[]` with `ts_offset_ms` per event. | S9 |
| **S11 — Tool-use cross-record pairing** | Implement the back-patch table (ADR 030 §4.3). Forward and backward references populated. | A `tool_use` in record N carries `pairing.resolved_by_request_id`; matching `tool_result` in record N+k carries `pairing.resolves_tool_use_in_request_id`. | S9, S10 |
| **S12 — File-based `WireSource` (tail mode)** | Implement `noodle-tap::WireSource::FileTail`. | A test consumes records from `tap.jsonl` while the proxy is writing. | S2 |
| **S13 — File-based `WireSource` (batch mode)** | Implement `noodle-tap::WireSource::FileRead`. | A test consumes records from an existing `tap.jsonl` to EOF. | S2 |
| **S14 — Per-provider decoder libraries** | Implement `noodle-domain::decoders::anthropic` (the only provider supported today). Other providers added as adapters arrive. | A test consumes `tap.jsonl` via WireSource, decodes Anthropic records, produces typed events. | S1, S12 |
| **S15 — Viewer reads via `WireSource`** | Refactor `noodle-viewer` to consume `tap.jsonl` through `WireSource::FileTail` rather than via direct event re-emission. | Viewer runs against a live `tap.jsonl`; no behaviour regression. | S12 |
| **S16 — `noodle-embellish` SQLite emitter** | Implement the `ai-telemetry` v0.0.2 mapping (ADR 031 §5) writing to a local SQLite file. | A test pipeline produces a SQLite database from a captured `tap.jsonl`. | S1, S4, S5, S6, S7, S8, S12 |

### 2.1 Slice ordering rationale

- **S0–S3 are foundational.** No user-visible value yet; everything
  else depends on these.
- **S4–S8 are envelope enrichment.** Each ships immediately
  visible value on `tap.jsonl` (a new field appears).
- **S9–S11 are the decoded layer.** Adds the OODA projection
  consumers need.
- **S12–S15 close the WireSource side** and unblock the viewer.
- **S16 is the value-loop closure.** Once landed, noodle output
  produces telemetry-shaped events end-to-end.

### 2.2 Slice independence

S4 and S5 are independent (different code paths). S6, S7, S8 are
independent of each other (different envelope fields). The
sequence in §2 is one valid ordering; parallel slices may be
merged independently if the workspace stays green.

### 2.3 Slice ↔ feature ↔ acceptance fixture

Every slice names the open feature stories it unblocks (so a
reviewer can start a downstream feature ticket the moment its
blocker lands) and the **specific** capture or test asset that
proves it works. This is the per-slice acceptance gate — a
reviewer should be able to verify a slice in under 5 minutes
from this table.

| Slice | Unblocks | Acceptance fixture (capture / golden / test) |
|---|---|---|
| S0 | (none — bookkeeping) | `cargo build --workspace` green; `noodle-domain` and `noodle-embellish` present in `Cargo.toml` workspace members |
| S1 | 005, 030, 034 | `crates/noodle-domain/tests/round_trip_serde.rs` — every type round-trips through serde |
| S2 | (foundational for S12–S16) | `cargo build --workspace` green; `WireSource` trait visible in `noodle-core` public API |
| S3 | 005, 030 | `captures/enterprise/claude-code-cli-api.mitm` — multi-RT session asserts `turn_id` stable across rounds; `marking_capture_replay.rs` |
| S4 | 016, 032 (provider field) | `tap.jsonl` golden — any record carries `provider: "anthropic"`; replay against `claude-code-cli-api.mitm` |
| S5 | (telemetry consumers) | Golden field on header-redaction transform — `Authorization` value equals `sk-ant-api03-wcq…` (first 12 chars + ellipsis), not `<redacted>` |
| S6 | (envelope consumers, embellishment) | `tap.jsonl` golden — `envelope.agent_app.id == "claude-code"`, `envelope.machine.host_id` non-empty, `envelope.collector_app.git_sha` matches build |
| S7 | (org-context consumers) | `tap.jsonl` golden — `envelope.subscription.api_key.prefix` populated; `envelope.subscription.organization.organization_id` extracted from URL/header |
| S8 | 032 | Golden against `claude-desktop-cowork-enterprise.mitm` — record carries `usage.tokens.input_tokens`, `output_tokens`, `cache_read_input_tokens` |
| S9 | 032, 033 | `crates/noodle-adapters/tests/anthropic_decoder.rs` — content.blocks[] kinds match expected for `claude-code-cli-api.mitm` |
| S10 | 032 | Golden — `events[]` populated with `ts_offset_ms` per SSE event; replay against `claude-code-cli-api.mitm` |
| S11 | 032 | `crates/noodle-adapters/tests/tool_use_pairing.rs` — forward and backward references match across multi-RT capture |
| S12 | (foundational for S14–S16) | `crates/noodle-tap/tests/wire_source_file_tail.rs` — consumes records from a live-written `tap.jsonl` |
| S13 | 028, 031, 035 | `crates/noodle-tap/tests/wire_source_file_read.rs` — consumes an existing capture to EOF |
| S14 | 028, embellish | `crates/noodle-domain/tests/anthropic_decoder_e2e.rs` — `tap.jsonl` → typed event stream against `claude-code-cli-api.mitm` |
| S15 | 035 | Viewer integration test — hub consumes `tap.jsonl` via `WireSource::FileTail`; HTTP/SSE/OODA snapshot tests pass against `claude-desktop-code-enterprise.mitm` |
| S16 | 028 | `crates/noodle-embellish/tests/end_to_end.rs` — SQLite produced from `claude-code-cli-api.mitm` conforms to `ai-telemetry` v0.0.2 schema |

If a slice's fixture does not exist when the slice is picked up,
**creating it is part of the slice's PR scope** — not a follow-up.
The slice does not merge without it.

---

## 3. Per-crate impact summary

Each crate has its own delta document with the full breakdown.
This table is the at-a-glance view.

| Crate | Slices touching it | Major impact |
|---|---|---|
| **`noodle-core`** | S2, S3 | `WireSource` trait surface; `SessionStore` handle in `TransformAttachment`; revised `RequestDetector` (`turn_id` / `parent_session_id` derivation) |
| **`noodle-domain`** (NEW) | S0, S1, S14 | Entire crate is new — type families + per-provider decoder libraries |
| **`noodle-adapters`** | S3, S4, S5, S6, S7, S8, S9, S10, S11 | Per-cell marking detectors; provider declaration consumption; redaction transform; envelope-field-producing detectors; decoded-content-block production |
| **`noodle-proxy`** | S4, S6 | Dispatch table parse: provider field. Collector-build info captured at compile time. |
| **`noodle-tap`** | S4, S6, S7, S8, S9, S10, S11, S12, S13 | Record-writer extended to include all new envelope fields; `WireSource::FileTail` and `::FileRead` implementations |
| **`noodle-viewer`** | S15 | Replace direct event consumption with `WireSource::FileTail` |
| **`noodle-embellish`** (NEW) | S0, S16 | Entire crate is new — embellishment processor (`tap.jsonl` → SQLite) |

---

## 4. Cross-crate dependencies

```
noodle-core (foundational; no deps in workspace)
   ↑
   ├── noodle-domain (depends on noodle-core for identifier types)
   │       ↑
   │       └── noodle-embellish (depends on both)
   │
   ├── noodle-adapters (depends on noodle-core)
   │       ↑
   │       └── noodle-proxy (depends on noodle-core, noodle-adapters, noodle-tap)
   │
   ├── noodle-tap (depends on noodle-core)
   │
   └── noodle-viewer (depends on noodle-core, noodle-domain)
```

Direction is **always inward** in the hexagonal sense — domain and
adapters never depend on the proxy. The refactor preserves this
shape.

---

## 5. Test coverage

Every slice ships test coverage. The discipline:

- **Unit tests** alongside code (`#[cfg(test)] mod tests`).
- **Integration tests** in `crates/<crate>/tests/` for cross-module
  flows.
- **End-to-end fixtures** under `captures/` — every slice that
  changes wire-observable behaviour adds or re-uses a capture
  asserting on client-visible bytes (ADR 017 §7).

A slice merges when:

1. `cargo clippy --workspace --all-targets -- -D warnings` green.
2. `cargo test --workspace` green.
3. New behaviour has new tests.
4. Cross-cutting acceptance test (capture-driven) added when the
   slice changes records on `tap.jsonl`.

---

## 6. Risks and blast radius

| Risk | Mitigation |
|---|---|
| Schema drift on `tap.jsonl` mid-refactor | Every record carries `schema_version` (ADR 030 §7). Consumers handle unknown fields gracefully. New fields land as optional first; required-ness tightened only after consumers update. |
| Marking detector regression | Capture-driven acceptance test (ADR 017 §7) per cell, asserting on client-visible bytes. Run pre- and post-S3. |
| Performance overhead from decoded layer | Decoded content is opt-in per cell via dispatch (ADR 031 §6 `omit_content` open question — pinned for future if real cost surfaces). |
| Cross-record patching complexity (S11) | First implementation uses an in-memory back-patch table bounded by N entries; falls back to side-effect emission on overflow. Bounded blast radius. |
| `noodle-embellish` SQLite contention | Single-writer model. Migration tool ships with the crate; schema version checked at startup. |

---

## 7. What's NOT in this refactor

For clarity, several items are explicitly excluded — pinned for
future refactor passes:

- **Async transforms / detectors** (ADR 015 §12; ADR 024 §8 deferred).
- **OTLP location resolution** (ADR 022 vs ADR 001 principle 7 — live disagreement; separate ADR).
- **Watchtower / control-plane attachment surface** (5 capabilities; separate ADR).
- **Persistent SessionStore / cold-cache recovery** (ADR 028 §10 deferred).
- **Schema migration tooling for `noodle-embellish`** (ADR 031 §8 deferred until first schema bump).
- **`noodle-macos-tproxy` changes** — entry-transport binding is stable; the refactor doesn't touch it.

---

## 8. Success criteria

The refactor is **done** when:

1. Every slice S0–S16 is merged on `main`.
2. The workspace is green at every commit (clippy + test).
3. `cargo run -p noodle-embellish` against a fresh `tap.jsonl`
   produces a SQLite file conforming to `ai-telemetry` v0.0.2.
4. The viewer reads `tap.jsonl` via `WireSource::FileTail` and
   renders HTTP, SSE, and OODA views without regression.
5. All capture-driven acceptance tests pass against the corpus
   in `captures/`.
6. `doc-gaps-status.md` updated: block 1 (`noodle-domain` crate)
   moves to ✅.

---

## 9. ADR 034 follow-on: enterprise CA epic (S17–S20)

ADR 034 (enterprise CA + external signing) landed after the
027–031 refactor was drafted. It introduces net-new code in
`noodle-core` (a trait), `noodle-adapters` (signer backends),
and `noodle-proxy` (procurement + health integration). The work
is **independent of S0–S16** — no slice in the main refactor
depends on it, and it can be picked up in parallel by a second
contributor.

| Slice | Scope | Demonstrable outcome | Blockers | Feature | Acceptance fixture |
|---|---|---|---|---|---|
| **S17 — `CertMintService` trait + `LocalCertMintService`** | Extract leaf minting behind a noodle-owned trait; reuse rama cache layer; no behaviour change in local-CA mode | Existing TLS-MITM capture replay passes byte-for-byte; fake mint service can be substituted in tests | none | 036 | TLS-MITM capture replay against `api.anthropic.com`; fake-service unit test asserts request shape |
| **S18 — BYOCA static mode** | Load operator-supplied `ca.pem` + `ca.key` from disk; explicit mode dispatch; fail loud on missing/mismodes | A leaf minted in BYOCA-static mode chains to the operator's CA | S17 | 037 | `openssl verify -CAfile ca.pem chain.pem leaf.pem` succeeds; permission-failure tests reject 0644 keys |
| **S19 — `ExternalCertMintService` + Vault PKI backend + procurement** | Network-backed mint via Vault PKI (mTLS / token); CSR-only (leaf private key never leaves host); startup procurement pre-warms cache | End-to-end MITM against `api.anthropic.com` with stub Vault returns a leaf chaining to the test enterprise CA | S17, S18 | 038 | wiremock Vault stub + MITM capture replay; audit events emitted with ADR 034 §5.4 shapes |
| **S20 — Rip-cord: health degradation on sustained mint failure** | `MintFailureCounter` → health probe transitions → entry transport fail-open. Recovery automatic on next success | 5 consecutive `SignerUnavailable` → health probe flips unhealthy within one probe interval; subsequent success flips it back | S19 | 039 | wiremock Vault returns 5×503 then 200; health endpoint state transitions asserted; manual end-to-end runbook step |

### 9.1 Why this is a separate epic

- **Different ADR pedigree.** S0–S16 are about `tap.jsonl`
  evidence shape (ADRs 027–031); S17–S20 are about enterprise
  CA (ADR 034). Bundling them would obscure scope.
- **Different reviewer audience.** S0–S16 needs codec / type-
  system review attention. S17–S20 needs TLS / PKI / security
  review attention.
- **Independent merge order.** S17 has no dependency on any
  S0–S16 slice; can start day one in parallel.

### 9.2 Risks specific to the CA epic

| Risk | Mitigation |
|---|---|
| Subtle change in cert minting breaks an existing client that pins on cert details | S17 acceptance includes byte-identical capture replay for the local-CA mode; replay covers existing TLS-MITM behaviour |
| External signer unavailable during testing | Wiremock-based Vault stub used in tests; real Vault optional / integration-only |
| Health-probe oscillation under intermittent signer failure | Threshold-based counter (default 5 consecutive) + once-per-transition audit event prevents flapping; tested via property test on counter |
| Procurement task races with first client connection at startup | Cache miss path always falls back to on-demand mint; procurement is best-effort |

### 9.3 Success criteria (epic 034)

The CA epic is **done** when:

1. S17–S20 merged on `main`.
2. The local-CA mode is byte-identical to pre-refactor (no
   regression risk for existing deployments).
3. A noodle proxy can be configured in `mode = "external"` with
   a stub Vault and pass an end-to-end MITM capture replay.
4. Killing the stub Vault while traffic is in flight causes the
   health probe to flip unhealthy within one probe interval +
   threshold window, and restoring the stub returns the proxy
   to healthy automatically.
5. `docs/features/036–039` all marked `done`.

---

## 10. Viewer-alignment follow-on epic (S21–S23)

S15 of the 027–031 refactor shipped the viewer's data-ingestion
seam — the hub now reads `tap.jsonl` through
`WireSource::FileTail`. But the viewer's **display model
(`Exchange`) and React frontend** still pre-date the decoded
envelope/content/events/pairing/usage layer that landed in S4–S11.
The screenshot of the live viewer confirms it functions; it also
confirms the OODA hierarchy is reconstructed by the **legacy
`ooda.ts` heuristic** rather than read from the on-disk
`marks.turn_id` / `content.blocks[]` / `events[]` / `pairing` /
`envelope.*` / `usage.*` fields the proxy now populates. The new
payload is on the wire but invisible in the UI.

This follow-on epic lights up the new structures end-to-end —
the viewer + the embellish processor both consume through the
`noodle_domain::decoders::AnthropicDecoder` (S14), proving the
decoder is reusable across consumers.

| Slice | Scope | Demonstrable outcome | Blocker |
|---|---|---|---|
| **S21 — Viewer consumes via `AnthropicDecoder` + `DecodedExchange` model** | Replace the slim `Exchange` model with a `DecodedExchange` that carries marks / content.blocks / events / pairing / envelope / usage. Hub feeds tap records through `noodle_domain::decoders::AnthropicDecoder` via a `ProviderDecoder` registry dispatched on `envelope.provider`. The legacy `ooda.ts`-style derivation gets superseded by reading `marks.turn_id` directly. | Exec-claude e2e prints decoder-produced turn_ids, token counts, content-block kinds, pairings — proves the decoder reaches the hub and the model carries the fields. Existing viewer integration tests still pass. | S14, S15 (both merged) |
| **S22 — Frontend displays the new fields** | React components: `turn_id` badge per row, token-usage + latency panel on response rows, content-block kind tags, tool-use pairing arrows, envelope inspector (agent_app / machine / subscription). Visual demo via `make viewer` against live `tap.jsonl`. | A user opens the viewer, sees the new typed fields rendered in OODA / HTTP / SSE views. | S21 |
| **S23 — `noodle-embellish` consumes via `AnthropicDecoder`** (reusable-decoder consolidation) | Refactor `noodle-embellish::mapper` to read `DecodedEvent`s from `AnthropicDecoder` instead of the inline tap.jsonl JSON parsing it does today. Same decoder, two consumers — viewer + embellish — validates the "reusable in other contexts" property the ADR 029 §7 design demands. | Embellish still produces the same `ai-telemetry` v0.0.2 SQLite rows (byte-identical to S16's golden), but reads through the typed decoder. | S14, S16 (both merged) |

### 10.1 Why this is a separate epic

S15 of the original refactor deliberately scoped the viewer work
to the data-ingestion seam (`WireSource::FileTail`) — the model
+ frontend refresh was left "out of scope" in S15's PR description.
This follow-on completes that work. It's grouped here so the
overview tracks all viewer-alignment slices in one place.

### 10.2 Risks specific to S21–S23

| Risk | Mitigation |
|---|---|
| Frontend regression — current React UI users depend on existing OODA / HTTP / SSE views | S22 ships as additive UI panels first; legacy derivation only retires once the new path is byte-equivalent for the OODA hierarchy. Visual snapshot tests pin the existing views. |
| Decoder consolidation breaks embellish output | S23's e2e asserts the SQLite database byte-identical to the S16 baseline; the refactor is a strict swap of the input plumbing, not the mapping logic. |
| Vendor-specific blocks fall through `VendorSpecific` and lose detail | The S14 decoder already routes unknown shapes through `VendorSpecific(VendorContentCategory)` etc. with the original tag preserved — the UI can still display the verbatim text. |

### 10.3 Success criteria (epic 10)

The viewer-alignment epic is **done** when:

1. S21–S23 merged on `main`.
2. Running `make viewer` against a live `tap.jsonl` shows
   `marks.turn_id` on session/turn headers, token usage + latency
   on response rows, content-block kind tags inline, and pairing
   arrows for tool calls.
3. The same `AnthropicDecoder` is the single source of truth
   for the viewer's `DecodedExchange` AND the embellish mapper.
   Grep proves both consumers import from
   `noodle_domain::decoders::anthropic` — no duplicate decoding
   logic.
4. The legacy `ooda.ts` heuristic is either retired or marked
   with a `// LEGACY — read marks.turn_id instead` comment
   pointing at the new path.
