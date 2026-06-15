# noodle — design gap status (post-architecture-review + ADR 028)

Status of every item in `doc-gaps.md` after the architecture-review PR (#54)
and ADR 028 (SessionStore + marking-detector contract).

Going through doc-gaps.md section by section, marking each:
✅ resolved · ⚠️ partial · ❌ still open

## §1 — First-class concerns named but no design

| Item | Status | Where addressed |
|---|---|---|
| The dispatch table | ✅ | ADR 025 |
| Watchtower control port | ❌ | ADR 001 §8.16 still says "planned" |
| Header rewriting | ❌ | ADR 001 §8.16 |
| Body-level model rewrite | ❌ | ADR 001 §8.16 |
| Provider translation | ❌ | ADR 001 §8.16 |
| Sensitive-content protection | ❌ | ADR 001 §8.15 still "planned" |
| Cost feedback (canonical cost-record shape) | ❌ | named only |
| Per-cell marking detector contract | ✅ | ADR 028 |

**2 of 8.**

## §2 — Crates / types asserted as canonical but no specification

| Item | Status |
|---|---|
| `noodle-domain` crate | ⚠️ ADR 001 §3.2 + restored `coverage-roadmap.md` cover shape and roadmap; **no ADR ratifies the crate**, no type definitions, no schema. AGENTS.md still lists six crates. |
| `SessionStore` | ✅ ADR 028 §3 |
| `ResolvedRecord` / `Resolved` | ⚠️ ADR 027 §5 pins which sink it lands on; canonical field shape still spread across ADR 020 + ADR 004 |
| Cross-request correlation scope | ✅ ADR 028 §3 + §4 (SessionStore is the cross-request surface; marking detector is the derivation rule) |
| `AuditEvent` / `AuditKind` | ❌ ADR 027 §5 says `Audit` lands on side-effects.jsonl; canonical `AuditKind` enum + per-kind `detail` schema still unspecified |

**2 of 5 fully resolved, 2 partial.**

## §3 — Wire-format references

| Target | Status |
|---|---|
| claude.ai chat-completion | ❌ still only in ADR 018 evidence |
| Google Gemini `streamGenerateContent` | ❌ |
| OpenAI Chat Completions (legacy) | ❌ |
| MCP over SSE / JSON-RPC | ❌ |
| DNS HTTPS / SVCB records | ❌ covered inline in ADR 023 §3.3, no standalone kb-articles ref |

**0 of 5.**

## §4 — Schemas, catalogs, examples

| Item | Status |
|---|---|
| `tap.jsonl` schema + worked example | ✅ ADR 027 |
| `SideEffect` / `Resolved` JSON wire shape | ⚠️ ADR 027 §5 pins the split (artifact → tap.jsonl extractions; rest → side-effects.jsonl); the `side-effects.jsonl` schema itself still not formally specified |
| Shipped codec / transform / detector catalog | ❌ ADR 025 §3.5 mentions `noodle catalog list` CLI; no reference doc enumerates today's catalog |
| macOS sysext bundle / lifecycle spec | ✅ ADR 026 §2.1 + ADR 023 §3.4 |
| CA file layout | ✅ already adequate (ADR 011) |

**3 of 5, 1 partial.**

## §5 — Architectural conflicts

| Conflict | Status |
|---|---|
| Single `tap.jsonl` vs multiple JSONL | ✅ ADR 027: two distinct sinks (`WireSink` → tap.jsonl; `SideEffectSink` → side-effects.jsonl), each with a clear purpose |
| Marks + extractions on tap line vs separate `SideEffectSink` plane | ✅ ADR 027 §5 pins exactly: extractions on tap.jsonl; hint/audit/resolved on side-effects.jsonl |
| OTLP downstream of file vs OTLP shipped from noodle-adapters | ❌ ADR 022 still carries `OtlpSideEffectSink` framing; ADR 001 principle 7 says proxy does not ship OTLP directly. **Live disagreement.** |
| `turn_id` / `parent_session_id` stamped at probe time vs derived from state | ✅ ADR 028: marks are stamped at flow open by the per-cell marking detector reading a typed `SessionStore` handle (revised ADR 021 §6); written back at flow close. |
| `noodle-tap` one sink vs umbrella | ⚠️ ADR 001 §3.5 clarifies spec — one file-based `WireSink`. Whether the code matches is a separate question. |

**3 of 5 fully resolved, 1 partial, 1 still actively contradictory** (OTLP location).

## §6 — Diagram gaps

| Diagram | Status |
|---|---|
| Request flow (layered data flow) | ⚠️ ADR 001 §6.1 has mermaid; layered L0–L5 drawio still pending |
| Response flow (layered data flow) | ⚠️ ADR 001 §6.2 has mermaid; drawio pending |
| Tap-write flow | ⚠️ ADR 001 §6.3 has mermaid; drawio pending |
| Cell-dispatch flow | ❌ ADR 019 still has no diagram |
| TLS-MITM cert-minting flow | ❌ ADR 011 §3 / §5 has mermaid; drawio pending |
| `flows.md` / `type-model.md` audit | ❌ not audited |

**0 of 6 fully resolved, 3 partials.**

## §7 — Process / worked-example gaps

| Item | Status |
|---|---|
| "Add a new provider" worked example | ⚠️ ADR 025 §7 shows the Cohere case at the cell-binding level; codec / transform / detector Rust files + test fixtures still not laid out |
| "Add a new dispatch cell" worked example | ✅ ADR 025 §3.1 + §7 |
| "Write a new transform with buffering" worked example | ❌ |
| Async transform / detector story | ❌ flagged in ADR 015 §12 and ADR 024 §8 deferred |

**1 of 4, 1 partial.**

## §8 — Status hygiene

| Item | Status |
|---|---|
| ADR 016 status "Proposed" | ❌ |
| ADR 017 status drift | ❌ |
| ADR 018 status | ✅ updated to "current" |
| ADR 019 status drift | ❌ |
| ADR 020 status drift | ❌ |
| `docs/architecture/architecture.md` refactor | ❌ still pending |
| `flows.md` / `type-model.md` audit | ❌ |
| `docs/guides/quic.log` stray file | ❌ |

**1 of 8.**

---

## Summary — the four load-bearing blocks

| Block | Status |
|---|---|
| 1. The `noodle-domain` crate | ⚠️ — shape and roadmap covered; no ADR ratifying the crate; no type catalog |
| 2. The dispatch table format | ✅ — ADR 025 |
| 3. `SessionStore` + per-cell marking detector contract | ✅ — ADR 028 |
| 4. The five architectural conflicts | **4 resolved, 1 still live** (OTLP location) |

---

## What's left before refactor planning

### Refactor blockers (must resolve before code work)

1. **`noodle-domain` crate ADR.** Defines the type families, content categories, capability classifications, speech acts. Without it, every refactor decision that touches what `noodle-core` vs `noodle-domain` vs `noodle-adapters` owns is guessing. **Highest priority.**

2. **OTLP location.** Live disagreement: ADR 022 carries `OtlpSideEffectSink` framing; ADR 001 principle 7 says the proxy doesn't ship OTLP directly. Pick one. Affects whether the OTLP code lives in `noodle-adapters` (current shape) or moves entirely into the downstream embellishment plane (ADR 001's framing). **Cannot refactor adapters until this is resolved.**

3. **Watchtower / control-plane bundle.** Five planned capabilities (control port, header rewrite, body model rewrite, provider translation, cost feedback). They all attach at the same architectural surface. A refactor that doesn't leave hooks for them will be re-done in a year. One ADR — "Control-plane attachment surface" — could specify all five together.

### Useful before refactor but not blockers

4. **`AuditEvent` / `AuditKind` catalog.** ADR 027 §5 pins where audits land; the canonical enum + per-kind `detail` schema is not specified. The refactor can proceed with the existing enum and tighten later.

5. **`ResolvedRecord` / `Resolved` field shape.** ADR 027 §5 pins which sink it lands on; canonical field shape still spread across ADR 020 + ADR 004. Consolidate into one place.

6. **Side-effects.jsonl schema.** ADR 027 pins what goes there; the line schema itself isn't formally written. Can be done as a §-extension to ADR 027.

7. **Catalog reference document.** What codecs / transforms / detectors / cells exist today, enumerated. ADR 025 §3.5 mentions a `noodle catalog list` CLI; no doc yet.

### Cleanup (do anytime)

8. Status hygiene on ADRs 016, 017, 019, 020 (proposed → current).
9. `docs/architecture/architecture.md` refactor (pending per `docs/features/refactor-plan.md`).
10. Layered L0–L5 drawio diagram for ADR 001 §6.
11. Cell-dispatch drawio for ADR 019.
12. Async transform / detector story (ADR 015 §12 + ADR 024 §8 deferred).

---

## Recommended order

1. `noodle-domain` ADR.
2. OTLP location ADR (or amend ADR 022 + ADR 001 to agree).
3. Watchtower / control-plane attachment surface ADR.

After these three, the refactor has a coherent target. Items 4–7 can be filled in as the refactor progresses; items 8–12 are cleanup that doesn't block.
