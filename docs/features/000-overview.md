# noodle backlog — source of truth

This table is the **immutable record of remaining work**. It
changes ONLY by (a) completing an item — its `Status` flips and a
PR link is added — or (b) explicit scope addition by Joe. Any
other change is a `git diff` Joe reviews. Collapsing, regrouping,
or omitting rows is a forbidden, visible deletion in history.

33 items. 6 block the attribution product (2–6, 24 — items 2,
4, 24 done; 3 partial; 5, 6 open). 6 gate making the layered
core the default (7–12 — all open; gates the "delete the legacy
path" cleanup). 4 are the Claude Desktop / transparent track
(13–16 — all open). 3 cleanup (17–19). 2 parked (20–21). 1
supports product iteration after the loop closes (22). 1
viewer-mode cleanup (23). 4 downstream of the attribution
product close the macOS-collector parity gap (25–28; cadence
pinned in `docs/adrs/036-macos-collector-parity-value-cadence.md`
— 24, 26, 27 **done** with 25 retired; 28 out-of-repo). 5
post-parity tracks added 2026-05-28 from the full audit
(29–33; cadence pinned in
`docs/adrs/040-post-parity-cadence.md`).

**Macos-collector parity status (2026-05-28):** the in-repo
cadence is complete. PR #87 (040.a) + PR #88 (040.b) + PR #93
(040.c) + PR #94 (042) + PR #95 (043) close the runbook
end-to-end in noodle. Only item 28 (out-of-repo OTel collector)
remains.

The attribution product = inject a tagging directive into the
outbound prompt → model emits the tag → extract/strip the tag
from the response → resolve → attribute cost → **emit telemetry**
the downstream consumer (the downstream telemetry consumer / OTel collector) can ingest.
The loop closes through `tap.jsonl` + `side_effects.jsonl` +
`roundtrips.jsonl` (slice 040.b shipped the per-round-trip view).
`noodle-embellish` (slice 042) maps to ai-telemetry v0.0.2 in
SQLite. `noodle-shipper` (slice 043) emits OTLP to the collector.
Items 2–6 + 24 are what makes the product; 24 is done, 2–6 are
the open hot-path-hardening gap captured in `040-post-parity-cadence.md`
Track A.

Column conventions: `Status` is a single-word value
(`Done` / `Partial` / `Open` / `Parked`). Detail belongs in
`Notes`. `Priority` is a single tag (`P0` / `P1` / `P2` / `P3`)
with any gating clause stated in `Notes`.

| ID | Item | Why needed | Blocks product? | Status | Notes | Priority |
|----|------|-----------|-----------------|--------|-------|----------|
| 1 | Backlog hygiene: this file + a story file per actionable row (`026` already retired to `done/`; this row stays open until items 2–6 have story files) | Tracking was fiction — prerequisite for accountability | No (meta) | Partial | Stories exist for items 2–6, 22, 23; older rows still lack files | P0 |
| 2 | Restate `Detector`/`Injector`/`Filter` → `Transform`; wire marker-strip + attribution-injector onto layered path — story [`029`](done/029-wire-transforms-onto-layered-path.md) | Layered core is read-only without this | YES | Done | Filter slice (ADR 017 §2.1–2.4: `EventSource` provenance + `MarkerStripTransform`, byte-faithful e2e), Injector slice (PR #36), and Detector slice (ADR 021: `RequestDetector` + `UserAgentDetector` replaces the v1 inline UA stand-in) all shipped. ADR 021 clarifies story 029 §1's single-trait framing into a two-tier read-only/pipeline split. Engine response-encode wiring landed via item 4 (ADR 017 §7). | P0 |
| 3 | Request/inject pipeline in the engine — story [`030`](030-request-inject-pipeline.md) | Engine is response-only; cannot inject the directive | YES | Partial | Per-domain request codecs + injector shipped (PR #36); session-keying piece pending | P0 |
| 4 | Side-effect sink + `Resolver` wiring (+ viewer panel) — story [`031`](031-side-effect-sink-and-resolver.md), ADR [`020`](../adrs/020-side-effect-sink-and-resolver-wiring.md) | Extracted tag currently goes nowhere | YES | Done | Loop closed end-to-end via slices 031.a/b/c: `SideEffectSink` port + 4 adapters incl. `SideEffectsJsonlSink`; `ResponseFlow` symmetric encode (closes ADR 017 §7); `MarkerStripTransform` wired into `tap_setup`; full-loop e2e proves `Resolved { tool: "Claude Code" }` lands in `side_effects.jsonl`. Viewer panel (ADR 020 §7) shipped — `SideEffectsJsonlSource` adapter tails the sink, hub fans to frontend, `AttributionPanel` renders per-session Resolved rows, HTTP-mode chips show inline attribution. | P0 |
| 5 | L5 coverage: `tool_use`→`ToolCall`, usage/billing fields, resolve Q5 envelope shape — story [`032`](032-l5-coverage-tool-use-and-usage.md) | Cannot attribute **cost** without token/usage data | YES | Open | | P0 |
| 6 | `JsonChunk` `BodyFrame` variant (**response side only**; the request direction is single-stage per ADR 018 §9, item 6 does **not** apply there) — story [`033`](033-jsonchunk-bodyframe-non-streaming-response.md) | Non-streaming Anthropic responses invisible on layered path today | YES (non-streaming) | Open | | P0 |
| 7 | Enforce §16 error contract (codec `Errored` audit + divergence check) | Silent-empty failures become one-line reads | No (observability) | Open | | P1 |
| 8 | `CacheAndRelease`/`Extractor` (016) — bounded buffers; replace 3 open-coded buffers + unbounded `SseFrameCodec` buffer | Memory-safety under load | No | Open | | P1 |
| 9 | Async transform variant | Required when a transform calls a model (cost classifier) | YES (later) | Open | | P1 |
| 10 | Bounded inter-layer channels / backpressure | Unbounded sync fold blows memory under live load | No | Open | | P1 |
| 11 | Perf benchmark: legacy vs layered | 015 §15 said bench before committing; this is the live LLM path | No | Open | | P1 |
| 12 | Flip layered → default; delete legacy `ProviderCodec`/`OrderedCodecRegistry`/`StreamingDecoder` (this also kills the legacy `uri.host()` bug — folded here, not hidden) | Make the new path the only path | No | Open | Gated by 7–11 | P1 |
| 13 | Transparent-NE → engine wiring | Sysext doesn't route into the engine; required for `claude.ai`/Claude Desktop capture | No (separate use case) | Open | | P2 |
| 14 | `023` UDP/443 blackhole | QUIC→TCP fallback; story-011 completion | No | Open | | P2 |
| 15 | `024` NEDNSProxyProvider Swift extension + sysext glue | DNS h3/ech strip (Rust core done; Swift not) | No | Open | | P2 |
| 16 | `025` System Keychain CA install + ops doc → closes story `011` | One-click trust; retires the runbook | No | Open | | P2 |
| 17 | `005` session keying + directive injection | Old design; superseded by ADR 018 (per-domain request codecs) | Supports #3 | Retired | Moved to [`done/005-…`](done/005-session-and-directive-injection.md) (E.2, 2026-05-30). | P3 |
| 18 | `009` WebSocket adapter (+ `BodyFrame::WsMessage`) | Protocol coverage | No | Open | | P3 |
| 19 | `SseFrameCodec`: `\r\n`/`\r`/BOM/`id:`/`retry:` support | Latent: silently breaks first non-`\n` provider | No | Open | | P3 |
| 20 | `030` OpenAI codec on layered stack | — | No | Parked | No test path | P3 |
| 21 | `032` `DeclarativeCodec<Spec>` | — | No | Parked | Depends on #20 | P3 |
| 22 | `034` Configurable marker grammar + injection-prompt templates | Today both are hardcoded constants. Iterating prompts and rebranding markers needs config not recompiles. | Supports product iteration | Open | Eligible now that the attribution loop has closed | P1 |
| 23 | `035` Viewer mode consolidation: retire `events.jsonl`, fold SSE timing into HTTP mode | After the four-snapshot wire model shipped (031.b), `events.jsonl` is recomputable from `tap.jsonl` and unread by the viewer; SSE mode adds only per-frame timing over HTTP mode — better as a row-detail inset than its own top-level mode. | No (cleanup) | Open | Recorded, not currently prioritized | P3 |
| 24 | Telemetry round-trip records + correlation IDs — ADR [`023`](../adrs/023-roundtrip-telemetry-records-and-correlation-ids.md); story [`040`](040-roundtrip-telemetry-records-and-correlation-ids.md) (sub-stories [`040.a`](040.a-side-effect-correlation-block.md), [`040.b`](040.b-roundtripsink-and-roundtrips-jsonl.md), [`040.c`](040.c-turn-and-agent-run-boundary-detection.md)) | Job one for shipping. the downstream telemetry consumer and any OTel-collector consumer needs one telemetry record per HTTP round trip; today's per-side-effect file forces consumer-side correlation by `flow_id`. ADR 023 specifies `roundtrips.jsonl`, the `RoundTripSink` adapter, and `session_id` / `agent_run_id` / `turn_id` / `flow_id` correlation across all data-plane files. Subsumes `events.jsonl` retirement from story 035. | YES — telemetry-out is the product surface | Done | 040.a PR #87 (correlation block on every SideEffect); 040.b PR #88 (`RoundTripSink` + `roundtrips.jsonl` + `TapUsage` widening for service_tier / inference_geo / cache_creation TTL); 040.c PR #93 (turn + agent-run boundary detection with canonical-system-hash plumbed through `AnthropicMarkingDetector`). | P0 |
| 25 | **(Retired)** OTLP emission moved into #27 (`noodle-shipper`). | Was `OtlpSideEffectSink` (in-proxy OTLP adapter, ADR 022). Retired because the system architecture (`docs/diagrams/system-architecture.drawio`) puts OTLP emission **post-enrichment** in the separate-process shipper, not in the proxy. The in-proxy path re-coupled the proxy to collector availability and bypassed `noodle-embellish`. | — | Retired 2026-05-27 | Scope absorbed into #27. | — |
| 26 | `noodle-embellish` maps `tap.jsonl` → `ai-telemetry` v0.0.2 — ADR [`031`](../adrs/031-embellishment-processor.md); story [`042`](042-ai-telemetry-v0-0-2-mapping.md) | The validating consumer per ADR 031. Until the mapper produces target-schema rows in SQLite, the existing the telemetry backend shipper can't consume rust noodle. Pins the rust pipeline against the schema doc at `feature-ai-collector-macos/docs/adrs/ai-telemetry-event-schema.md`. | Supports #27, #28 | Done | PR #94. Mapper joins `roundtrips.jsonl` per-`event_id`, promotes attribution map into `context_json`, stamps `correlation_quality` (full / wire_only / attribution_only / minimal), uses proxy `event_id` as SQLite PK for idempotent re-runs. Schema parity test against external `ai-telemetry-event-schema.md`. | P1 |
| 27 | `noodle-shipper`: rollups SQLite → OTLP → collector — ADR [`022`](../adrs/022-otel-collector-embellishment-plane.md) §2 / §3; ADR [`031`](../adrs/031-embellishment-processor.md) §"hand off"; story [`043`](043-shipper-handoff-contract.md) | Pins the contract between `noodle-embellish` SQLite output and the OTel collector. E5 (removed) finds the existing macOS the telemetry backend shipper polls a shared SQLite (WAL, `delivery_status='pending'` cursor); 043 adopts the same cursor-on-flag pattern but emits OTLP instead of the proprietary the telemetry backend protocol. **This slice absorbs the OTLP-emission scope retired from old item #25** — the architecture puts OTLP at the shipper boundary, not in the proxy. | Supports #28 (full macOS-parity runbook); the OTLP boundary the proxy emits *into* via files, not directly | Done | PR #95. New `noodle-shipper` crate; `RollupsCursor` state machine (pending → in_flight → delivered \| retry → poison); OTLP/HTTP JSON exporter; binary CLI; `docs/guides/shipper-runbook.md`. Full chain live-probed end-to-end. | P1 |
| 28 | OTel collector with identity-resolution + cost-rate-card processors — ADR [`022`](../adrs/022-otel-collector-embellishment-plane.md) §2; story [`044`](044-otel-collector-separate-repo.md) | Owns `device_id` → "this person on this team" (feature 028's deferred scope), cost-rate-card application, redaction, sampling. **Lives in its own repo, not noodle.** Tracked here for visibility because it is the full-runbook proof point: "marker emitted → token-aware OTLP span lands in the telemetry backend." | Out-of-repo; on the critical path for the macOS-parity runbook | Open | Gated by #27 (`noodle-shipper` is the OTLP source). Noodle-side gate satisfied by PR #95's wiremock receiver test + `docs/guides/shipper-runbook.md`. | P2 |
| 29 | **Hot-path hardening cadence** — Track A in [`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md). Subsumes existing items #5 (L5 coverage — story [`032`](032-l5-coverage-tool-use-and-usage.md)), #6 (`JsonChunk` `BodyFrame` — story [`033`](033-jsonchunk-bodyframe-non-streaming-response.md)), and the §16 error-contract / bounded-buffer / async-transform / backpressure / perf-bench / flip-default / configurable-markers gates from items #7–#12 + #22. | Closes the layered-core production-ready gap; gates the "delete the legacy path" cleanup. The macOS-parity cadence shipped without these because the loop closes on the layered path the proxy already runs; production-readiness needs them before the legacy path can retire. | Internal — does not block product, gates internal cleanup | Open | Tracked in `040-post-parity-cadence` Track A | P1 |
| 30 | **ADR 039 componentization** — Track B in [`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md). Carve `noodle-adapters::sink` into `noodle-sinks` (proxy-host-only); carve `noodle-adapters::cert::external` into `noodle-cert-external`; split `noodle-embellish` mapper from CLI/SQLite; create `noodle-detect` facade crate compileable to `wasm32-unknown-unknown`. | ADR 039 names the three deployment topologies (endpoint, gateway, plugin); §4 lists the carve-outs required to make plugin embedding real. Empirical audit in ADR 039 §4 confirms `noodle-core` + `noodle-domain` are plugin-ready as-is. | Internal — unlocks gateway + plugin topologies | Open | Tracked in `040-post-parity-cadence` Track B | P1 |
| 31 | **Platform completeness** — Track C in [`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md). Subsumes existing items #13 (transparent-NE → engine wiring), #14 ([`023`](023-udp-blackhole.md) UDP/443 blackhole), #15 ([`024`](024-dns-h3-ech-strip.md) Swift `NEDNSProxyProvider` glue), #16 ([`025`](025-system-keychain-ca.md) System Keychain CA install). | Required for fleet deployment on macOS; the current proxy works behind explicit `HTTPS_PROXY` but the macOS endpoint product needs all four to ship. | Supports the gateway / endpoint deployment topologies in ADR 039 | Open | Tracked in `040-post-parity-cadence` Track C | P2 |
| 32 | **Enterprise CA hardening** — Track D in [`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md). Subsumes [`036`](036-cert-mint-service-trait.md) (cert-mint service trait), [`038`](038-external-cert-mint-vault.md) (external cert-mint via Vault — adapter exists in code; story is operationalisation), [`039`](039-rip-cord-health-degradation.md) (rip-cord / health degradation), and ADR 035 endpoint-product-coexistence implementation. ADR 034 + ADR 037 framework. | Required for enterprise deployment scenarios (managed device fleets, IT-supplied CA, ZTNA gateway coexistence). | Supports gateway deployment topology | Open | Tracked in `040-post-parity-cadence` Track D. Note: [`037`](037-byoca-static-mode.md) already shipped (slice S18). | P2 |
| 33 | **Backlog hygiene + ADR doc-drift cleanup** — Track E in [`docs/adrs/040-post-parity-cadence.md`](../adrs/040-post-parity-cadence.md). | The audit (2026-05-28) found shipped feature files still labelled `open` and several ADRs with status not matching shipped state. Backlog hygiene is meta-work but the corpus is the source of truth. | Internal — no product blocker | Done (partial) | 2026-05-30 sweep: E.1 (Status: lines already correctly flagged by #96 audit, no work needed). E.2 retired `005` and `028` to `done/`. E.3 promoted ADR 034 → `current` (S17/S18/S19/B.2/B.5 all shipped). E.4 created stories [`045`](045-shipper-otlp-grpc-transport.md), [`046`](046-shipper-otlp-auth-headers.md). E.5 ADR 019 review remains a Joe-only gate. | P3 |

## Shipped (verifiable in `git log` / merged PRs, not claimed here)

Layered trait surface + codecs + engine + proxy wiring landed via
PRs #16–#33: `Codec`/`Transform`/registries (`noodle-core::layered`),
`DnsWireCodec` + `StripH3`/`StripEch`, `SseFrameCodec`,
`LayeredAnthropicCodec`, `InspectionEngine`, `WireLogLayer`
engine wiring + `NOODLE_LAYERED_CORE`, origin-form host fix.
**This is plumbing — read-only decode, verified live in the
viewer. It is NOT the attribution product (items 2–6).**

Done feature stories are in `docs/features/done/` (001–022).
