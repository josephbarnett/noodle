# 048 — WASM plugin author experience audit

**Status:** audit only. No code or doc changes proposed beyond Section 5.
**Audience:** Joe; whoever picks up the plugin-author-experience thrust next.
**Scope:** the deployment topology where `noodle-detect` is embedded into a
third-party LLM gateway (LiteLLM, Bifrost, Portkey, …) as a WASM artifact.
The endpoint and gateway proxy topologies are out of scope here; they have
their own docs and are demonstrably operable in the repo today.

---

## Executive summary

1. ADR 039 is the only design document that names the plugin topology
   beyond a passing mention. ADRs 001, 006, 015, 016, 019, 020, 021, 022,
   023, 025, 028, 029, 033, 041, and 042 — every document a plugin author
   would land on first — describe the trait surface and the data plane as
   though only the proxy host exists. Nothing in those ADRs has been
   updated to reflect the B.1–B.5 carve-outs that shipped on 2026-05-28
   to 2026-05-29.
2. ADR 039 §2.3 specifies an `AttributionFacts` shape with `resolved: Resolved`
   and `usage: Option<WireUsage>`. The shipped `AttributionFacts` at
   `crates/noodle-detect/src/facts.rs:14` carries `resolved: Option<ResolvedRecord>`
   and `round_trip: Option<RoundTripRecord>` and has no `usage` field. The
   ADR and the code disagree on the load-bearing output shape.
3. The shipped `detect()` at `crates/noodle-detect/src/lib.rs:80-104` is a
   stub: it ignores `_request` and `_response` (both prefixed `_`) and
   returns an empty bundle. Its rustdoc claims it "synchronously dispatches
   the user-agent detector, mints correlation IDs from
   `DetectContext::marking_store`, and returns the assembled
   `AttributionFacts`." It does none of that. This is the central
   doc/code drift.
4. `docs/diagrams/plugin-architecture.drawio` is byte-identical to
   `docs/diagrams/gateway-architecture.drawio` (`md5 a5d968fc…`). The plugin
   topology has no distinct diagram. ADR 039 line 24 says "Plugin-topology
   diagram pending; the facade design in §2.3 + §3 is the textual
   specification until then" — that's still true, and the duplicate file
   misleads anyone who finds it before reading the ADR.
5. No rendered PNG exists for either the duplicate plugin-architecture
   drawio source or for any plugin-specific flow. `docs/images/` contains
   `gateway-architecture.png` (2026-05-27); there is no
   `plugin-architecture.png`.
6. `crates/noodle-detect/` has no `README.md`, no `examples/` directory,
   no integration test, and no host-language glue reference (Python, Go,
   Node). A plugin author who clones the repo and finds the crate has the
   four-file `src/` tree and nothing else.
7. Cross-language embedding guidance in ADR 039 §3 names `wasmtime-py`,
   `wasmtime-go`, and `@bytecodealliance/jco` but provides zero code,
   zero call signatures, and no statement of which guest ABI the WASM
   artifact exposes (raw `wasm32-unknown-unknown` extern "C", WIT/Component
   Model, witx). A Go/Python/Node engineer cannot start without making this
   decision themselves.
8. ADR 040 Track B is the only place the B.1–B.6 slices are tracked. No
   per-slice feature stories exist (`docs/features/000-overview.md` row 30
   acknowledges this). Compare to Track A and Track D, where most slices
   have feature stories.
9. The carved crates `noodle-sinks`, `noodle-cert-external`,
   `noodle-embellish-core`, `noodle-detect`, and `noodle-tls` are absent
   from ADR 001 (component architecture, canonical reference) and absent
   from `docs/adrs/refactor-*.md`. ADR 001 §3.2 still describes the
   pre-carve crate layout.
10. ADR 006 (extensibility posture, last touched 2026-05-09) still reads
    "v1 supports compile-time plugins only." It pre-dates ADR 039 by
    eighteen days and pre-dates the carve-out work by nearly three weeks.
    A reader who finds 006 will conclude WASM is not yet supported, even
    though `cargo build --target wasm32-unknown-unknown -p noodle-detect`
    has been green since #102.

---

## 1. Inventory

For every document and code asset that touches the WASM plugin topology.
`mtime` is the file modification time on the audit date (2026-06-01);
"post-B.5" means after 2026-05-29 (the B.5 PR #102 merge). "References
039?" is grep for "039" or "noodle-detect" or "wasm" in the file.

### 1.1 Design ADRs

| Path | One-line purpose | mtime | Post-B.5? | References 039? | Rendered diagram? |
|---|---|---|---|---|---|
| `docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md` | The plugin-topology ADR. Defines the three topologies, names the carve-outs, pins the facade contract. | 2026-05-27 | No | self | endpoint + gateway PNGs referenced; plugin PNG explicitly absent (line 24) |
| `docs/adrs/040-post-parity-cadence.md` | Track-B carve-out roadmap; B.1–B.6 slice table. | 2026-05-28 | No | yes (line 47, 51–58) | none referenced |
| `docs/adrs/006-extensibility-posture.md` | "Compile-time plugins only for v1." Pre-039 by 18 days. | 2026-05-09 | No | no — references WASM as deferred | none |
| `docs/adrs/001-component-architecture.md` | Canonical component list. §3.2 still describes pre-carve crates. §8.12 still references ADR 006's deferred WASM posture. | 2026-05-27 | No | no | many (none plugin-specific) |
| `docs/adrs/015-layered-codec-architecture.md` | The `Codec` + `Transform` trait surface a plugin author writes against. Makes no statement about plugin embedding. | 2026-05-21 | No | no | `015-layered-codec-architecture.png` |
| `docs/adrs/016-cache-and-release-primitives.md` | Streaming-buffer primitives transforms use. Status: Proposed. Makes no plugin statement. | 2026-05-14 | No | no | `016-cache-and-release-primitives.png` |
| `docs/adrs/019-endpoint-routed-capability-dispatch.md` | The dispatch frame — `(domain, endpoint, direction) → capability chain`. Pure proxy framing; no plugin discussion. | 2026-05-17 | No | no | none |
| `docs/adrs/020-side-effect-sink-and-resolver-wiring.md` | `SideEffectSink` port + Resolver wiring. Names `noodle-adapters` as the sink home; pre-dates B.1 carve to `noodle-sinks`. | 2026-05-17 | No | no | none |
| `docs/adrs/021-detector-vs-transform-two-tier.md` | The `RequestDetector` trait surface. Critical for plugin authors writing detectors; makes no plugin statement. | 2026-05-17 | No | no | none |
| `docs/adrs/022-otel-collector-embellishment-plane.md` | Downstream context — where `SideEffectSink` output goes. Plugin-relevant because a plugin host's gateway may already have an OTel collector. | 2026-05-21 | No | no | `022-data-and-embellishment-planes.drawio` (contains the only existing "plugin" string in any diagram source) |
| `docs/adrs/023-roundtrip-telemetry-records-and-correlation-ids.md` | The `RoundTripRecord` shape. The `round_trip: Option<RoundTripRecord>` field of `AttributionFacts` traces here. No plugin statement. | 2026-05-27 | No | no | `system-architecture.png` |
| `docs/adrs/025-dispatch-table.md` | Dispatch-table v2; refers to ADR 006 for WASM. No plugin-specific section. | (not opened — referenced via grep) | No | only via 006 | (not checked) |
| `docs/adrs/028-session-store-and-marking-detector-contract.md` | `MarkingStore` port + per-cell `MarkingDetector` contract. A plugin host's `DetectContext` carries `Arc<dyn MarkingStore>`; this is the contract the plugin author must implement (or import from `noodle-adapters::marking`). No plugin statement. | 2026-05-21 | No | no | none |
| `docs/adrs/029-noodle-domain-crate.md` | Typed telemetry vocabulary `AttributionFacts.hints` and `artifacts` carry. No plugin statement, but pure-type crate so structurally plugin-ready. | 2026-05-21 | No | no | none |
| `docs/adrs/033-product-architecture-separation-of-concerns.md` | References ADR 039 in the related-ADR list (line 10). §3 (line 161) still describes "compile-time plugins (ADR 006)" — pre-039 framing. | 2026-05-21 | No | references but not aligned | none |
| `docs/adrs/041-l5-coverage-tool-use-and-usage.md` | Latest ADR (2026-05-31, post-B.5). Tool-use accumulation + usage on TurnEnd. Makes no mention of `noodle-detect` despite shipping after the facade. | 2026-05-31 | Yes (mtime) | no | none |
| `docs/adrs/042-codec-side-channel-and-error-contract.md` | Post-B.5. Codec audit-emission contract. Affects every plugin codec; no mention of plugin topology. | 2026-05-31 | Yes | no | none |

### 1.2 Code assets

| Path | One-line purpose | mtime | Post-B.5? | References 039? |
|---|---|---|---|---|
| `crates/noodle-detect/Cargo.toml` | Crate manifest. Includes a comment block (lines 32–39) explaining the wasm32 `getrandom` feature flag. | 2026-05-29 | Yes (it is B.5) | yes |
| `crates/noodle-detect/src/lib.rs` | Public surface. `pub fn detect(...)`. v1 stub. | 2026-05-29 | Yes | yes |
| `crates/noodle-detect/src/context.rs` | `DetectContext`, `Clock`, `SystemClock` | 2026-05-29 | Yes | yes |
| `crates/noodle-detect/src/facts.rs` | `AttributionFacts` | 2026-05-29 | Yes | yes |
| `crates/noodle-detect/src/request.rs` | `DetectRequest` | 2026-05-29 | Yes | no |
| `crates/noodle-detect/src/response.rs` | `DetectResponse` | 2026-05-29 | Yes | no |
| `crates/noodle-detect/examples/` | absent | — | — | — |
| `crates/noodle-detect/tests/` | absent | — | — | — |
| `crates/noodle-detect/README.md` | absent | — | — | — |
| `crates/noodle-tls/` | B.5 carve-out — TLS MITM primitives moved out of `noodle-adapters` | 2026-05-29 | Yes | yes (Cargo.toml comments) |
| `crates/noodle-cert-external/` | B.2 carve-out — Vault PKI client | 2026-05-28 | Yes | yes |
| `crates/noodle-embellish-core/` | B.3 carve-out — pure mapper library | 2026-05-28 | Yes | yes |
| `crates/noodle-sinks/` | B.1 carve-out — file/runtime-coupled SideEffectSink adapters | 2026-05-28 | Yes | yes |

### 1.3 Diagrams

| Path | One-line purpose | mtime | Rendered PNG | Referenced from ADR? |
|---|---|---|---|---|
| `docs/diagrams/plugin-architecture.drawio` | Intended to depict the plugin topology. **Byte-identical to `gateway-architecture.drawio`** — see Section 4. | 2026-05-27 | absent | no |
| `docs/diagrams/gateway-architecture.drawio` | Gateway-host topology. | 2026-05-27 | `docs/images/gateway-architecture.png` | ADR 039 line 20–22 |
| `docs/diagrams/system-architecture.drawio` | Endpoint-host topology. | 2026-05-27 | `docs/images/system-architecture.png` | ADR 039 line 16–18; ADR 023 line 17 |
| `docs/diagrams/022-data-and-embellishment-planes.drawio` | Data plane → embellishment plane. The only diagram with the word "plugin" in it — but as a label inside the architecture, not as the topology. | 2026-05-21 | drawio only; no PNG matched in `docs/images/` for this name | ADR 022 line 107 |
| `docs/diagrams/015-layered-codec-architecture.drawio` | The trait stack. | 2026-05-14 | `docs/images/015-layered-codec-architecture.png` | ADR 015 |
| `docs/diagrams/016-cache-and-release-primitives.drawio` | Buffer primitives. | 2026-05-14 | `docs/images/016-cache-and-release-primitives.png` | ADR 016 |
| `docs/diagrams/flows.md` | Five mermaid diagrams. Hexagonal view, request lifecycle, adapter selection, stream pipeline, session/turn state. **None covers the plugin topology.** | 2026-05-09 | inline mermaid | no |

### 1.4 Feature stories

| Path | One-line purpose | Status |
|---|---|---|
| `docs/features/000-overview.md` (row 30) | Track B componentization tracker | Open, points to ADR 040 |
| `docs/features/done/028-embellishment-addon-layer.md` | Mentions `noodle-detect` only via the embellishment-plane prose. | Done (parked) |
| No B.1–B.6 per-slice stories | — | absent |
| No plugin-author guide story | — | absent |

---

## 2. Doc-quality findings (per-document)

Scoring legend: **good** — meets the criterion; **weak** — partial; **missing** — absent.

### 2.1 ADR 039 — Deployment topologies and the `noodle-detect` facade

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | Tight, no asides. Tables carry the load. |
| Diagram present and rendered | weak | Endpoint + gateway diagrams referenced and rendered. Plugin diagram explicitly absent (line 24) — and the file that exists at that name is a duplicate of the gateway diagram (Section 4 below). |
| Flows / sequences described | weak | §3 gives a per-host embedding table but no end-to-end sequence (host receives bytes → calls `detect()` → routes facts). No fail-mode flow. |
| API / contract specified precisely | **weak / divergent** | §2.3 specifies `AttributionFacts { resolved: Resolved, usage: Option<WireUsage>, … }`. Code at `crates/noodle-detect/src/facts.rs:14` ships `resolved: Option<ResolvedRecord>`, `round_trip: Option<RoundTripRecord>`, no `usage`. The ADR and the shipped contract disagree. Also: `DetectContext` is described as carrying "session_id, prior turn_id, dispatch-table override, clock" (line 82) but the shipped struct at `crates/noodle-detect/src/context.rs:17` carries `clock`, `marking_store`, `session_id` — no `prior turn_id`, no `dispatch-table override`. |
| Examples included | missing | No code example showing a host calling `detect()`. No example detector or transform. No example of constructing a `DetectContext`. |
| Cross-references to related ADRs | good | Related-ADR list at the top is accurate and bidirectional with ADRs 020 (line 9), 022 (line 11), 033 (line 12), 037 (line 12). |

**Specific weak passages (line citations + suggested rewrite):**

- **Line 24**: "Plugin-topology diagram pending; the facade design in §2.3 + §3 is the textual specification until then."
  - The duplicate `plugin-architecture.drawio` exists but is a stale gateway copy. Either rewrite to "Plugin-topology diagram TODO — `docs/diagrams/plugin-architecture.drawio` currently duplicates the gateway diagram and must be replaced before this ADR closes" or remove the file (Section 5).

- **Lines 77–93** (the `pub fn detect` and `AttributionFacts` snippets):
  - `AttributionFacts.resolved: Resolved` → ship-shape is `Option<ResolvedRecord>`. The wrapper type changed from `Resolved` (the map) to `ResolvedRecord` (the typed record per ADR 020 §2.2) and became optional for request-only invocations. The ADR should match the shipped types.
  - `AttributionFacts.usage: Option<WireUsage>` → shipped struct has no `usage` field; the equivalent information lives inside `round_trip: Option<RoundTripRecord>` (per ADR 023 §4). Either re-add `usage` to the struct, or update the ADR to reflect that usage is carried under `round_trip.usage`.
  - The function signature shows `response: &DetectResponse` (non-optional) while the shipped facade takes `Option<&DetectResponse>` (`crates/noodle-detect/src/lib.rs:82`). The ADR's prose at line 78 says "or None for request-only flows" but the snippet contradicts that.
  - Suggested rewrite: replace lines 77–93 with the shipped surface verbatim, with a footnote that v1 ships the shape but the body is a stub pending detector wiring.

- **Lines 101–104** (invariants): "Pure function modulo `Clock`. Same inputs + same clock → same outputs."
  - The shipped v1 body returns an empty `AttributionFacts` for every input. The "same inputs → same outputs" invariant is currently true vacuously. The doc-comment at `crates/noodle-detect/src/lib.rs:72` claims the function "synchronously dispatches the user-agent detector, mints correlation IDs…" — it does not. Either the ADR's invariant text or the source comment should acknowledge "v1 is a shape-only stub; detector dispatch lands in a follow-up slice."

- **Lines 105–108** (streaming invariant): "A streaming variant produces facts incrementally as response bytes arrive."
  - No streaming variant exists in the shipped surface. The ADR makes a forward-looking commitment that the code doesn't carry. Mark as a future addition or remove.

- **Lines 117–124** (embedding strategy table): names `wasmtime-py`, `wasmtime-go`, `@bytecodealliance/jco`. Says nothing about which guest ABI the WASM artifact exposes. Plugin authors in Python/Go/Node need to know: is `noodle-detect.wasm` a raw `wasm32-unknown-unknown` module with exported functions and a hand-rolled extern "C" surface, a WIT/Component-Model component, or a witx/WASI export? Right now §3 implies all of these are equivalent. They are not.
  - Suggested rewrite: add §3.1 "Guest ABI" naming one of the three, with rationale. Without it, downstream stories cannot begin.

- **Lines 200–215** (Non-goals): "v1 ships static detector + transform registration at WASM compile time."
  - "WASM compile time" is ambiguous — it means at the time the `noodle-detect.wasm` artifact is built (i.e., in the noodle repo), not at the time the host instantiates the module. Plugin authors will read this and ask "can I register my own detector from my plugin code?" The answer is "no, you fork noodle-detect, build your own .wasm, ship that" — but the ADR doesn't say that explicitly.

### 2.2 ADR 040 — Post-parity cadence

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | Tight track-and-slice tables. |
| Diagram present and rendered | n/a | This is a cadence doc, not an architecture doc. |
| Flows / sequences described | good | Each slice is one row with a Why and a story-file pointer where applicable. |
| API / contract specified precisely | n/a | This doc indexes ADRs; the contract lives in 039. |
| Examples included | n/a | Cadence doc. |
| Cross-references to related ADRs | good | Each slice links to ADR 039 or the relevant story. |

**Specific weak passages:**

- **Line 54** (B.4): "synchronous `detect(req, resp, ctx) → AttributionFacts` API".
  - Same divergence as ADR 039: shipped surface takes `Option<&DetectResponse>`. Minor.

- **Line 55** (B.5): "Closes ADR 039 §8 acceptance signal #1."
  - True per PR #102. The cadence doc claims this is closed in the proof point on line 58 but the B.5 row still reads as forward-looking. Mark B.5 status as **shipped** in the table (the cadence doc is the only artifact tracking these slices since no per-slice feature stories exist).

- **Lines 56–58** (B.6 + proof point): "Reference plugin (out of noodle repo)…"
  - Status of this slice is unspecified. After B.5, the next gate is whether a sister repo actually exists. The doc doesn't say.

### 2.3 ADR 006 — Extensibility posture

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | Crisp two-page decision doc. |
| Diagram present and rendered | n/a | — |
| Flows / sequences described | n/a | Decision doc. |
| API / contract specified precisely | weak | Names trait targets (`Detector`, `Injector`, `Filter`, `ProviderCodec`) that have since been superseded by ADRs 015 + 021 (`Codec` + `Transform` + `RequestDetector`). The contract is stale. |
| Examples included | weak | One concrete example: "new providers go in `noodle-adapters/src/provider/<name>.rs`". After B.1–B.5, the destination crate for sinks is `noodle-sinks`; for cert: `noodle-cert-external`; for TLS: `noodle-tls`. ADR 006 lines 79–80 still says "audit/wire sinks go in `noodle-adapters/src/audit/` or `noodle-adapters/src/log/`." Stale. |
| Cross-references to related ADRs | weak | Names no related ADRs. Should at minimum cross-reference 015, 021, 039. |

**Specific weak passages:**

- **Line 9**: "v1 supports compile-time plugins only."
  - After B.5, `cargo build --target wasm32-unknown-unknown -p noodle-detect` succeeds (PR #102). The proxy host still uses compile-time plugins, but the plugin host topology is precisely WASM-based. The blanket "v1 supports compile-time plugins only" reads as wrong to a plugin author who finds this ADR via search.
  - Suggested rewrite: split the decision into "proxy host: compile-time" and "plugin host: WASM via `noodle-detect`" — and forward-reference ADR 039.

- **Lines 53–65** (When to revisit): The triggers are now partially satisfied (the WASM target builds, the carve-outs landed, the facade exists). The ADR should be promoted to a successor status that acknowledges the threshold has been crossed for the plugin topology.

- **Lines 67–84** (What this means in practice): file-path examples reference the pre-carve crate layout. Update or annotate.

### 2.4 ADR 001 — Component architecture

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | Long but well-structured. |
| Diagram present and rendered | good | Multiple. |
| Flows / sequences described | good | §8 indexes most of the corpus. |
| API / contract specified precisely | weak | §3.2 enumerates `noodle-core`, `noodle-domain`, `noodle-adapters`, `noodle-proxy`, `noodle-tap`, `noodle-viewer`, `noodle-embellish` (and the macOS shim). After B.1–B.5, the workspace also has `noodle-sinks`, `noodle-cert-external`, `noodle-embellish-core`, `noodle-detect`, `noodle-tls`. None of these are listed. |
| Examples included | n/a | Component listing. |
| Cross-references | weak | §8.12 (line 619–624) describes extensibility via ADR 006's pre-WASM framing. Doesn't cross-reference ADR 039. |

**Specific weak passages:**

- **§8.12 (line 621–622)**: "Compile-time plugins only for v1. Adding a new codec, transform, detector, or sink is a new file plus registration at startup."
  - Same issue as ADR 006. Add a sub-section §8.12.1 referencing ADR 039 and the plugin topology.

- **§3.2**: needs five new crate rows (the carve-outs) or a follow-on sub-section "post-039 carve-outs."

### 2.5 ADR 015 — Layered codec architecture

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | The most architecturally load-bearing doc; written carefully. |
| Diagram present and rendered | good | `015-layered-codec-architecture.png`. |
| Flows / sequences described | good | §8 walks a worked example (LLM cost attribution). |
| API / contract specified precisely | good | Trait surfaces at §3 + §4 are complete. |
| Examples included | good | §8 is a full worked example, §9 is the attachment cheat sheet. |
| Cross-references | good | Many. |

**Specific weak passages:**

- **§9 (line 478, "Attachment cheat sheet")**: tells a *proxy-host* author where to attach. A plugin-host author reads this and asks "where do I attach inside the `noodle-detect` facade?" The answer requires reading ADR 039 — but ADR 015 has no forward-reference.
  - Add a paragraph at the end of §9: "Plugin hosts (ADR 039) consume these traits via the `noodle-detect` facade; the same attachment rules apply, modulo the engine layer not running. See ADR 039 §2.3 for the synchronous-call shape."

### 2.6 ADR 016 — `CacheAndRelease` + `Extractor`

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | good | `016-cache-and-release-primitives.png`. |
| Flows / sequences described | weak | The primitives are described; no end-to-end use site is walked. |
| API / contract specified precisely | good | §3 + §4 traits + invariants. |
| Examples included | weak | Mentioned in §2 as the three open-coded implementations; no positive example yet. |
| Cross-references | good | — |

**Status:** still labelled "Proposed" (line 3). Track A.4 has shipped. Status line is stale.

### 2.7 ADR 019 — Endpoint-routed capability dispatch

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | missing | No companion diagram. |
| Flows / sequences described | weak | The 3-axis frame is named (§2.1) but no concrete flow walks a `(host, endpoint, direction) → capability chain` end-to-end. |
| API / contract specified precisely | weak | "Capabilities are compiled; the cell→chain mapping is config." The capability catalog is named but never enumerated. |
| Examples included | weak | Sub-bullets ("exfil monitoring is `request→upstream` with an observe capability") suggest cells; no example cell is written out. |
| Cross-references | good | — |

**Status:** "Drafted before code, pending Joe review" (line 3) — has been pending since 2026-05-16.

### 2.8 ADR 020 — `SideEffectSink` + Resolver wiring

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | missing | No companion diagram. |
| Flows / sequences described | good | §2.3 walks engine wiring. |
| API / contract specified precisely | good | `SideEffectSink::record(&self, effect: SideEffect)` is the contract. |
| Examples included | weak | Names four driven adapters (TracingSink, EventsJsonlSink, InMemorySink, MultiSideEffectSink); none is shown. |
| Cross-references | good | — |

**Specific weak passages:**

- **Lines 73–86 (driven adapters)**: names the adapters as living in `noodle-adapters`. After B.1, they live in `noodle-sinks`. Update or annotate.

### 2.9 ADR 021 — Detector vs Transform two-tier

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | missing | — |
| Flows / sequences described | good | Decision table at lines 60–71 is the contract. |
| API / contract specified precisely | good | Trait surface at lines 73–80. |
| Examples included | weak | `UserAgentDetector` is named; not shown. The `noodle-detect` facade re-exports it (line 62 of `lib.rs`); ADR 021 doesn't note this. |
| Cross-references | good | — |

### 2.10 ADR 022 — OTel collector embellishment plane

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | good | `022-data-and-embellishment-planes.drawio` (no PNG in `docs/images/`). |
| Flows / sequences described | good | Mermaid sequence at lines 115–137. |
| API / contract specified precisely | good | Decision section is concrete. |
| Examples included | good | The mermaid sequence is itself an example. |
| Cross-references | good | — |

**Plugin relevance:** a plugin author whose host gateway already runs an OTel collector will care about this ADR. ADR 022 doesn't say "the plugin host can attach its own OTLP sink to its `noodle-detect` output" — but that is exactly the integration pattern. Suggested addition: a §"Plugin host integration" naming this.

### 2.11 ADR 023 — Round-trip telemetry records + correlation IDs

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | good | `system-architecture.png` (line 17). |
| Flows / sequences described | good | §2.4 + §2.5 walk turn + agent-run boundary detection. |
| API / contract specified precisely | good | §4 pins the JSONL wire format. |
| Examples included | good | §4 is an example. |
| Cross-references | good | — |

**Plugin relevance:** `AttributionFacts.round_trip: Option<RoundTripRecord>` traces straight here. ADR 023 doesn't acknowledge the plugin facade. Minor fix: forward-reference 039 in the related-ADR list.

### 2.12 ADR 028 — `SessionStore` + marking-detector contract

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | missing | — |
| Flows / sequences described | good | §4 + §5 are the cell-by-cell contract. |
| API / contract specified precisely | good | Marking-detector trait shape pinned. |
| Examples included | good | §1.1 walks `api.anthropic.com`. |
| Cross-references | good | — |

**Plugin relevance:** `DetectContext.marking_store: Arc<dyn MarkingStore>` (at `crates/noodle-detect/src/context.rs:19`) requires the plugin author to supply a `MarkingStore` impl or use `InMemoryMarkingStore` from `noodle_adapters::marking`. ADR 028 doesn't mention the plugin facade. The trait is pure (no I/O) so structurally plugin-ready, but the doc should say so explicitly.

### 2.13 ADR 029 — `noodle-domain` crate

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | missing | — |
| Flows / sequences described | good | §2 type-family taxonomy. |
| API / contract specified precisely | good | The whole ADR is type-shape work. |
| Examples included | good | Per-family examples throughout §2. |
| Cross-references | good | — |

**Plugin relevance:** these types appear in `AttributionFacts.hints` and `artifacts`. The facade re-exports the families at `crates/noodle-detect/src/lib.rs:49-53`. ADR 029 §1 (line 130) states "No async runtime. No HTTP framework." That's plugin-ready by construction. Worth a one-line forward-reference to 039.

### 2.14 ADR 033 — Product architecture separation of concerns

| Criterion | Score | Notes |
|---|---|---|
| Professionalism and clarity | good | — |
| Diagram present and rendered | n/a | This is an architecture doc; uses inline tables. |
| Flows / sequences described | good | — |
| API / contract specified precisely | good | — |
| Examples included | good | — |
| Cross-references | weak | Line 10 references ADR 039 in the header. Line 161 still says "compile-time plugins (ADR 006)" without the §3 deployment-topology framing 039 introduces. |

### 2.15 ADRs 041 + 042 (post-B.5, 2026-05-31)

These ADRs landed two days after the plugin facade. Neither mentions plugins.

- **ADR 041 (tool_use accumulation)** — every plugin author writing a vendor codec must follow §2.1's per-block accumulation rule. ADR 041 references ADR 015 (the trait), ADR 017 (provenance), ADR 018 (request codecs), ADR 023 (round trips), ADR 029 (domain types) — but not ADR 039.
- **ADR 042 (codec side channel)** — the audit-emission contract. Every plugin codec must follow it. References ADR 015 §13, ADR 020, ADR 041, ADR 040. Not ADR 039.

**Recommendation:** every post-039 ADR that pins a trait-surface contract should forward-reference 039 in the related-ADR header so a plugin author reading it from inside the facade lands on the topology context.

---

## 3. Plugin-author UX gap analysis

A hypothetical engineer at a downstream company wants to add a new
attribution capability (a custom marker pattern, a new extracted field, a
request-classification heuristic) and ship it as a WASM plugin into their
existing LiteLLM or Bifrost gateway. Below is every question the docs do
not answer, organised by phase.

### 3.1 Onboarding

| Question | Answer in docs? | Where it should live |
|---|---|---|
| Where do I start? | No. There is no "getting started" entry point. | New `docs/guides/plugin-author-quickstart.md` |
| Which crate do I depend on? | Implicit only — `noodle-detect`. Not stated in any onboarding doc. | Plugin-author guide |
| Which trait do I implement to add a new detector? | ADR 021 names `RequestDetector`. The plugin-relevance is implicit (you implement it, your `.wasm` exports a registry that includes it). | ADR 021 should forward-reference 039 |
| Do I fork `noodle-detect` and add my detector inside, or do I depend on `noodle-detect` from my own crate and register from outside? | **Critical ambiguity.** Ship-shape v1 is "fork": the facade re-exports types but provides no `register_detector(…)` extension point. ADR 039 §7 (non-goals, lines 210–212) says "v1 ships static detector + transform registration at WASM compile time." This is the answer but it is buried in a non-goals section. | ADR 039 §3 (Embedding strategy) should make this an explicit subsection. |
| What's the minimum viable plugin? | Not answered. | Plugin-author guide with a "5-line detector example" |
| Versioning policy for `noodle-detect`? Stability guarantees? | Not stated. ADR 006 historically said "the trait surface is still moving." With B.5 shipped, what's the new posture? | ADR 039 §8 (Acceptance signals) should add a stability note. |

### 3.2 Authoring

| Question | Answer in docs? | Notes |
|---|---|---|
| What's the contract for a `RequestDetector`? | ADR 021 lines 60–80. Good. | — |
| What's the contract for a `Transform<E>`? | ADR 015 §4 + §5. Good. | — |
| What's the contract for a `MarkingDetector`? | ADR 028 §4. Good. | — |
| What's the contract for a `MarkingStore`? | ADR 028 §3, source at `crates/noodle-core/src/marking.rs:216-223`. Good. | — |
| What's the error-handling contract? | ADR 015 §13 (empty-on-error + audit-channel) and ADR 042. Good. | — |
| Can I write a custom `Codec`? | ADR 015 §3. Yes — and the trait shape doesn't change in the plugin context. But there's no plugin-specific authoring guide. | — |
| How does the `<noodle:NAME>VALUE</noodle:NAME>` marker grammar work? | ADR 015 §8.1; `noodle-core::MarkerScanner`; `crates/noodle-adapters/src/transform/marker_strip.rs`. Adequate. | — |
| If I want to detect a new marker tag (e.g. `<my-co:project>`), how? | Not explicitly answered. Story 034 (configurable marker grammar) is open and tracked under A.9. For a plugin author today, this means hardcoding their own scanner. | Story 034 should call out plugin-host implications. |
| What does the model self-tag look like in practice? | ADR 015 §8 is the worked example. Good. | — |
| How do I unit-test my detector? | Not answered for plugin authors. The proxy-host test patterns aren't directly applicable (no proxy in the loop). | New examples directory under `crates/noodle-detect/examples/` |

### 3.3 Building

| Question | Answer in docs? | Notes |
|---|---|---|
| What command do I run to build the WASM artifact? | ADR 039 §8 line 221 says `cargo build --target wasm32-unknown-unknown -p noodle-detect`. Good. | — |
| Do I need `cargo-component`, `cargo wasi`, or plain `cargo`? | **Not answered.** This is the guest-ABI question (see 2.1). | ADR 039 §3.1 (to be added) |
| What's the output artifact's shape? Raw `.wasm`, `.wasm` + `.wit`, `.wasm` Component? | **Not answered.** | ADR 039 §3.1 |
| Where does the artifact land? `target/wasm32-unknown-unknown/release/noodle_detect.wasm`? | **Not answered.** Conventional but not stated. | Plugin-author guide |
| What is the artifact size? Is there a stripped/release path? | **Not answered.** Operationally important for a downstream operator. | Plugin-author guide |
| Are there feature flags I should turn on/off? | The shipped `Cargo.toml` has no `[features]` section. Implicit "all on, all the time." | Plugin-author guide |
| Toolchain version requirements (rust 1.75+, etc.)? | Workspace `rust-version` is inherited. Not surfaced for plugin authors. | Plugin-author guide |
| Reproducibility (`Cargo.lock` discipline, deterministic builds)? | Not addressed. | Plugin-author guide |

### 3.4 Embedding

| Question | Answer in docs? | Notes |
|---|---|---|
| For LiteLLM (Python): what's the wasmtime-py call? | Named in ADR 039 §3 table; **no code sample**. | New `docs/guides/plugin-embedding-litellm.md` |
| For Bifrost (Go): what's the wasmtime-go call? | Named; no code sample. | New `docs/guides/plugin-embedding-bifrost.md` |
| For Portkey (Node): what's the jco call? | Named; no code sample. | New `docs/guides/plugin-embedding-portkey.md` |
| What's the host-side guest ABI? Exported function names? Argument types? | **Not answered.** The shipped facade is a Rust `pub fn detect`; what that looks like as a WASM export — including string marshaling, byte-buffer ownership, the type wrapping `Bytes`, `SmolStr`, `Vec<(SmolStr,SmolStr)>` — is not documented. | ADR 039 §3.1 + plugin-author guide |
| How does the host pass `request.body: Bytes`? | Not documented. WASM has no Rust `Bytes` type; the host marshals (ptr, len) pairs or pre-allocates a guest buffer the host writes into. | Plugin-author guide |
| How does the host pass `request.headers: Vec<(SmolStr, SmolStr)>`? | Not documented. | Plugin-author guide |
| How does the host receive `AttributionFacts`? Is it serialised to JSON, or returned via a pre-shared buffer, or via a series of getter calls? | Not documented. | Plugin-author guide |
| What's the lifecycle? Instantiate per-request? Per-process? Per-thread? | Not documented. The facade is stateless per-call (`detect()` is a free function modulo the `Clock` and `MarkingStore` injected via `DetectContext`), so per-process instance is the natural answer — but it's not stated. | ADR 039 §3 should add lifecycle guidance. |
| How does the host supply `Arc<dyn MarkingStore>`? You can't pass a `dyn Trait` across WASM. | **Critical gap.** The shipped `DetectContext` carries `Arc<dyn MarkingStore>` and `Arc<dyn Clock>` — these are host-side concrete types that don't cross the WASM boundary. Either (a) the WASM artifact owns the implementation and the host calls a flat function with no `dyn` parameters; or (b) the WASM imports a host-supplied callback for marking-store ops. Neither is documented. | This is the single biggest design question the embedding section has to answer. ADR 039 should pin it. |
| How does the host supply `Arc<dyn Clock>`? | Same as above. | Same. |
| Where do the resulting `Hint`s, `Artifact`s, `AuditEvent`s, `ResolvedRecord`, `RoundTripRecord` go in the host? | ADR 039 line 100 says "The host decides what to do with the returned `AttributionFacts`." OK in principle. In practice, the host needs sample code showing the OTLP-export pattern (per ADR 022) — that doesn't exist. | Plugin-author guide |

### 3.5 Testing

| Question | Answer in docs? | Notes |
|---|---|---|
| How do I unit-test my detector without a proxy? | Not answered. | Plugin-author guide + `examples/` |
| Are there reusable test fixtures (captured request/response pairs)? | `captures/` exists in the repo (referenced by ADR 019). Not exposed to plugin authors. | Document the `captures/` corpus + how to use it. |
| Is there a property-test harness for `RequestDetector` and `Transform`? | Not exposed. | Plugin-author guide |
| Is there a Rust-side smoke test for the WASM artifact (load via wasmtime-rust, call detect, assert facts)? | Not present. | Add `crates/noodle-detect/tests/wasm_smoke.rs` (or a sister test crate) |
| How do I test the host-side embedding glue? | Not answered. | Per-language embedding guides |
| What's the property test for "the facade preserves the round-trip invariant when no transform fires"? | Not articulated as a plugin-author test obligation. | Plugin-author guide |
| Deterministic clock for replay tests? | The `Clock` trait at `crates/noodle-detect/src/context.rs:32-34` is the answer. Mentioned in source comments (`FakeClock` is implied at line 31) but no `FakeClock` is shipped in the crate. | Add `FakeClock` to the crate or document where the proxy host's test version lives. |

### 3.6 Operating

| Question | Answer in docs? | Notes |
|---|---|---|
| Audit emissions — what does the host do with `AuditEvent::Errored`? | ADR 042 explains the contract but doesn't address the plugin host. The proxy host routes these through `SideEffectSink` (ADR 020); the plugin host has its own `AttributionFacts.audits` to handle. | Add plugin-operating guide |
| Error contract — does the WASM artifact trap, or return `Result`, or emit an audit and return empty? | The Rust source returns by value. The WASM ABI question (trap vs return) is conflated. | ADR 039 §3.1 |
| Performance budgets — what's the per-`detect()` call time budget? | Not stated. The post-parity perf bench (`docs/guides/codec-perf-bench.md`, 2026-05-31) measures legacy vs layered codec at ~245 MiB/s vs ~168 MiB/s for the proxy host. No equivalent measurement exists for the WASM-hosted facade. Per honest-engineering policy this is "not yet measured." | Plugin-author guide should explicitly say "not yet measured; benchmark before relying on hot-path embedding." |
| Memory budget — what's the WASM module's peak allocation? | Not stated. | Plugin-author guide |
| How do I debug a plugin from inside a Python host (e.g. wasmtime-py's debug hooks)? | Not addressed. | Per-language embedding guide |
| Logging — do tracing spans cross the WASM boundary? | Not addressed. `noodle-detect` depends on `tracing` (Cargo.toml line 30); whether `tracing` macros work inside a wasm32-unknown-unknown guest (subscriber needs a host-side adapter) is not stated. | ADR 039 + plugin-author guide |
| Versioning + upgrade path: my plugin pinned `noodle-detect 0.2.0`; noodle ships 0.3.0; what changed? | No CHANGELOG, no versioning policy. | Standard release-discipline doc (out of scope here but flag it) |
| Failure modes — what happens if the host doesn't supply a `MarkingStore`? | Today the type is non-`Option<…>` in `DetectContext`. The host must supply one. Documented in source; not in any operator doc. | Plugin-operating guide |

---

## 4. Diagrams audit

### 4.1 Diagrams that exist, by topology

| Topology / flow | drawio source | PNG rendered | Referenced from prose? |
|---|---|---|---|
| Endpoint topology | `docs/diagrams/system-architecture.drawio` | `docs/images/system-architecture.png` | ADR 039 line 16–18; ADR 023 line 17 |
| Gateway topology | `docs/diagrams/gateway-architecture.drawio` | `docs/images/gateway-architecture.png` | ADR 039 line 20–22 |
| Plugin topology | `docs/diagrams/plugin-architecture.drawio` (**byte-identical to gateway-architecture.drawio per `md5 a5d968fc…`**) | absent | not referenced from any ADR |
| Data plane → embellishment plane | `docs/diagrams/022-data-and-embellishment-planes.drawio` | absent in `docs/images/` (no `022-…png`) | ADR 022 line 107 |
| Layered codec stack | `docs/diagrams/015-layered-codec-architecture.drawio` | `docs/images/015-layered-codec-architecture.png` | ADR 015 |
| CacheAndRelease primitives | `docs/diagrams/016-cache-and-release-primitives.drawio` | `docs/images/016-cache-and-release-primitives.png` | ADR 016 |
| Hexagonal architecture | `docs/diagrams/architecture-hexagonal.drawio` | `docs/images/architecture-hexagonal.png` (+ misspelled `architecture-hexigonal.png`) | ADR 001 |
| Component relationships | `docs/diagrams/component-relationships.drawio` | `docs/images/component-relationships.png` | ADR 001 (presumably) |
| Noodle component-object model | `docs/diagrams/noodle-component-object-model.drawio` | absent in images | (not checked) |
| OSI mapping | `docs/diagrams/osi-mapping.drawio` | `docs/images/osi-mapping.png` | older docs |
| System context | `docs/diagrams/system-context.drawio` | `docs/images/system-context.png` | (not checked) |
| Mermaid flows (5 diagrams) | `docs/diagrams/flows.md` | inline | not referenced; freestanding |

### 4.2 Diagrams that should exist but don't

| Diagram | What it would show | Where it would be referenced |
|---|---|---|
| **Plugin-host call sequence** | Host gateway receives bytes → instantiates WASM module → calls `detect()` → consumes `AttributionFacts` → routes to OTLP. Sequence diagram, not architecture. | ADR 039 §3 |
| **Plugin topology architecture (replacing the duplicate)** | The actual plugin topology: host gateway, embedded WASM module, no proxy, no MITM, no `tap.jsonl`. Make explicit what the plugin host does *not* have. | ADR 039 §2 |
| **Crate dependency graph (post-B.5)** | Which crates depend on which. Specifically: which crates the plugin host pulls (`noodle-core`, `noodle-domain`, `noodle-detect`, `noodle-embellish-core`, `noodle-adapters` pure submodules) vs which it does not (`noodle-proxy`, `noodle-tap`, `noodle-sinks`, `noodle-cert-external`, `noodle-tls`, `noodle-macos-tproxy`). | ADR 001 §3, ADR 039 §2.1/2.2 |
| **Guest-ABI surface** | If the answer to §3.1 above is "Component Model", show the WIT world. If "raw extern C", show the C function signatures. | ADR 039 §3.1 (new) |
| **Three-topology side-by-side** | One picture, three columns. Endpoint / gateway / plugin. Same data plane components highlighted in each, missing ones greyed out. | ADR 039 §2 |

### 4.3 What's missing per existing diagram

- `plugin-architecture.drawio` — needs to be replaced or deleted. As-is, it actively misleads.
- `022-data-and-embellishment-planes.drawio` — needs a PNG export. The ADR 022 line 107 reference is to drawio only.
- `noodle-component-object-model.drawio` — has no PNG. May or may not be plugin-relevant; not investigated.

---

## 5. Change plan

Items are grouped by type; within each group, prioritised P0 (blocking) /
P1 (important) / P2 (nice-to-have), with size (small ≤1 file change, medium
2–5 files, large >5 files) and sequencing notes.

### 5.1 Update existing docs

| # | File | What to change | Why | Priority | Size | Sequence |
|---|---|---|---|---|---|---|
| U1 | `docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md` | Sync the `AttributionFacts` and `DetectContext` signatures in §2.3 to the shipped surface (`Option<&DetectResponse>`, `Option<ResolvedRecord>`, `Option<RoundTripRecord>`, drop `usage` field or pin it under `round_trip`). Acknowledge v1-stub status. Annotate `prior turn_id` and `dispatch-table override` removals. | The ADR and shipped contract disagree on the output shape. Single highest-impact correction in the corpus. | P0 | small (1 file, ~30 lines) | First — every other plugin-author doc references this surface |
| U2 | `docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md` | Add §3.1 "Guest ABI" pinning the choice (raw `wasm32-unknown-unknown` exports vs Component Model + WIT). Without this, U3/U4/U5 can't be written. | Plugin authors in non-Rust hosts cannot start without this. Critical gap. | P0 | small (1 ADR addition, ~30 lines) | First, alongside U1 |
| U3 | `docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md` | Add §3.2 "Host-supplied dependencies" — how `Arc<dyn MarkingStore>` and `Arc<dyn Clock>` cross (or do not cross) the WASM boundary. Almost certainly: the WASM module ships an `InMemoryMarkingStore`-equivalent internally and exposes a host-callback for cross-process persistence; the `Clock` is read via an imported `now_unix_ms()` host function. | The biggest unanswered design question in §3 today. | P0 | small | After U2 (needs the ABI choice) |
| U4 | `docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md` | Update §8 acceptance signals to reflect B.1–B.5 shipped status. Add a new signal #4 acknowledging "facade contract documented to match shipped code." | Status hygiene; closes the slice properly. | P1 | small | After U1 |
| U5 | `docs/adrs/040-post-parity-cadence.md` | Annotate B.1–B.5 as **shipped** in the slice table. Note B.6 status (sister-repo reference plugin). | The cadence doc is the only B-track tracker. Status must be current. | P1 | small (annotations in 1 table) | Standalone |
| U6 | `docs/adrs/006-extensibility-posture.md` | Split the decision: (a) proxy host = compile-time plugins (unchanged); (b) plugin host = WASM via `noodle-detect` (new — forward-reference 039). Update §"What this means in practice" file paths to reflect B.1–B.5 carve-outs (`noodle-sinks`, `noodle-cert-external`, `noodle-embellish-core`, `noodle-tls`, `noodle-detect`). Promote status to "decided + revisited 2026-06-01." | The ADR is dated 2026-05-09; B.5 closed the WASM gate on 2026-05-29. The ADR currently reads as wrong to a plugin author. | P1 | small | Standalone |
| U7 | `docs/adrs/001-component-architecture.md` | Add the five carved crates to §3.2. Update §8.12 to forward-reference ADR 039. | The canonical component list. Must be current. | P1 | small | Standalone |
| U8 | `docs/adrs/015-layered-codec-architecture.md` | Add a closing paragraph to §9 "Attachment cheat sheet" noting the same rules apply inside the `noodle-detect` facade (plugin context). Forward-reference 039. | Plugin authors land on this ADR for trait surfaces and need to know it applies to them. | P2 | small | Standalone |
| U9 | `docs/adrs/020-side-effect-sink-and-resolver-wiring.md` | Update §2.1 driven-adapter list to note relocation to `noodle-sinks`. Forward-reference 039 for plugin-host fact-routing. | Crate location is wrong post-B.1. | P2 | small | Standalone |
| U10 | `docs/adrs/021-detector-vs-transform-two-tier.md` | Note in the "Examples" section that `UserAgentDetector` is re-exported from `noodle-detect` at `noodle_adapters::request_detector`. | Closes the plugin-author lookup loop. | P2 | small | Standalone |
| U11 | `docs/adrs/028-session-store-and-marking-detector-contract.md` | Add a §"Plugin host integration" paragraph: the `MarkingStore` trait is pure and plugin-ready; `InMemoryMarkingStore` is re-exported via `noodle-detect`. | Marking is a load-bearing part of the plugin contract; deserves explicit mention. | P2 | small | Standalone |
| U12 | `docs/adrs/029-noodle-domain-crate.md` | Add a one-line forward-reference to 039 (the facade re-exports these vocabulary families). | Closes the loop. | P2 | trivial | Standalone |
| U13 | `docs/adrs/033-product-architecture-separation-of-concerns.md` | Update §3 (line 161) "compile-time plugins (ADR 006)" wording to reflect the post-039 plugin topology. | Stale framing. | P2 | small | Standalone |
| U14 | `docs/adrs/041-l5-coverage-tool-use-and-usage.md` and `docs/adrs/042-codec-side-channel-and-error-contract.md` | Add ADR 039 to the related-ADR header. | Every post-039 trait-contract ADR should anchor to the deployment topology. | P2 | trivial | Standalone |
| U15 | `docs/adrs/016-cache-and-release-primitives.md` | Flip status from "Proposed" to "current" since A.4 shipped. | Status hygiene. | P2 | trivial | Standalone (overlaps with cadence Track E) |

### 5.2 New docs to write

| # | Name | Location | Purpose | Audience | Est. sections | Priority | Size | Sequence |
|---|---|---|---|---|---|---|---|---|
| N1 | Plugin-author quickstart | `docs/guides/plugin-author-quickstart.md` | The 5-line "hello detector" path. From `cargo build --target wasm32-unknown-unknown -p noodle-detect` through host-side instantiation and a verified `AttributionFacts` round-trip. | Engineer at a downstream company picking this up cold | Onboarding · Authoring · Building · Embedding · Testing | P0 | medium (~3–5 files including code snippets) | After U2 + U3 (needs the ABI + host-dep contracts) |
| N2 | Plugin embedding — LiteLLM (Python) | `docs/guides/plugin-embedding-litellm.md` | Per-host glue: wasmtime-py instantiation, request/response marshalling, `AttributionFacts` consumption, OTLP forwarding from the plugin. | LiteLLM operator | Setup · Glue code · Operating | P1 | medium | After N1 |
| N3 | Plugin embedding — Bifrost (Go) | `docs/guides/plugin-embedding-bifrost.md` | As N2, for wasmtime-go. | Bifrost operator | Same | P1 | medium | After N1 |
| N4 | Plugin embedding — Portkey (Node) | `docs/guides/plugin-embedding-portkey.md` | As N2, for jco / wasmtime-node. | Portkey operator | Same | P2 | medium | After N1 |
| N5 | Plugin authoring guide — detectors & transforms | `docs/adrs/plugin-authoring.md` | Pure-doc reference: every trait a plugin author can implement, with shipped re-export paths, example shape, and the test idioms. Cross-links ADRs 015 + 021 + 028 + 029 + 042 from a plugin-author lens. | Engineer extending the facade with a new detector / transform / marking-detector | nine sections (one per re-exported trait family) | P1 | large (one doc but content-heavy) | After U1–U3, parallel with N1 |
| N6 | Per-slice feature stories — B.1 through B.5 (retroactive) + B.6 | `docs/features/done/048-B1-noodle-sinks-carveout.md` through `…052-B5-noodle-tls-and-wasm.md`, `docs/features/053-B6-reference-plugin-litellm.md` | Closes the gap that ADR 040 acknowledges (Track B has no per-slice stories). Retroactive for B.1–B.5; open story for B.6. | Anyone auditing what B.1–B.5 actually shipped | Standard story template | P2 | medium (6 small files) | Standalone |
| N7 | Three-topology architecture explainer | `docs/adrs/three-topology-explainer.md` | One-page (no diagram-only) explainer that walks endpoint vs gateway vs plugin side-by-side. Anchored to ADR 039 §2; expands the table at lines 39–44 with concrete per-topology data flows. | A new engineer or evaluator landing in the repo | Endpoint flow · Gateway flow · Plugin flow · What's the same · What's different | P2 | medium | After U1 |

### 5.3 New diagrams

| # | Name | drawio + PNG | What it shows | Where referenced | Priority | Size | Sequence |
|---|---|---|---|---|---|---|---|
| D1 | Plugin-host call sequence | `docs/diagrams/plugin-call-sequence.drawio` + `docs/images/plugin-call-sequence.png` | Sequence: host gateway accepts bytes → instantiates WASM module (cold or warm) → calls `detect()` with `(DetectRequest, Option<DetectResponse>, DetectContext)` → receives `AttributionFacts` → routes facts to OTLP exporter. Includes the host-supplied `MarkingStore` callback. | ADR 039 §3, N1 plugin-author quickstart | P0 | small (one diagram + PNG) | After U2 + U3 |
| D2 | Plugin topology architecture (replacing the duplicate) | overwrite `docs/diagrams/plugin-architecture.drawio` + add `docs/images/plugin-architecture.png` | The actual plugin topology: host gateway in the centre, `noodle-detect.wasm` embedded inside it, no proxy, no MITM, no tap.jsonl. Show the data flow into the host gateway from its existing client/upstream paths, and the OTLP output of attribution facts. | ADR 039 §2.3 line 24 | P0 | small | After U1 |
| D3 | Three-topology side-by-side | `docs/diagrams/three-topologies-comparison.drawio` + PNG | Three vertical columns: endpoint / gateway / plugin. Same component palette in each; greyed-out boxes where a topology lacks that component. | N7 three-topology explainer; ADR 039 §2 | P1 | small | After D1 + D2 |
| D4 | Crate dependency graph (post-B.5) | `docs/diagrams/crate-deps-post-B5.drawio` + PNG | Boxes for each workspace crate. Edges for `dependencies`. Plugin-host crates highlighted; proxy-host-only crates shown but greyed. | ADR 001 §3, ADR 039 §2.1/2.2 | P1 | small | Standalone |
| D5 | Guest-ABI surface | `docs/diagrams/plugin-guest-abi.drawio` + PNG | If choice is Component Model: render the WIT world. If raw exports: function-signature table. | ADR 039 §3.1, N1 quickstart | P1 | small | After U2 |

### 5.4 Code-adjacent changes (in `crates/noodle-detect/` and neighbours)

| # | Change | Why | Priority | Size | Sequence |
|---|---|---|---|---|---|
| C1 | Reconcile the v1 `detect()` body or its doc-comment. Either ship the user-agent dispatch the doc-comment claims (small slice: build a `RequestDetectorRegistry` with `UserAgentDetector`, run it, fill `hints`), or rewrite the doc-comment to say "v1 shape-only stub; returns empty facts; detector wiring lands in a follow-up." | Today the source code lies about its behaviour. This is the single most user-hostile artifact. | **P0** | small (rewrite comment) or medium (real wiring) | First. Recommend the comment fix landing immediately; the wiring is a separate slice. |
| C2 | Add `crates/noodle-detect/README.md` (~200 lines) covering: crate purpose, the public surface, the 5-minute quickstart, links to ADR 039 + N1. | A plugin author who finds the crate today sees no entry point. | P0 | small | Parallel with N1 |
| C3 | Add `crates/noodle-detect/examples/01_minimal_detector.rs` — call `detect()` with a hand-built `DetectRequest`, `InMemoryMarkingStore`, and `SystemClock`; assert `AttributionFacts` shape. | Working example beats prose. | P0 | small | After C1 |
| C4 | Add `crates/noodle-detect/examples/02_custom_marker_pattern.rs` — implement a tiny custom marker scanner as a `Transform<NormalizedEvent>`, plug into the local detect path, show the artifact emission. | Demonstrates the authoring path. | P1 | small | After C3 |
| C5 | Add `crates/noodle-detect/tests/wasm_smoke.rs` (or a sister crate `noodle-detect-wasm-smoke`) — uses `wasmtime` (the Rust embed) to load the built `noodle-detect.wasm`, calls into it, asserts a known output. Runs in CI on a `--features wasm-smoke` flag (so doesn't slow regular CI). | Catches regressions in the WASM target. ADR 039 §8 acceptance signal #1 is currently a hand-run check. | P1 | medium | After C1 |
| C6 | Add a `prelude` module to `noodle-detect`: `pub mod prelude { pub use crate::*; pub use noodle_core::layered::*; … }` so plugin authors write `use noodle_detect::prelude::*;` and get the working set in one line. | Ergonomics. The current re-export list is 12 lines of imports the author must type. | P1 | small | Standalone |
| C7 | Add `FakeClock` to `crates/noodle-detect/src/context.rs` (test-only via `#[cfg(any(test, feature = "test-util"))]`, or always-on; tiny). Doc-comment at line 31 already says "test: FakeClock returns a fixed value for snapshot tests" but no `FakeClock` is shipped. | Closes a doc/code gap. | P1 | trivial | Standalone |
| C8 | Add type aliases or a `HostInputs` builder: `DetectRequest::builder().method("POST").host("api.anthropic.com").path("/v1/messages").header("user-agent","claude-cli/0.4").body(bytes).build()`. The shipped `DetectRequest` has 5 public fields and uses `SmolStr` (`smol_str` is a workspace dep; not all host languages know it). | Ergonomics; lowers the barrier from "know SmolStr" to "use the builder." | P2 | small | Standalone |
| C9 | Add a host-callable JSON serialization for `AttributionFacts` (via existing `serde` derives — most of these types already derive `Serialize`). Useful for non-Rust hosts that want to receive the bundle as a JSON blob over the WASM boundary. Verify the dependency graph stays WASM-clean. | Reduces per-language marshalling burden. | P2 | small | After U3 (which decides marshalling) |

### 5.5 Sequencing summary

Strictly ordered first batch (gates the rest):

1. **U1 + U2 + U3** — pin ADR 039 to the shipped surface, decide the guest ABI, decide host-callback contract. None of the per-host guides can be written without these. (~half a day if the ABI choice is uncontroversial; could be a real architecture decision otherwise.)
2. **C1** — make the source code stop lying. Either a 1-line comment fix today, or the real user-agent-dispatch slice (~half a day).
3. **D1 + D2** — replace the duplicate plugin-architecture diagram and add the call sequence. (~2 hours of drawio + PNG export.)
4. **U5** — annotate the cadence doc with shipped status. Trivial.

Parallelisable second batch (after the gate is open):

5. **N1** + **C2** + **C3** — the quickstart README + minimal example. (~one day total.)
6. **U4** + **U6** + **U7** — bring 039 acceptance signals, 006 extensibility posture, and 001 component list current.
7. **D3** + **D4** + **D5** — the additional diagrams.

Background work (any time):

8. **U8 through U15** — the per-ADR cross-references and status flips.
9. **N5 + N7** — the authoring guide and three-topology explainer.
10. **C4 through C9** — additional examples, smoke test, prelude, builders.
11. **N2 + N3 + N4** — per-host embedding guides. These each take real engineering against a real host gateway; sequence as the corresponding host integration is prioritised, not all-up-front.

Standalone (Track E hygiene; do anytime):

12. **N6** — backfill B.1–B.5 feature stories.

---

## Appendix A. Specific code/doc divergences to fix

For ease of resolution, the load-bearing divergences from the audit, in
one place. Each is a single edit; all should land together when U1 lands.

| Divergence | ADR 039 says | Shipped code says | Resolution |
|---|---|---|---|
| `detect` second parameter | `response: &DetectResponse` (line 80) | `response: Option<&DetectResponse>` (`crates/noodle-detect/src/lib.rs:82`) | ADR follows code. |
| `DetectContext` fields | `session_id, prior turn_id, dispatch-table override, clock` (line 81) | `clock, marking_store, session_id` (`crates/noodle-detect/src/context.rs:17-24`) | ADR follows code. The `prior turn_id` and `dispatch-table override` fields are not shipped and should either be added explicitly to the roadmap or removed from the ADR. |
| `AttributionFacts.resolved` | `Resolved` (the map) (line 89) | `Option<ResolvedRecord>` (`crates/noodle-detect/src/facts.rs:29`) | ADR follows code; cross-reference ADR 020 §2.2. |
| `AttributionFacts.usage` | `Option<WireUsage>` (line 90) | absent — usage carried inside `round_trip.usage` per ADR 023 §4 | Drop `usage` from ADR 039 §2.3; add a note "usage carried under `round_trip.usage` per ADR 023." |
| `AttributionFacts.round_trip` | not in §2.3 snippet | `Option<RoundTripRecord>` (`crates/noodle-detect/src/facts.rs:32`) | Add to ADR §2.3. |
| `AttributionFacts.at_unix_ms` | not in §2.3 snippet | `u64` (`crates/noodle-detect/src/facts.rs:35`) | Add to ADR §2.3. |
| `detect()` doc-comment (source) | "synchronously dispatches the user-agent detector, mints correlation IDs from `DetectContext::marking_store`, and returns the assembled `AttributionFacts`" (`crates/noodle-detect/src/lib.rs:72-74`) | body returns empty facts; ignores request/response/marking_store | Rewrite source comment to state v1 stub status, or wire the user-agent detector (recommended: shape pin first; wiring as a follow-up slice). |
| Streaming variant | "A streaming variant produces facts incrementally as response bytes arrive…" (lines 105–108) | not shipped | Mark as future work in ADR 039. |
| Crate location of sinks | ADR 020 §2.1 lists sinks under `noodle-adapters` | shipped in `noodle-sinks` (B.1) | Annotate ADR 020. |
| Crate location of cert | ADR 034 names `noodle-adapters::cert::external` | shipped in `noodle-cert-external` (B.2) | Already handled per Track E sweep, per commit `2e0082b`. |
| Crate location of embellish mapper | ADR 031 names `noodle-embellish` | shipped split between `noodle-embellish-core` (pure) and `noodle-embellish` (CLI/SQLite) (B.3) | Annotate ADR 031. |
| Crate location of TLS primitives | ADR 011 (TLS MITM) names `noodle-adapters` | shipped in `noodle-tls` (B.5) | Annotate ADR 011. |

## Appendix B. Files cited

The following files are referenced by absolute path in this audit:

- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/Cargo.toml`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/src/lib.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/src/context.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/src/facts.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/src/request.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-detect/src/response.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-core/src/layered.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-core/src/marking.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-adapters/src/request_detector.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-adapters/src/transform/marker_strip.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-adapters/src/transform/attribution_inject.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-adapters/src/marking/in_memory_store.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/crates/noodle-adapters/src/marking/anthropic.rs`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/001-component-architecture.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/006-extensibility-posture.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/015-layered-codec-architecture.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/016-cache-and-release-primitives.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/019-endpoint-routed-capability-dispatch.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/020-side-effect-sink-and-resolver-wiring.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/021-detector-vs-transform-two-tier.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/022-otel-collector-embellishment-plane.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/023-roundtrip-telemetry-records-and-correlation-ids.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/028-session-store-and-marking-detector-contract.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/029-noodle-domain-crate.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/033-product-architecture-separation-of-concerns.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/039-deployment-topologies-and-the-noodle-detect-facade.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/040-post-parity-cadence.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/041-l5-coverage-tool-use-and-usage.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/adrs/042-codec-side-channel-and-error-contract.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/diagrams/plugin-architecture.drawio`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/diagrams/gateway-architecture.drawio`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/diagrams/system-architecture.drawio`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/diagrams/022-data-and-embellishment-planes.drawio`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/diagrams/flows.md`
- `/Users/josephbarnett/business/code/josephbarnett/noodle/docs/features/000-overview.md`
