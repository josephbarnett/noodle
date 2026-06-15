# decisions/refactor-plan.md

Documents in `docs/decisions/` that contain enduring architectural content
mixed with obsolete material. Each entry lists what to keep and what to
strip; refactor against `docs/adrs/001-component-architecture.md` and the canonical
ADRs in `docs/adrs/`.

A pre-refactor copy of each file is kept in `.delete/` for comparison.

---

## `architecture.md`

(was `docs/adrs/001-architecture.md`)

### Keep

- §3 the layered model — referenced by ADR 015 (`docs/adrs/015-layered-codec-architecture.md`) as the framing it codifies. Lift the diagram and the model description; drop the OSI cross-reference text where it adds noise.
- §6 the rama building-block mapping table (L1 transport → upper layers) — the layer-to-rama-primitive table is factually accurate for the layers it names. The Rust composition snippet that follows the table is dead and goes.
- §8 security considerations — current. CA-key handling, audit policy, tag-leakage threat model, prompt-side leakage threat model all still apply.
- §9 build choices — Rust 2024, Tokio, rama, BoringSSL, tracing, workspace layout. Mostly current; workspace-layout bullet needs a one-line update (the seven-crate set per `docs/adrs/001-component-architecture.md` §2, not the four crates listed today).

### Strip

- §2 goals / non-goals — restated and superseded by `docs/adrs/001-component-architecture.md` §1; remove.
- §4 core abstractions — the `LlmAdapter` and `TagPolicy` Rust snippets and the `Session` / `SessionStore` snippet. Replaced by `Codec` + `Transform` + `RequestDetector` (ADR 015 §3; ADR 021). The session model in `noodle-core` exists; describe it in prose, not Rust.
- §5 request lifecycle mermaid sequence — references `adapter.matches()`, `adapter.inject_directive()`, `adapter.decode()`, `adapter.encode()`. The lifecycle is now per-cell dispatch by `(domain, endpoint, direction)` per ADR 019. Replace with a sequence that names the current operations.
- §6 the composed rama service Rust snippet — references `LlmInspectionLayer`, `AddInputExtensionLayer`. Live composition is in `crates/noodle-proxy/src/mitm.rs` and `tap_setup`.
- §7 layer-by-layer design notes — the layer descriptions themselves are still right, but the `LlmAdapter` and `TagPolicy` trait references and the `openai.rs` / `anthropic.rs` file-naming references are stale. Re-state the layer descriptions against the current interfaces.
- §10 open questions — all resolved (marker format, turn-boundary detection, adapter mismatch fallback, multi-modal). Reference the ADRs that resolved them, drop the open-question framing.
- §11 companion docs — references ADR 002 and old diagrams. Recompute the link list against the current corpus (`docs/adrs/001-component-architecture.md`, the four canonical diagrams, current ADRs).

### Acceptance

The refactored document describes the architecture as it is, not as it was
first drafted. No references to `LlmAdapter` or `TagPolicy`. No Rust
snippets for trait shapes that have been superseded. The layered model,
security posture, and build choices remain.
