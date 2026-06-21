# ADR 000 — Tracking ledger: implementation status across the ADR series

**Status:** living.
**Purpose:** A single place to read the implementation state of each
ADR. The ADR documents themselves capture *intent and contract*;
this file records *what is built today*. Updated as ADRs land,
ship, or get reversed.

**Status legend:**

| Glyph | Meaning |
|---|---|
| ✓ | **Shipped.** Code exists and exercises the ADR's design end-to-end against real traffic. |
| ◐ | **Partial.** Some of the ADR's seams are built; named gaps remain. |
| □ | **Designed only.** ADR is current; no implementation yet. |
| ✗ | **Superseded / reversed.** Replaced by a later ADR; see notes. |

Feature stories in `docs/features/done/` use their own numbering
(S1–S48+); they are the *slices* that landed ADR contracts, not the
ADRs themselves. Cross-references live in the per-ADR notes below.

---

## ADR ledger

| # | Title | Status | Anchor / notes |
|---|---|---|---|
| 001 | Component architecture | ✓ | Crate boundaries in `crates/noodle-*`. |
| 002 | Hexagonal and patterns | ✓ | Ports-and-adapters: `noodle-core` traits, `noodle-adapters` impls. |
| 004 | Attribution model | ✓ | `context.tool` attribution shipped via marking detector + ai-telemetry. |
| 006 | Extensibility posture | ◐ | Trait surface and WASM facade designed; runtime hot-load of WASM plugins on `noodle-proxy` itself not yet built. Feature S48 covered WASM plugin author experience. |
| 007 | Viewer architecture | ✓ | `crates/noodle-viewer` — HTTP/SSE/OODA modes shipped (S12–S18). |
| 011 | TLS MITM and noodle root CA | ✓ | `crates/noodle-tls`; CA self-signed mode (S2). |
| 014 | Transparent mode and QUIC MITM | ◐ | Transparent mode partial; entry-transport ADR 037 supersedes parts. |
| 015 | Layered codec architecture | ✓ | `Codec`/`Transform` traits shipped (S26, S29). |
| 016 | Cache and release primitives | ✓ | Part of the codec engine. |
| 017 | EventSource provenance | ✓ | Per-frame provenance carried through the engine (S17). |
| 018 | Normalized request + per-domain codec chain | ✓ | Endpoint-routed dispatch shipped (S19, S20). |
| 019 | Endpoint-routed capability dispatch | ✓ | Dispatch wired in `noodle-proxy`. |
| 020 | Side-effect sink and resolver wiring | ✓ | `SideChannelTx`, `SideEffectSink` shipped (S31). |
| 021 | Detector vs transform two-tier | ✓ | Detector + transform traits separate. |
| 022 | OTel collector embellishment plane | ✓ | `noodle-embellish` → `noodle-shipper` → OTLP (S42–S46). |
| 023 | Round-trip telemetry records and correlation IDs | ✓ | `RoundtripsSink`, `roundtrips.jsonl`, correlation block (S40, S40.a–c). |
| 024 | Fail-open and bypass | ✓ | Rip-cord health degradation shipped (S39). |
| 025 | Dispatch table | ◐ | In-code dispatch live; runtime/config-file externalization deferred. |
| 026 | Deployment lifecycle | ✓ | Build/run lifecycle documented. |
| 027 | tap.jsonl boundary format | ✓ | Envelope + DNS wire codec landed (S27). |
| 028 | Session store and marking detector | ✓ | `MarkingStore` + marking detector shipped. |
| 029 | noodle-domain crate | ✓ | `crates/noodle-domain` — typed vocabulary. |
| 030 | tap.jsonl decoded layer | ✓ | Decoded model + viewer rendering (S35). |
| 031 | Embellishment processor | ✓ | `crates/noodle-embellish` + `noodle-embellish-core`; ai-telemetry v0.0.2 mapping (S42). |
| 032 | Rama foundation | ✓ | `rama` is the proxy substrate in `crates/noodle-proxy`. |
| 033 | Product architecture separation of concerns | ✓ | Documented and reflected in the crate split. |
| 034 | Enterprise CA + external signing | ✓ | `crates/noodle-cert-external`; BYOCA-static (S37), Vault PKI (S38). |
| 035 | Endpoint-product coexistence | ✓ | Reflected in noodle-macos / noodle-proxy split. |
| 036 | macOS collector parity value cadence | ✓ | Parity cadence executed; `noodle-macos-tproxy` + `apps/noodle-macos`. |
| 037 | Entry transport | ✓ | `crates/noodle-macos-tproxy` — transparent capture on macOS. |
| 038 | Side-effects JSONL wire format | ✓ | `side_effects.jsonl` writer shipped. |
| 039 | Deployment topologies + noodle-detect facade | ◐ | Topology naming live; `crates/noodle-detect` facade exists; WASM-host integration tests partial. |
| 040 | Post-parity cadence | ✓ | Cadence ran; produced S40+ slices. |
| 041 | L5 coverage tool_use and usage | ✓ | tool_use accumulation + usage (S32). |
| 042 | Codec side channel + error contract | ✓ | Side-channel and error model in `noodle-core`. |
| 043 | Kubernetes gateway deployment (single Pod) | ◐ | Dockerfile + `deploy/k8s/{deployment,service,otlp-sink}.yaml` shipped; ops listener wired. End-to-end Pod demo against a real cluster not yet logged as acceptance. |
| 044 | Scalable cluster: CA service, fleet, Parquet data plane | □ | Designed; no `noodle-ca` service crate yet, no `ParquetSink` adapter, no fleet manifests. |
| 045 | Watchtower — in-path policy classification + action gating | □ | Designed; no `PolicyDecision` port, no `policy.*` OTLP attributes, no classifier impls. |
| 046 | Telemetry viewer — fleet observability over the data plane | □ | Designed; no GenAI/OpenInference semantic alignment in the shipper, no fleet-tier viewer. |
| 047 | Session brain — ephemeral per-session retrieval | ◐ | Rung 1 shipped end-to-end: `Brain` + `BrainObservation` in `crates/noodle-embellish-core/src/brain.rs` (5 unit tests + replay against real tap.jsonl); `Embellisher` observes per pair; `TelemetryRow.brain` carries it; 9 `brain_*` SQLite columns (idempotent ADD COLUMN migration); shipper emits `brain.*` OTLP attributes. Rung 1.5 (per-chain disambiguation via response `msg_id` propagation) and rungs 2-5 (semantic index, recall API, cross-session persistence, read+inject) deferred. E1 evidence at [`notes/e1-compaction-evidence.md`](../../notes/e1-compaction-evidence.md). |
| 048a | Design ↔ code gap review and remediation | ◐ | [`048-design-gap-review.md`]. G0 resolved; **G1** (lineage steal by interposed side-call) test missing — fixture `quota-and-title.fixture.json` seeded; **G2** (`pause_turn` closes turn incorrectly) pinned at `crates/noodle-core/src/marking.rs:459`; **G3** (operator directive `text`/`as` parsed but discarded — `DEFAULT_DIRECTIVE` lands instead) unfixed; R5 turn rollup unimplemented. **⚠ Numbering collision with 048b below — needs renumber.** |
| 048b | Inject / Extract: LLM self-classification | ◐ | [`048-inject-extract-llm-self-classification.md`]. Items 0–5, 8 shipped: `crates/noodle-adapters/src/transform/placement.rs` (all 7 placements), stateless injection gate w/ quota-probe skip, six-tag `crates/noodle-proxy/default-noodle.toml`. Items 6–7 (per-turn rollup, OTLP per-turn grain) not built; G3 directive-text wiring outstanding. |
| 049 | Sub-agent lineage | ✓ | `crates/noodle-adapters/src/marking/anthropic.rs`; 8 tests in `crates/noodle-adapters/tests/adr_048_sub_agent_state.rs`. Lineage anchor (spawn-prompt fingerprint) shipped end-to-end. The fragile per-agent-run / system-hash identity was **superseded by ADR 052's header-driven frame model, which is now live** (#9) — the "rewrite pending" caveat is resolved. |
| 050 | Session-state service: one port, pluggable backends | □ | [`050-session-state-service.md`]. Status proposed; no port/impl on `main`. Lifts the per-process in-memory marking-store limitation (ADR 028 §3) for multi-replica. Engine decision recorded (§2.5): **Valkey** (BSD-3) over AGPL Redis, **`fred`** Rust client, ElastiCache-for-Valkey or self-hosted Valkey+Sentinel for HA; throughput is not the deciding factor at this op rate. |
| 051 | Viewer "LEARNED" reveal + debugger IA | □ | [`051-viewer-learned-reveal.md`]. Full info architecture + LearnedStore pseudocode; no viewer panel built. `side_effects.jsonl` already carries `event_id`/`turn_id`/`agent_run_id` for the feed. Gated on ADR 052 marks reshape. |
| 052 | Turn / run / lineage — per-session `tool_use` frame tree | ✓ | [`052-turn-run-lineage-frame-tree.md`]; header-driven detector `crates/noodle-adapters/src/marking/frame_tree.rs` is **live** (`noodle-proxy/main.rs:162` → `mitm.rs:151` → `wirelog.rs:638-678`) and **proven end-to-end in a real cluster** — a live Claude session with parallel sub-agents renders as a `turn → invoke_agent frame → chat` tree in Tempo (PR #9 + branch). Replay oracle `tests/adr_052_frame_tree.rs` (corpus: 12 RT / 1 turn / 4 frames). **Two open notes:** (a) the clean stateless §5 extractor `crates/noodle-adapters/src/marking/record.rs` (story 058 slice 1) exists but is **not wired in** — record.rs-vs-frame_tree.rs keeper decision deferred; (b) mid-stream attach (proxy restart) orphans in-flight turns → [feature 063](../features/063-mid-stream-attach-turn-recovery.md). |
| 053 | Documentation taxonomy | ✓ | `docs/{adrs,architecture,guides,knowledge,features}`. ~85 docs + 36 source/config files migrated. (ADR 050 had been left in `docs/design/`; now relocated — `docs/design/` is empty.) `docs/architecture/*.md` moved but not yet freshened. |
| 054 | Cross-agent `<system-reminder>` convention | ✓ | `crates/noodle-proxy/default-noodle.toml` (`user_prepend`) + `crates/noodle-adapters/src/transform/placement.rs` + `crates/noodle-adapters/src/enhancer.rs` (idempotent dedup). Convention verified on Claude Code + OpenCode. |
| 055 | File-edit tracking per round trip | □ | [`055-file-edit-tracking.md`]. Designed only. Extracts edit count + files from `DecodedEvent::ToolUse` (Edit/Write/MultiEdit/NotebookEdit + Anthropic text-editor tool) in the embellisher; 4 new `file_edits_*`/`tool_use_count` SQLite columns (ADR 047 ADD COLUMN pattern); `file_edits.*`/`gen_ai.tool.*` OTLP attrs; viewer `ToolUseStatsPanel` + OODA badge. No new architecture — horizontal extension of the existing telemetry pipeline. |
| 056 | Context weight + carry-cost per round trip | ✓ | [`056-context-weight.md`]. Shipped (#8); `context.*` attributes (`cache_read_tokens`, `cache_creation_tokens`, `input_tokens`, `preamble_bytes`, `system_bytes`, `tools_bytes`) observed on **live spans in Tempo**. Decomposes carried context vs. marginal prompt from the `usage` block + request `system`/`tools` sizes. |
| 057 | OTel GenAI trace export — turn-grouped hierarchical spans | ✓ | [`057-otel-genai-trace-export.md`]. `trace = turn`; `build_resource_spans_payload` (`noodle-shipper/src/exporter.rs`) groups rows by `turn_id`, emits one `invoke_agent` span per `(turn,frame)` + one `chat` span per RT, side-calls off-tree. **Proven live in-cluster**: a parallel-sub-agent session renders `invoke_agent ROOT → sub-agent frames → chat leaves` in Tempo (stories 060/062). Dev harness `docker/otel-genai/` + offline emitter `crates/noodle-trace-emitter` (story 061). **Open:** the committed wiremock corpus regression test (story 060 acceptance: 1 trace / 4 invoke_agent / 12 chat) is not yet landed — behavior is proven, the regression guard is not. |

> **Intentional ADR-number gaps:** 003, 005, 008, 009, 010, 012, 013 were never written (numbering is sequence, not contiguous).
>
> **Open numbering issues to resolve:** two ADRs share number **048** (gap-review + inject/extract) — one should renumber. *(Resolved: ADR 050 was relocated from `docs/design/` into `docs/adrs/`, restoring ADR 053 taxonomy compliance and fixing the cross-reference link from ADR 048b.)*

## Coverage summary

- **Foundation (001, 002, 011, 015–022, 026–033)** — fully shipped. The codec engine, side-channel, marking detector, decoded layer, and embellishment pipeline are the working substrate the recent ADRs build on.
- **Telemetry path (023, 031, 038, 042)** — fully shipped end-to-end: `tap.jsonl` → `roundtrips.jsonl` / `side_effects.jsonl` → embellish → SQLite → shipper → OTLP/HTTP/gRPC.
- **Enterprise CA + signing (034, 037)** — fully shipped including BYOCA-static and Vault PKI backends.
- **K8s deployment (043)** — Dockerfile, manifests, ops listener all present; the named acceptance test ("Pod survives `kubectl rollout restart` without losing delivered rows") is a runbook execution that has not been formally logged.
- **Scaling and data plane (044)** — designed only. The biggest substrate gap.
- **Watchtower (045)** — designed only.
- **Fleet viewer (046)** — designed only.
- **Session brain (047)** — rung 1 shipped end-to-end (in-process observer + OTLP `brain.*` attrs); rungs 1.5–5 deferred.
- **Turn / lineage rework (048a/048b/049/052)** — **shipped and proven live.** ADR 052's header-driven frame tree is the live detector and reconstructs `session → turn → frame → round-trip` end-to-end in a real cluster (parallel sub-agents render correctly in Tempo). Two open notes: the clean stateless §5 `record.rs` is unwired (keeper decision deferred), and mid-stream attach orphans in-flight turns ([feature 063](../features/063-mid-stream-attach-turn-recovery.md)). Turn rollup and per-turn OTLP grain (048 R5 / items 6–7) remain unbuilt; G3 directive-text wiring outstanding.
- **OTel GenAI trace-export chain (052 §10 / 056 / 057 / stories 060–062)** — **proven end-to-end against the real product.** Live agent session → proxy (frame_tree detector) → embellish → shipper (`local-hier`, hierarchical spans) → collector → Tempo → navigable turn→frame→chat tree with `gen_ai.*` + `context.*` attributes. Remaining: the story-060 committed wiremock regression test; story 059 (OODA **viewer** render on live marks) and story 057 (viewer left-panel turn tree) are the viewer-side follow-ons. Dev viewing harness (`docker/otel-genai/`, offline emitter) shipped; prod collector stays ADR 044's separate repo.
- **Session-state service (050)** — designed only; the enabler for multi-replica (lifts the per-process marking store).
- **Viewer LEARNED reveal (051)** — designed only; gated on 052's marks.
- **Docs taxonomy (053) + system-reminder convention (054)** — shipped.

The angel-demo build-out (Tier 1+2 from the planning conversation) is
materially the work to take ADRs **044/045/046/047** from designed-only
to shipped on the substrate that ADRs 001–043 already provide.

## Maintenance

When an ADR ships:

1. Update its row's status glyph here.
2. Add a one-line anchor (crate path, done-feature ID, or commit
   range) so the next reader can find the code.

When an ADR is superseded or reversed:

1. Mark its row `✗` with the superseding ADR number.
2. Leave the row in place — the history matters.

This file is the canonical answer to *"is X built yet?"* — keep it
honest. If a row says ✓ and the code is missing a seam, downgrade to
◐ with a note.
