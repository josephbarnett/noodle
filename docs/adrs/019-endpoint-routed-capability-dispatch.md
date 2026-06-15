# 019 — Endpoint-routed capability dispatch (the general frame)

**Status:** Drafted before code, pending Joe review. The general
frame; ADR 018 is its first concrete instance.
**Author:** Joe Barnett · Claude
**Date:** 2026-05-16
**References:** 015 (layered codec stack — the per-layer
mechanism this routes), 017 §7 + PR #35 (`RequestFlow` —
bidirectional seam proven), 018 (normalized request model —
becomes one instance under this frame), backlog item 21
(`032 DeclarativeCodec<Spec>`, PARKED — this is its safe,
near-term subset), `captures/` (the evidence), the product
mission memory (cost attribution + CISO-owned security/DLP).

---

## 1. Context — what the captures and the mission forced

Four convergent findings, each evidenced earlier in this design
thread, make a single abstraction unavoidable:

1. **One host carries many unrelated protocols.** In the chat
   capture, `claude.ai` alone served ~45 distinct endpoint
   families (HTML, telemetry, i18n, MCP-over-SSE, JSON-RPC, plain
   JSON, the chat completion); `api.anthropic.com` served both
   the documented Messages API *and* `/api/claude_code/settings`
   and app-update paths. Host-only selection is empirically
   wrong.
2. **The actions must be configurable, not hardcoded** (Joe):
   the approach must evolve — add an endpoint, re-sequence a
   chain, flip one to passthrough — without recompiling.
3. **Behavior is a function of `(domain address, endpoint,
   direction)`** (Joe): nothing we have discussed (attribution
   injection, marker-strip, tool_use capture, synthetic-tool
   injection, MCP decode, exfil observation, passthrough) is a
   *feature*. Each is one configured cell in that 3-axis space.
4. **The mission is directional and dual** (cost + CISO
   security/DLP): exfil monitoring is "data leaving this host
   toward a third party"; recon is "elicit from the client."
   These are distinct actor+flow vectors, not request/response.

## 2. Decision

**2.1 The dispatch key is the tuple
`(domain address, endpoint, direction)`.** A flow is classified
to exactly one cell. The cell resolves to an ordered chain of
*capabilities*. This is the entire system: a programmable
dispatch matrix over network flows.

- **domain address** — host (pattern), e.g. `api.anthropic.com`,
  `claude.ai`, `*.anthropic.com` only where genuinely uniform.
- **endpoint** — method + path pattern + content negotiation
  (`accept` / `content-type`), e.g.
  `POST …/chat_conversations/{id}/completion` + SSE.
- **direction** — see 2.2.

**2.2 `direction` is the 4-way actor+flow axis**, not binary
request/response:

| direction | meaning |
|---|---|
| `request→upstream` | client bytes heading to the third party |
| `response→client` | upstream bytes heading back to the client |
| `inject→client` | engine-originated content sent toward the client (no upstream request caused it) |
| `harvest←client` | engine consuming a client-produced result it solicited |

Exfil monitoring is `request→upstream` with an observe
capability. Attribution injection is `request→upstream` with an
inject capability. Marker-strip is `response→client` with a
filter capability. The recon channel is `inject→client` paired
with `harvest←client`. One contract; no special cases.

**2.3 Capabilities are compiled; the cell→chain mapping is
config.** The **capability catalog** (codecs + transforms:
`SseFrameCodec`, `JsonChunkCodec`, per-domain L5 codecs,
`AttributionInjector`, `MarkerStripTransform`, observe/audit
capabilities, `passthrough`, …) is vetted Rust. The
**routing table** — `{host, path, method, accept} → cell →
ordered [capability ids]` — is data, owned and edited by the
deploying CISO. Config *selects and sequences vetted
capabilities*; it never carries executable code. This is the
hard security boundary and the realization of "evolve without
hardcoding actions." It is the safe subset of the parked
`DeclarativeCodec<Spec>` (item 21) — selection/composition, not
arbitrary specs.

**2.4 Default is transparent passthrough.** Every unpopulated
cell passes bytes untouched (the existing
"transparent-unless-understood" guarantee). The product is the
populated cells; everything else is invisible plumbing.

**2.5 Cross-direction correlation is a first-class capability
contract.** A cell is dispatched independently, but some
capabilities are *cross-cell stateful*: `harvest←client` only
has meaning paired with a prior `inject→client` (correlated by
`tool_use_id` / conversation id). A capability therefore declares
an optional **correlation scope** (a keyed, per-conversation
store the engine owns and hands to both ends of the pair). The
per-cell dispatch stays clean; correlated capabilities opt into
shared state explicitly rather than the dispatcher special-casing
them. This is the architectural tension the 4-way axis
introduces, resolved here rather than deferred.

## 3. Alternatives rejected

- **Host-only / host+path-only selection.** Misses the
  directional axis the mission needs; cannot express
  inject/harvest. Empirically insufficient (§1).
- **Binary request/response direction + a side mechanism for
  inject/harvest.** Fractures the model into "the table plus
  special cases" — the leaky spine. Rejected; 4-way keeps one
  contract.
- **Code-in-config (full `DeclarativeCodec<Spec>` now).** Opens
  a code-execution surface incompatible with a CISO-owned
  security product; contradicts the v1 compiled-plugins
  principle. Deferred to item 21; this ADR ships the safe subset.
- **A feature per scenario in engine code.** The thing every
  prior turn of this design proved wrong: scenarios are cells,
  not features.

## 4. Consequences

- The engine becomes a **dispatcher over the key space** + the
  capability catalog. `InspectionEngine` already selects codecs
  per probe and runs `RequestFlow`/`ResponseFlow`; this
  formalizes selection as the 3-axis cell lookup and adds the
  `inject→client`/`harvest←client` directions + the correlation
  scope.
- **ADR 018 is the first instance**, not a parallel design: its
  per-domain request codecs are catalog entries; its
  `AttributionInjector` is one capability bound to the
  `(api.anthropic.com|claude.ai, completion, request→upstream)`
  cells. No rework — 018 is re-read as "instance of 019."
- **The CISO owns the routing table.** Audit of every populated
  cell is intrinsic (the table *is* the policy). The
  authorization/scope model (mission memory) is expressed *as*
  the routing table + a per-capability authorization predicate —
  a designed control, not a caveat.
- Backlog: item 21 reframed as "full `DeclarativeCodec<Spec>` —
  superset of ADR 019, still parked." Flagged for Joe, not
  silently folded.

## 5. Scope boundary

ADR 019 defines **only the dispatch contract**: the key space,
the 4-way direction axis, catalog-vs-config split, default
passthrough, and the correlation-scope mechanism. It introduces
**no capability** and **no code**. Concrete capabilities and
their cells are instances: ADR 018 (attribution), item 2 Filter
(marker-strip, shipped), future exfil/recon capabilities. Those
are not designed here; they plug into this contract.

## 6. Security considerations

- **Config is not code.** The routing table selects and
  sequences vetted catalog capabilities only. No interpreter, no
  arbitrary spec execution (that is item 21, parked). This bound
  is what makes a CISO-owned product defensible.
- **Authorization is a designed control.** Each capability
  carries an authorization predicate; the routing table is the
  CISO-owned policy surface; every populated cell is auditable by
  construction. Reconnaissance capabilities
  (`inject→client`/`harvest←client`) are legitimate **only**
  within this consented, company-owned, audited scope (mission
  memory) — the contract makes scope explicit and enforceable,
  not implicit.
- **Correlation state is bounded.** The per-conversation
  correlation scope (2.5) is engine-owned, keyed, and lifetime-
  bound to the flow/conversation — not an ambient global. No
  capability reads another's correlation scope.
- **Default-deny posture.** Unpopulated cells are passthrough
  (observe nothing, change nothing). The product only ever does
  what a populated cell says — the audit surface equals the
  table.
