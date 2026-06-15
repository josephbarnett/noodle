# 040 — Post-parity value cadence

**Status:** current.
**Companion to:** [`036-macos-collector-parity-value-cadence.md`](036-macos-collector-parity-value-cadence.md) (the prior thrust — macOS-collector parity, complete in-repo). This doc organises the work that remains after that cadence shipped.
**Audience:** Anyone picking up the next slice. Five tracks of work; each track has value-ordered slices the way 036 did.

---

## Goal

The macOS-collector parity thrust is complete in noodle (PRs #87, #88, #93, #94, #95). Three things still need work:

1. **Layered-core production-ready** — the proxy ships with two codec paths (legacy + layered) because the layered path isn't production-ready yet. Track A closes that gap and gates the legacy-path delete.
2. **Componentization (ADR 039)** — the same codebase needs to serve three deployment topologies (endpoint proxy, gateway proxy, plugin in existing LLM gateways). The carve-outs Track B specifies make plugin embedding real.
3. **Platform completeness + enterprise hardening** — Track C (fleet macOS deployment) and Track D (enterprise CA + coexistence) the macOS endpoint product needs to ship. Required for any deployment beyond `HTTPS_PROXY=…`.

Plus the meta-track:

4. **Backlog hygiene** — corpus drift cleanup (Track E).

## Tracks

### Track A — Layered-core production-ready

The layered path closes the attribution loop in production today. The legacy path still ships because the layered path has unfinished hot-path concerns. This track closes them so the legacy path can retire.

| Slice | What | Why | Item # |
|---|---|---|---|
| A.1 | **L5 coverage** — `tool_use` → `ToolCall`; decoded usage / billing fields; ADR 029 §Q5 envelope shape | Cost attribution on the layered path requires token decoding. Story [`032`](../features/032-l5-coverage-tool-use-and-usage.md) | #5 |
| A.2 | **`JsonChunk` `BodyFrame`** variant — response-side only (request stays single-stage per ADR 018 §9) | Non-streaming Anthropic responses are invisible on the layered path today. Story [`033`](../features/033-jsonchunk-bodyframe-non-streaming-response.md) | #6 |
| A.3 | **§16 error contract enforcement** — every codec's empty-on-error emits exactly one `Errored` audit; divergence test | Observability of silent failures | #7 |
| A.4 | **`CacheAndRelease` / `Extractor`** — bounded buffers; replace 3 open-coded buffers + the unbounded `SseFrameCodec` buffer (ADR 016) | Memory safety under load | #8 |
| A.5 | **Async `Transform` variant** | Required when a transform calls a model (cost classifier) | #9 |
| A.6 | **Bounded inter-layer channels + backpressure** | Unbounded sync fold blows memory under live load | #10 |
| A.7 | **Perf benchmark — legacy vs layered** | ADR 015 §15 promised this before delete-legacy | #11 |
| A.8 | **Flip layered → default; delete legacy `ProviderCodec` / `OrderedCodecRegistry` / `StreamingDecoder`** | The cleanup A.3–A.7 are gating | #12 |
| A.9 | **Configurable marker grammar + injection-prompt templates** — feature [`034`](../features/034-configurable-marker-and-injection-prompts.md) | Hardcoded constants today; product iteration needs config, not recompiles | #22 |

**Proof point.** After A.8, `noodle-proxy` ships one codec path. After A.9, marker grammar + injection prompt are configurable without a rebuild.

**Dependencies between slices.** A.1, A.2, A.3, A.9 are independent. A.4 / A.5 / A.6 land in any order. A.7 needs the codecs in a stable state. A.8 needs A.3 + A.4 + A.6 (at minimum) before delete-legacy is safe.

**Recommended starting slice.** A.1 — fills the cost-attribution gap and feeds A.7's benchmark.

### Track B — ADR 039 componentization

ADR 039 names three deployment topologies (endpoint, gateway, plugin). The plugin topology requires the host-independent crates to be plugin-embedable. The audit in ADR 039 §4 confirmed `noodle-core` + `noodle-domain` are ready as-is; the rest needs carve-outs.

| Slice | What | Notes |
|---|---|---|
| B.1 | **Carve `noodle-adapters::sink` → new `noodle-sinks` crate** (proxy-host-only) | The `SideEffectSink` port stays in `noodle-core`. The file-backed `SideEffectsJsonlSink` + `RoundTripSink` + `TracingSink` + `MultiSideEffectSink` move out. |
| B.2 | **Carve `noodle-adapters::cert::external` → new `noodle-cert-external` crate** | Vault PKI signer; needs `reqwest` + runtime. Plugin host doesn't need it (the host gateway already terminates TLS). |
| B.3 | **Split `noodle-embellish`** — pure mapper library + CLI/SQLite binary | Plugin hosts call the mapper directly; the binary stays as the file-mode operational entrypoint. |
| B.4 | **Create `noodle-detect` facade crate** — synchronous `detect(req, resp, ctx) → AttributionFacts` API; re-exports the host-independent types + pure-logic adapter submodules | The plugin host entry point. WASM-compilable. |
| B.5 | **WASM target verification** — `cargo build --target wasm32-unknown-unknown -p noodle-detect` succeeds without `cfg(not(target_arch = "wasm32"))` guards in the host-independent crates | Closes ADR 039 §8 acceptance signal #1. |
| B.6 | **Reference plugin (out of noodle repo)** — likely against LiteLLM as the most-used Python LLM gateway | Closes ADR 039 §8 acceptance signal #3. Sister repo. |

**Proof point.** After B.4 + B.5, a sister repo can `cargo build` the WASM artifact and embed it in any host language. After B.6, the plugin topology is demonstrably real, not just designed.

**Recommended starting slice.** B.4 — the facade pins the boundary the other carve-outs orient around. B.1–B.3 can land after.

### Track C — Platform completeness (fleet macOS deployment)

The current proxy works behind explicit `HTTPS_PROXY` for any client. The macOS endpoint product needs these four to ship without that.

| Slice | What | Story | Item # |
|---|---|---|---|
| C.1 | **Transparent-NE → engine wiring** | (no story file) — sysext doesn't route into the engine; required for `claude.ai` + Claude Desktop capture | #13 |
| C.2 | **UDP/443 blackhole** | [`023`](../features/023-udp-blackhole.md) — QUIC→TCP fallback; closes feature 011 | #14 |
| C.3 | **NEDNSProxyProvider Swift sysext** | [`024`](../features/024-dns-h3-ech-strip.md) — Rust core done; Swift not | #15 |
| C.4 | **System Keychain CA install + ops doc** | [`025`](../features/025-system-keychain-ca.md) — one-click trust; retires the manual runbook | #16 |

**Proof point.** After C.1–C.4 + Track D, an installer hands the user a working endpoint product with no `HTTPS_PROXY` knowledge required.

### Track D — Enterprise CA + coexistence hardening

| Slice | What | Story |
|---|---|---|
| D.1 | **Cert-mint service trait** | [`036`](../features/036-cert-mint-service-trait.md) |
| D.2 | **External cert-mint via Vault** | [`038`](../features/038-external-cert-mint-vault.md) — adapter exists in code; story is operationalisation |
| D.3 | **Rip-cord / health degradation** | [`039`](../features/039-rip-cord-health-degradation.md) — ADR 024 §4 escalation |
| D.4 | **ADR 035 endpoint-product-coexistence implementation** | No story file yet; needs runtime detection + per-product coexistence rules |

**Note.** Feature 037 (BYOCA static mode) already shipped (slice S18). It's the first slice of this track that landed.

### Track E — Backlog hygiene + ADR doc-drift cleanup

Meta-work. The audit on 2026-05-28 found seven shipped feature files still labelled `open` and four ADRs whose `Status:` doesn't match shipped state.

| Slice | What |
|---|---|
| E.1 | **Flip Status: lines on shipped feature files** — `040`, `040.a`, `040.b`, `040.c`, `042`, `043`, `E1`–`E5` are all shipped but still labelled `open` |
| E.2 | **Retire superseded / parked files** — feature `005` (superseded by ADR 018; item #17) → `done/`; feature [`028`](../features/028-embellishment-addon-layer.md) (deferred since 2026-05-16; identity-resolution scope moved into slice 044) → `done/` with a pointer note |
| E.3 | **Promote ADRs from `proposed` → `current`** — ADR 034 (enterprise CA + external signing) where supporting features have shipped; ADR 016 (cache & release) folded into Track A.4 |
| E.4 | **Open question docs** — file the 043 deferred follow-ups (OTLP/gRPC support, auth headers) as feature stories |
| E.5 | **ADR 019 review + finalisation** — drafted-pending-review; the dispatch-table v2 (ADR 025) references it as load-bearing |

**Recommended starting slice.** E.1 — five minutes per file; eliminates the "is this shipped?" confusion the audit found.

## Cross-track parallelism

Tracks A through E are mostly independent. Suggested parallelism if more than one person is working:

- **Person 1** drives Track A (hot-path hardening — production-ready cleanup; the larger thrust).
- **Person 2** drives Track B (componentization — the carve-outs that unlock the gateway + plugin topologies).
- Tracks C + D can run in parallel with A + B once a macOS dev is involved; both depend on Track A only loosely (the layered path needs to be production-ready before the endpoint product ships, but C.1–C.3 + D.1–D.4 can lay groundwork).
- Track E is fire-and-forget; can be done in 20-minute slices alongside any other work.

## Priorities

**P0:** None. The product loop closes on the in-repo code today.

**P1:** Tracks A + B. A unblocks the legacy-path delete (debt that grows the longer it sits); B unblocks the gateway + plugin deployment topologies (revenue surface, not pure refactor).

**P2:** Tracks C + D. Needed for the macOS endpoint product to ship for non-developer users.

**P3:** Track E. Important for corpus accuracy but doesn't gate any code.

## What this cadence is NOT

- Not a project plan with dates. Each slice has a story file or a clear scope; the order is recommended, not mandated.
- Not a re-statement of every ADR. ADRs are the durable design; this cadence indexes them by value-ordered work.
- Not a substitute for the `000-overview.md` backlog. The overview is the immutable row-level record; this cadence is the value-ordered narrative for the next thrust.
