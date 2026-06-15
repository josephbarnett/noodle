# noodle — design gap analysis

What an engineer cannot find or reconstruct from the current design corpus.

Scope: `docs/adrs/001-component-architecture.md`, `docs/adrs/*`, `docs/decisions/*`,
`docs/knowledge/*`, and the diagram set in `docs/diagrams/`. Bar:
complete professional design coverage, no narrative substitutes.

---

## 1. First-class concerns named in the architecture but with no design

`001-component-architecture.md` enumerates these as architectural attachment
points or first-class responsibilities. No design document specifies any
of them.

| Concern | Where named | What's missing |
|---|---|---|
| **The dispatch table** | ADR 019; `001-component-architecture.md` §1.3 | File format, schema, validation rules, worked example. ADR 019 says "config, CISO-owned" and stops. |
| **Watchtower control port** | `001-component-architecture.md` §6.6 | The interface noodle exposes for an external controller to signal per-session decisions. No port definition, no message format, no flow diagram. |
| **Header rewriting** | `001-component-architecture.md` §6.6 | A `Transform<HeaderMap>` slot is asserted at L3. No transform interface for it, no preservation semantics (which headers may be modified, which must round-trip verbatim). |
| **Body-level model rewrite** | `001-component-architecture.md` §6.6 | The `Transform<NormalizedRequest>` slot for per-session model switching. No transform, no policy interface. |
| **Provider translation** | `001-component-architecture.md` §6.6 | Asymmetric decode/encode codec pairing across vendors (e.g. claude.ai → OpenAI). No spec for the cross-vendor mapping. |
| **Sensitive-content protection** | `001-component-architecture.md` §6.7 | Outbound + inbound secret / IP / PII detection. Attachment points named; no patterns, no detector catalog, no redaction policy spec. |
| **Cost feedback** | `001-component-architecture.md` §6.6 | "Operational audit events on the side channel." No canonical cost-record shape (tokens × model × rate). |
| **Per-cell marking detector contract** | `001-component-architecture.md` §1.3, §4.4 | ADR 021 defines `RequestDetector` as an interface. What each cell's marking detector emits (Anthropic's session header, claude.ai's conversation id, etc.) is unspecified. |

## 2. Crates / types asserted as canonical but with no specification

| Item | Where asserted | What's missing |
|---|---|---|
| **`noodle-domain` crate** | `001-component-architecture.md` §2.2 | No ADR ratifies the crate. The Agent-Protocol type families are listed but no type definitions, no cross-vendor survey doc, no schema. The dependency graph in `AGENTS.md` lists six crates, not the seven the architecture spec describes. |
| **`SessionStore`** | `001-component-architecture.md` §2.1; ADR 015 §12 #4; ADR 020 §2.3 | The interface is named in three places and defined in none. What it stores, the cross-request lifetime, the per-session state shape — all unspecified. |
| **`ResolvedRecord` / `Resolved`** | ADR 020 §2.2 (Rust definition); ADR 004 (concept) | The canonical shape of the attribution record — fields, types, downstream contract — is not pinned beyond a `category → value` map. |
| **Cross-request correlation scope** | ADR 019 §2.5 | The keyed, per-conversation store named as the contract for `inject→client` ↔ `harvest←client` correlation. No spec. |
| **`AuditEvent` / `AuditKind`** | ADR 015 §13; ADR 020 §1 | Kinds referenced in passing (`Errored`, `InvariantViolation`, `Injected`, `Redacted`, `Filtered`, `Overflow`, `Resolved`, `CacheAndReleaseOverflow`). No canonical enumeration, no per-kind `detail` schema. |

## 3. Wire-format references missing for first-class observation targets

`docs/knowledge/` covers Anthropic SSE, OpenAI Responses, QUIC primer,
session hierarchy. The corpus omits:

| Target | Why first-class | Where mentioned |
|---|---|---|
| **claude.ai chat-completion** | The single most consequential traffic for noodle (Claude Desktop chat). Wire shape (request envelope with `prompt` / `personalized_styles`, response SSE with `chatcompl_` envelope, `parent_uuid`, citations) appears only inside ADR 018's evidence section. | ADR 018 §1 |
| **Google Gemini `streamGenerateContent`** | ADR 015 §2 lists `GoogleCodec` as a planned L5 codec. No protocol reference. | ADR 015 §2 |
| **OpenAI Chat Completions (legacy)** | The format Codex CLI uses. ADR 010 covers the newer Responses API and only names the legacy format as a "different code path." | ADR 010 §"Persistent-connection variant" |
| **MCP over SSE / JSON-RPC** | ADR 018 §1 documents `/mcp/v2/bootstrap`, `/v1/toolbox/shttp/mcp/…`, `/v1/code/…` as endpoints the proxy must classify but not interpret. No reference. | ADR 018 §1 |
| **DNS HTTPS / SVCB records** | The data shape the DNS proxy strips (`alpn=h3`, `ech=`). Covered only inline in ADR 014. No standalone reference. | ADR 014 §9 |

## 4. Schemas, catalogs, and examples missing

| Missing | Why needed |
|---|---|
| **`tap.jsonl` schema + worked example** | `001-component-architecture.md` §2.4 lists fields. No JSON Schema, no example line, no versioning story for downstream consumers to depend on. |
| **`SideEffect` / `Resolved` JSON wire shape** | ADR 020 defines Rust variants. Downstream consumers reading the file boundary need the JSON shape. Not specified. |
| **Shipped codec / transform / detector catalog** | An engineer cannot answer "which codecs ship in the current build, against which `(domain, endpoint, direction)` cells, with which transforms?" without reading the source. No reference doc. |
| **macOS sysext bundle / lifecycle spec** | ADR 014 §10 "Lessons" carries operational reality (runs as root, hostname not on flow meta, launchctl setenv). No spec for bundle layout, install flow state machine, uninstall flow state machine. |
| **CA file layout** | ADR 011 §4a documents the disk layout. Adequate. |

## 5. Architectural conflicts carried over from the prior audit — still unresolved

These are not gaps in coverage; they are gaps in consistency. An engineer
reading the corpus top-to-bottom will internalize two contradictory models.

| Conflict | Documents that disagree | Status |
|---|---|---|
| Single `tap.jsonl` boundary vs multiple JSONL files | `001-component-architecture.md` §2.4 vs ADR 011 §1, ADR 020 §2.1 | Open |
| Marks + extractions on the tap line vs on a separate `SideEffectSink` plane | `001-component-architecture.md` §2.4 vs ADR 020 §2 | Open |
| OTLP downstream of file boundary vs OTLP shipped from `noodle-adapters` | `001-component-architecture.md` §5 vs ADR 022 §2 | Open |
| `turn_id` / `parent_session_id` stamped by proxy at probe time vs derived from cross-request state | `001-component-architecture.md` §2.4 vs ADR 008 §"Correction (2026-05-10)" + ADR 021 | Open |
| `noodle-tap` is one file-based `WireSink` vs `noodle-tap` is an umbrella for multiple sinks | `001-component-architecture.md` §2.4 vs current code structure + ADR 020 §2.1 | Open |

## 6. Diagram gaps

`docs/diagrams/` carries: system context, component relationships,
hexagonal, layered-codec (companion to ADR 015), cache-and-release
primitives (companion to ADR 016), data-and-embellishment-planes
(companion to ADR 022), OSI mapping, component object model. Plus
`flows.md` and `type-model.md` markdown.

| Missing | Why |
|---|---|
| **Request flow diagram (layered data flow)** | The "data through layers" view we agreed view 4 should be. Shows bytes entering at L0, climbing through L1 TLS → L2 wire → L3 HTTP → L4 body framing → L5 vendor semantics → transforms, then descending back to bytes on the upstream side. |
| **Response flow diagram (layered data flow)** | The mirror of the request flow. |
| **Tap-write flow diagram** | How a record gets composed and written to `tap.jsonl`: identification fields + marks + bodies + extractions joining into one record per direction. |
| **Cell-dispatch flow diagram** | How `(domain, endpoint, direction)` is computed and how the catalog → chain mapping resolves. ADR 019 has no diagram. |
| **TLS-MITM cert-minting flow diagram** | ADR 011 §3 has the 30-second mental model and §5 has the per-host leaf sequence; could be lifted out for the diagram set. |

`flows.md` and `type-model.md` (the markdown-mermaid files in
`docs/diagrams/`) have not been audited against the canonical
architecture. Either may be current, partially stale, or fully superseded
by `001-component-architecture.md`.

## 7. Process and worked-example gaps

The corpus tells an engineer *what* the architecture is. It does not show
them *how* to extend it.

| Missing | Where partially named |
|---|---|
| **"Add a new provider" end-to-end worked example** | ADR 002 §5 lists steps; no worked example with file paths, test fixtures, registration code. |
| **"Add a new dispatch cell" worked example** | ADR 019 names the contract; no example. |
| **"Write a new transform with buffering" worked example** | ADR 016 specifies the primitives; no end-to-end example combining a `Transform`, a `CacheAndRelease` instance, and an `Extractor`. |
| **Async transform / detector story** | ADR 015 §12 names it as a future variant; no design, no story file, no interface sketch. |

## 8. Status hygiene

Status drift remains in the corpus. An engineer trusting the status
header will be wrong for several files.

- ADR 016 — "Proposed." Not implemented (no `CacheAndRelease` primitive in code per the backlog).
- ADR 017 — "Decided. Implementing (backlog item 2, first slice)." Item 2 first slice shipped.
- ADR 018 — refactored to "current."
- ADR 019 — "Drafted before code, pending Joe review." Per the backlog, the model has been built against.
- ADR 020 — "Decided. Implementing." Per the backlog, story 031 shipped.

`docs/architecture/architecture.md` carries a "refactor pending" stamp. The
refactor itself has not been performed.

`docs/diagrams/flows.md` and `docs/diagrams/type-model.md` have not been
audited.

`docs/guides/quic.log` is a stray log file in the operations directory.

---

## Summary — the four blocks an engineer cannot reconstruct

Out of all the gaps above, four blocks of design content are the
load-bearing absences:

1. **The `noodle-domain` crate** — content-semantic type system, asserted as canonical, with no ADR and no type catalog.
2. **The dispatch table format** — the CISO-owned routing policy file, asserted as the security boundary, with no schema.
3. **`SessionStore` and the per-cell marking detector contract** — cross-request state shape, asserted as the mechanism for `turn_id` / `parent_session_id` / per-cell correlation, with no specification (and a live architectural conflict about whether it is even possible to stamp those fields at probe time).
4. **The architectural-conflict block (audit findings #1–#4 / #6)** — five places where `001-component-architecture.md` and the ADR set teach incompatible models of the system. Until these are resolved at the spec level, an engineer cannot identify which is canonical.

Beyond those four, the rest of the gaps are reference material (wire
formats, schemas, catalogs), worked examples (add-a-provider,
add-a-transform), and diagram completeness (the layered flow views).
