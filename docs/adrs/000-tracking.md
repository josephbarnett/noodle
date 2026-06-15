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

## Coverage summary

- **Foundation (001, 002, 011, 015–022, 026–033)** — fully shipped. The codec engine, side-channel, marking detector, decoded layer, and embellishment pipeline are the working substrate the recent ADRs build on.
- **Telemetry path (023, 031, 038, 042)** — fully shipped end-to-end: `tap.jsonl` → `roundtrips.jsonl` / `side_effects.jsonl` → embellish → SQLite → shipper → OTLP/HTTP/gRPC.
- **Enterprise CA + signing (034, 037)** — fully shipped including BYOCA-static and Vault PKI backends.
- **K8s deployment (043)** — Dockerfile, manifests, ops listener all present; the named acceptance test ("Pod survives `kubectl rollout restart` without losing delivered rows") is a runbook execution that has not been formally logged.
- **Scaling and data plane (044)** — designed only. The biggest substrate gap.
- **Watchtower (045)** — designed only.
- **Fleet viewer (046)** — designed only.
- **Session brain (047)** — rung 1 shipped end-to-end (in-process observer + OTLP `brain.*` attrs); rungs 1.5–5 deferred.

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
