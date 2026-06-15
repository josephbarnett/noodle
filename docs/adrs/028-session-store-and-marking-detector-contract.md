# ADR 028 — SessionStore and the per-cell marking-detector contract

**Status:** current. Specifies the cross-request state surface
(`SessionStore`) and the per-cell marking-detector behaviour that
produces `session_id`, `turn_id`, and `parent_session_id` for the
`tap.jsonl` marks block (ADR 027 §4.2).

**Resolves:** the conflict between ADR 001 §5.4 ("Mark"
responsibility — marks stamped at flow open), ADR 021 (stateless
`RequestDetector`), and the per-cell reality
(`docs/knowledge/session-hierarchy.md`): each cell exposes its
own combination of identifiers and boundary signals on the wire,
and the per-cell marking detector is the component that either
extracts the identifier where it exists or mints one from the
available boundary signals where it does not.

**Related:** ADR 015 §12 #4 (the typed `SessionStore` handle in
`TransformAttachment`), ADR 019 (per-cell capability dispatch), ADR
020 (`SessionStore` referenced in side-effect wiring), ADR 021
(revised by §6 below — `RequestDetector` gains a read handle on
`SessionStore`), ADR 027 (where the marks land),
`docs/knowledge/session-hierarchy.md` (the authoritative
wire-level reference).

---

## Goal

The goal of this ADR is to define how every round-trip record on
`tap.jsonl` is tagged with three stable identifiers — `session_id`,
`turn_id`, and `parent_session_id` — that let downstream consumers
group, audit, and reason about agent activity **without re-deriving
turn boundaries or sub-agent lineage from raw wire data**.

### Why

`tap.jsonl` is the canonical evidence boundary (ADR 027). Every
downstream consumer — viewers, evaluators, security tooling, the
embellishment plane — reads marks instead of replaying wire streams
and re-running boundary detection. For that contract to hold, the
marks must be:

1. **Stable across all round-trips of the same turn** — so a
   "turn" is a queryable unit, not a join across rows.
2. **Stable across a sub-agent's entire run, with a back-pointer
   to the parent turn** — so multi-agent traces can be reconstructed
   without re-walking lineage downstream.
3. **Produced once, by a per-cell marking detector**, whose job is
   to inspect the wire signals that **that cell** carries and
   either:
   - **extract** the identifier from the wire when the cell exposes
     one directly, or
   - **mint** the identifier from the cell's boundary signals when
     the wire does not carry one.

The third point is the architectural keystone: identifier
derivation is **per cell** because the wire facts are per cell. A
provider that ships a stable turn id on the wire is decoded by a
detector that extracts it; a provider that ships only boundary
signals is decoded by a detector that mints from them. The
`SessionStore` and the contract in §4–§5 are the same shape in
either case; only the per-cell detector's body differs.

Without this contract, every consumer reinvents boundary detection
from raw wire bytes (the viewer already does, at `ooda.ts:270+`),
divergence is inevitable, and `tap.jsonl` stops being a sufficient
evidence boundary. With this contract, `tap.jsonl` is the boundary,
and consumers join records by mark.

### What this ADR specifies

1. The **wire facts** that anchor the contract (§1) — what the
   wire carries, what it does not, and what the proxy must derive.
2. The **`SessionStore` surface** (§3) — the cross-request typed
   handle that holds per-session state.
3. The **marking-detector contract** (§4–§5) — the decision rule
   every per-cell detector implements, the inputs it receives, and
   the outputs it produces.
4. The **revised `RequestDetector` shape** (§6) — how ADR 021's
   stateless detector is amended to read `SessionStore` while
   remaining stateless w.r.t. flow history except through that
   typed handle.
5. Naming corrections (§7), tap.jsonl integration (§8), and the
   verification contract (§9).

### Non-goals

- **Identity resolution** (turning `device_id` into "this person on
  this team") — embellishment-plane concern; deferred to story 028.
- **The wire codec layer** (how SSE bytes become structured events)
  — owned by ADR 015 / ADR 018.
- **Side-effect transport** (where marks land downstream of
  `tap.jsonl`) — owned by ADR 027 §5.

---

## 1. Wire facts the contract operates on

Wire facts are **per cell**. Each cell exposes a different mix of
identifiers, boundary signals, and lineage pointers, and the
per-cell marking detector is written against its cell's mix. This
section enumerates the facts cell-by-cell so the §4–§5 contract can
state the marking-detector behaviour in a way that accommodates
both extraction (identifier present on the wire) and minting
(identifier absent — derive from boundary signals).

The two cells specified below are illustrative of the design
contract, not exhaustive: any future cell follows the same shape —
declare what its wire carries, then declare whether its detector
extracts or mints each of `session_id`, `turn_id`, and
`parent_session_id`.

### 1.1 `api.anthropic.com`

| Signal | Where | Scope / meaning |
|---|---|---|
| `X-Claude-Code-Session-Id` (header) | request | **per-session.** Spans every turn and every round-trip within the session. Sub-agents share the parent's session id (`docs/knowledge/session-hierarchy.md` §"Correction (2026-05-10)"). |
| **`delta.stop_reason: "end_turn"`** (SSE `message_delta.delta.stop_reason`) | response | **THE turn-end signal.** The value `"end_turn"` is emitted if and only if the model has completed the current turn. This is the unambiguous, single-source-of-truth wire signal that a turn has ended. The marking detector treats `end_turn` as definitive: the next request on the same session belongs to a new turn. |
| `delta.stop_reason: "max_tokens"` | response | **Exceptional turn-end.** Response truncated at the token cap. For turn-membership purposes the boundary effect is the same as `end_turn`: the turn closes here. Distinguished from `end_turn` only for audit / quality reasons. |
| `delta.stop_reason: "tool_use"` | response | **Mid-turn pause, not a boundary.** The model has emitted one or more `tool_use` blocks and is waiting for `tool_result`s to be supplied in the next request. The turn continues. The marking detector treats this round-trip as a continuation of the current turn — not as a turn boundary. |
| `message.id` (SSE `message_start.message.id`) | response | **Per-round-trip.** Distinct value every round-trip. Not echoed in subsequent `messages[]` history (verified across all five captures in `captures/`). |
| `X-Client-Request-Id` (header) | request | **Per-round-trip.** |

### 1.2 `claude.ai`

| Signal | Where | Scope |
|---|---|---|
| `conversation_uuid` (URL path segment in `/api/.../chat_conversations/{conversation_uuid}/completion`) | request | per-conversation. The session-equivalent. |
| `system` / `personalized_styles` (request body) | request | per-round-trip. |
| `human_message_uuid`, `assistant_message_uuid` (request body `turn_message_uuids` block) | request | **per-round-trip.** Verified empirically across three round-trips of one conversation: six distinct UUIDs, no reuse. UUIDv7 — timestamps, not lineage. |
| `delta.stop_reason` (SSE `message_delta.delta.stop_reason`) | response | per-round-trip. Same shape as `api.anthropic.com`. |

### 1.3 What the wire carries — signals vs identifiers

The wire does **not** carry opaque identifiers that span multiple
round-trips. It **does** carry the signals from which those
identifiers are derived:

| What the wire carries | What it lets the proxy derive |
|---|---|
| **`delta.stop_reason: "end_turn"`** in `message_delta` | **The turn boundary, named.** The wire announces turn-end in plain words. Detection is a single equality check on this value, not a heuristic. `max_tokens` is an exceptional close with the same boundary effect. `tool_use` is a mid-turn pause and does not close the turn. |
| `system` field replacement between requests on the same session (per-agent-run scope — §1.1) | **Agent-run transition.** The host program replaces the entire `system` payload when a new agent run begins (sub-agent invocation, title-generation run). Empirically stable across all turns and all round-trips within an agent run, so a replacement is a strong agent-run-boundary signal. |
| `Agent` tool_use blocks in assistant content (response stream) and matching `tool_result` blocks (next request body) | **Sub-agent lineage.** A parent's `Agent` tool_use_id is the spawn pointer; the matching `tool_result` is the close. The proxy threads parent ↔ sub-agent by walking this chain. This is the canonical lineage signal — `system` replacement alone is insufficient (title generation also replaces `system` but is not a sub-agent spawn). |
| `X-Claude-Code-Session-Id` (header) / `conversation_uuid` (URL) | **Session identity.** Cells extract this per-cell. |
| `<system-reminder>` text blocks inside user messages (§1.1) | **Per-turn context, not a boundary.** These are host-injected per-turn payloads. The marking detector does not treat their content or hash changes as turn or agent-run boundaries. |

What the proxy mints, given those signals:

- **`turn_id`** — proxy-generated ULID at every `end_turn` /
  `max_tokens` / cold-start boundary detected via `stop_reason`.
  Same id reused across all round-trips of the same turn (the
  identifier itself is not transmitted by the wire; it is computed
  from the boundary signal).
- **`parent_session_id`** — proxy-generated link from a sub-agent's
  first round-trip back to the parent's turn, derived from the
  Agent tool_use lineage above (the link itself is not transmitted
  by the wire; it is computed from the lineage signal).

The proxy is not inventing turn membership or sub-agent lineage out
of nothing. Both are present in the wire data; the proxy converts
them into stable identifiers downstream consumers can read directly
from `tap.jsonl` without re-deriving.

### 1.4 The decoder's existing `TurnId` field is misnamed

`crates/noodle-adapters/src/provider/anthropic.rs:115-118` defines
`AnthropicStreamingDecoder { turn_id: Option<TurnId> }`. The field
holds `payload.message.id` from `message_start` — a per-round-trip
identifier. ADR 008 / wire-fact terms this is a **round-trip id**,
not a turn id. §7 of this ADR specifies the rename.

---

## 2. Decision

Two separate concerns are kept distinct in the contract:

| Concern | How the contract addresses it |
|---|---|
| **Turn-boundary detection** ("is this round-trip the end of a turn?") | Read `delta.stop_reason` from the response stream. The signal is on the wire, in every response, on every cell that runs SSE responses (verified empirically — §1.1, §1.2). |
| **Turn identification** ("which records belong to the same turn?") | The proxy mints a ULID at the first round-trip of each turn. Subsequent round-trips of the same turn carry the same minted id. SessionStore caches the current id and the prior round-trip's `stop_reason`. |

The marks block on `tap.jsonl` (ADR 027 §4.2) carries:

- `session_id` — extracted from the probe per cell.
- `turn_id` — minted by the marking detector at flow open per the §4 decision rule.
- `parent_session_id` — derived per cell from cross-request state in
  SessionStore (§5).
- per-cell correlation fields — defined by each cell's spec (§5).

The decision is binding for every cell whose dispatch table entry
includes a `marking_detector` capability.

---

## 3. `SessionStore` — interface and shape

`SessionStore` is the cross-request state surface for marks. It is a
port in `noodle-core`; the shipped implementation is
`InMemorySessionStore` (`noodle-adapters`).

### 3.1 Interface

```rust
pub trait SessionStore: Send + Sync + 'static {
    /// Read the cached state for a session. None if no round-trip
    /// has been observed for this session.
    fn get(&self, session_id: &SessionId) -> Option<SessionState>;

    /// Write the cached state at end of response. Atomic per-session.
    fn put(&self, session_id: SessionId, state: SessionState);
}

#[derive(Clone, Debug)]
pub struct SessionState {
    /// The turn currently in progress, if any.
    pub current_turn_id: Option<TurnId>,
    /// The most recent round-trip's `stop_reason` for this session.
    /// None before the first response has been observed.
    pub last_stop_reason: Option<StopReason>,
    /// The system-prompt hash of the most recent round-trip.
    /// Used to detect sub-agent transitions within a session
    /// (`docs/knowledge/session-hierarchy.md` §"Correction").
    pub last_system_hash: Option<SystemHash>,
    /// When this session's first round-trip is a sub-agent of
    /// another session, the parent. None for top-level sessions.
    pub parent_session_id: Option<SessionId>,
    /// Timestamp of the most recent write. Used for eviction.
    pub last_observed_at_unix_ms: u64,
}
```

### 3.2 Lifetime

- **Per-process.** State is held in memory; not persisted across
  proxy restarts. A proxy restart starts every session from a
  cold cache. The first round-trip after restart for a given
  session is treated as a fresh turn (§4).
- **Per-process scope.** No cross-process sharing. The proxy
  process is the single writer.
- **Eviction.** Sessions are evicted after `last_observed_at_unix_ms`
  exceeds a configured TTL (default 6 hours — covers the longest
  observed Claude Code session in `captures/`). Eviction does not
  affect in-flight flows; eviction only applies to sessions with no
  open flows.

### 3.3 What `SessionStore` is not

- Not a store of message bodies. Those land on `tap.jsonl`.
- Not a store of `Hint` / `Artifact` / `AuditEvent`. Those land on
  `SideEffectSink` (ADR 020, ADR 027 §5).
- Not durable. A restart loses the cache; the §4 rule handles
  cold-cache cleanly.

---

## 4. The marking-detector contract

A marking detector is a per-cell `RequestDetector` (ADR 021)
extended to read `SessionStore` at flow open and write back at
flow close. The contract has three steps.

### 4.1 Step 1 — request open

The decision is keyed by three inputs:

| Input | Source |
|---|---|
| **Cell identity** | `(domain, endpoint, direction)` dispatch — only cells whose dispatch entry includes a `marking_detector` capability run this contract. |
| **`session_id`** | Extracted per-cell from the probe. The cell's spec documents which probe field carries it (header on `api.anthropic.com`; URL path segment on `claude.ai`). |
| **`SessionStore[session_id]`** | The cached state. Three meaningful states: absent (first round-trip for this session), present with `last_stop_reason == tool_use` (mid-turn), present with `last_stop_reason ∈ {end_turn, max_tokens, None}` (prior turn closed). |

Decision table:

| SessionStore entry | `last_stop_reason` | Outcome |
|---|---|---|
| absent | — | First round-trip for this session — mint a fresh `turn_id` (ULID), write the entry. |
| present | `tool_use` | Continuation — reuse the cached `current_turn_id`. |
| present | `end_turn` / `max_tokens` / `None` | Prior turn closed — mint a fresh `turn_id`, overwrite the entry. |

The chosen `turn_id` is stamped on the **request** `tap.jsonl`
record. If `session_id` could not be extracted from the probe, the
marking detector emits an `AuditEvent { kind: Errored, … }` on
the side-effect bus and skips marks for this flow.

### 4.2 Step 2 — response stream

The marking detector reads `delta.stop_reason` from the response's
`message_delta` event as the SSE stream flies by (the same SSE event
the marker-strip transform reads to know the response is wrapping
up). The `turn_id` decided at step 1 is stamped on the **response**
`tap.jsonl` record (paired by `request_id`).

### 4.3 Step 3 — response close

At flow close, the marking detector writes to SessionStore:

```
SessionStore.put(session_id, SessionState {
    current_turn_id:        <turn_id from step 1>,
    last_stop_reason:       <stop_reason observed in step 2>,
    last_system_hash:       <hash of system from this request>,
    parent_session_id:      <preserved or set per §5>,
    last_observed_at_unix_ms: now(),
});
```

The next round-trip for this `session_id` reads this state at its
own step 1.

### 4.4 Universal vs per-cell fields

A marking detector produces two categories of mark:

| Category | Who produces | Field names |
|---|---|---|
| **Universal marks** (every marking-enabled cell) | The general contract above | `session_id`, `turn_id`, `parent_session_id` |
| **Per-cell correlation fields** (only the cell whose wire carries them) | The cell's marking-detector spec | named per cell — see §5 |

Downstream consumers read by field name and ignore fields they do
not recognise (ADR 027 §6 — versioning posture).

---

## 5. Per-cell marking-detector specs

Each marking-enabled cell has its own spec documenting which probe
fields carry the universal marks and which per-cell correlation
fields it emits. The specs that follow are the v1 catalog.

### 5.1 `(api.anthropic.com, /v1/messages, request→upstream)`

| Mark | Source |
|---|---|
| `session_id` | `X-Claude-Code-Session-Id` request header. Required; missing header → emit `AuditEvent { kind: Errored }`, skip marks. |
| `turn_id` | Per §4.1 decision rule. |
| `parent_session_id` | Computed at flow open by comparing the request's `system` hash to `SessionStore[session_id].last_system_hash`. If the hashes differ and `last_system_hash` is not None, the current round-trip is a sub-agent run; emit `SessionStore[session_id].last_system_hash`'s session id as `parent_session_id`. (Refinement: §9 #2.) |
| `x_client_request_id` | `X-Client-Request-Id` request header, per-round-trip correlation. |

### 5.2 `(claude.ai, /api/.../chat_conversations/{id}/completion, request→upstream)`

| Mark | Source |
|---|---|
| `session_id` | `conversation_uuid` URL path segment. Required. |
| `turn_id` | Per §4.1 decision rule. |
| `parent_session_id` | Same mechanism as §5.1, keyed by `personalized_styles` hash instead of `system` hash. (Refinement: §9 #2.) |
| `human_message_uuid`, `assistant_message_uuid` | Request body `turn_message_uuids` block. Per-round-trip; downstream consumers reading `tap.jsonl` for per-message correlation use these. |

### 5.3 Cells without a marking detector

Cells whose dispatch entry does not include a `marking_detector`
capability (auxiliary endpoints, telemetry pings, MCP, DNS-rewrite
cells, etc.) do not emit marks. Their `tap.jsonl` records carry the
identification block (§4.1 of ADR 027) but the marks block is
absent.

---

## 5.5 Applicability to the plugin topology

The `MarkingStore` port (§3) and the `MarkingDetector` trait (§4)
are host-independent. The in-memory implementation
`InMemoryMarkingStore` (in `noodle-adapters::marking`) is
plugin-embeddable; the proxy host uses it directly and the
`noodle-detect` facade (ADR 039 §2.3) carries it in
`DetectContext::marking_store` by default.

For plugin deployments where the host gateway needs to persist
sessions across plugin-instance restarts, the host supplies its
own `MarkingStore` impl across the WASM boundary via host-imported
functions (ADR 039 §3). The shim in `noodle-detect` wraps those
imports in the `MarkingStore` trait the facade consumes; the
plugin author's code is identical to in-process Rust code.

Per-cell `MarkingDetector` impls (e.g.
`AnthropicMarkingDetector`) are pure logic and compile to WASM
unchanged. Plugin authors writing new detectors for additional
providers follow the same contract §4 specifies.

## 6. Revision to ADR 021

ADR 021 (`RequestDetector` two-tier) describes `RequestDetector` as
stateless. This ADR revises that contract:

> A `RequestDetector` is stateless with respect to flow history
> **except** through the typed `SessionStore` handle in
> `TransformAttachment` (ADR 015 §12 #4). The handle is read-only
> at flow open and read-write at flow close. State outside
> `SessionStore` is forbidden.

The revision is narrow. `SessionStore` is the single permitted
cross-request state surface for detectors and transforms.

---

## 7. Integration with `CacheAndRelease` and `Extractor`

ADR 016's buffering primitives are intra-flow; this ADR's
`SessionStore` and marking detector are cross-flow. They compose
through three integration points; ADR 016 itself does not change.

### 7.1 Marks on outputs

When `CacheAndRelease` emits an `Audit` (overflow, deadline, policy
violation) or `Extractor` emits a `Captured` artifact, the engine
attaches the current flow's marks (`session_id`, `turn_id`,
`parent_session_id`) at the side-effect / extraction boundary. The
primitive does not read marks; the engine wrapper does. This is
the mechanism by which `tap.jsonl` extractions inherit the marks of
the round-trip they came from (ADR 027 §5).

### 7.2 Turn-aware `Extractor` policy

An `Extractor` whose decision logic depends on turn context —
different thresholds at turn-start vs mid-turn, skipping extraction
during `tool_use` continuations, classifier-call rate-limits keyed
to the turn — reads `turn_id` (and the boundary signals behind it)
through the same `SessionStore` read handle defined in ADR 015 §12
#4. The Extractor trait surface in ADR 016 §4 does not change; the
handle is supplied by the engine when the Extractor is attached.

### 7.3 Cross-turn aggregation

Accumulators whose scope is the **turn**, not the flow — per-turn
token totals, per-turn classifier-call budgets, per-turn dedup sets
of captured values — live in `SessionStore` keyed by
`(session_id, turn_id)`, not inside `CacheAndRelease`.
`CacheAndRelease` is intra-flow by construction; any state that
needs to survive the flow boundary is, by definition, a
`SessionStore` concern.

### 7.4 What does not change

- `CacheAndRelease<E>` and `Extractor<E>` trait shapes (ADR 016 §3,
  §4) — unchanged.
- `BoundedCacheAndRelease` default impl — unchanged.
- The release-decision contract (bytes ceiling, wall-clock deadline,
  overflow audit) — unchanged. None of these are turn-aware; they
  remain pure intra-flow concerns.

---

## 8. Naming corrections in `noodle-core`

The existing `noodle-core::event::TurnId` holds `payload.message.id`
from `message_start` — a per-round-trip identifier (§1.4). Two
corrections:

| Old name | New name | What it carries |
|---|---|---|
| `noodle_core::event::TurnId` | `noodle_core::event::RoundTripId` | The per-round-trip identifier the decoder extracts from `message_start.message.id`. Distinct value every round-trip. |
| (new type) | `noodle_core::event::TurnId` | The proxy-minted user-intent turn id. ULID. Carried in the marks block on `tap.jsonl`. |

`NormalizedEvent::TurnStart { turn_id, role }` and
`NormalizedEvent::TurnEnd { turn_id, finish }` rename their
`turn_id` field to `round_trip_id` of type `RoundTripId`.

The migration is mechanical (compiler-checked rename). Tests
require the same renames; no behaviour changes.

---

## 9. Alternatives rejected

Two alternatives to the §4 contract were considered.

| Alternative | Rejected because |
|---|---|
| **Stamp `session_id` at request open; backfill `turn_id` and `parent_session_id` on the response record at end of flow.** Asymmetric records — the request line carries `session_id` but not `turn_id`; the response line carries both. | Every downstream consumer would have to handle the asymmetry and re-join request and response records to attribute the request to a turn. The viewer's `foldIntoTurns` would be reproduced in every consumer. |
| **Drop "stamped at probe time" from ADR 001 §5.4 entirely. Write the `tap.jsonl` record at end of flow with all marks resolved.** | Defers the record write until response close, which blocks streaming consumers from reading the request side before the response completes. The streaming-tail-friendly posture of `tap.jsonl` (a consumer can see the request line immediately) is lost. |

The §4 contract — read SessionStore at flow open, write back at
flow close — preserves both record symmetry and streaming-tail
friendliness at the cost of revising ADR 021's "stateless"
qualifier (§6).

---

## 10. Open questions deferred

1. **Cold-cache behaviour for resumed sessions.** When noodle
   starts mid-conversation (the proxy was restarted while a Claude
   Code session was in flight), the first observed round-trip
   appears with a session id whose prior state is gone. The §4
   decision rule treats it as a fresh turn. A consumer correlating
   long sessions will see an apparent turn boundary at the restart
   point. Acceptable for v1; revisit if a real consumer requires
   smoother continuity.
2. **Refinement of `parent_session_id` derivation.** §5.1 / §5.2
   currently emit `parent_session_id` whenever the system-prompt
   hash changes within a session. This produces a parent link for
   the *first* round-trip of every sub-agent run but does not link
   subsequent round-trips of that sub-agent run back to the
   original parent. SessionStore would need to track an active
   sub-agent stack to do that — pinned for the implementing PR.
3. **Friendlier name for `parent_session_id` (cosmetic).** The
   concept is correct: a sub-agent has its own noodle `session_id`,
   distinct from the parent's, and `parent_session_id` is the
   parent's noodle session id. The only open question is whether a
   friendlier surface name would read better on `tap.jsonl` for
   downstream consumers. Deferred until a real consumer surfaces a
   preference.
4. **Eviction TTL value.** The 6-hour default in §3.2 is anchored
   to the longest Claude Code session observed in `captures/`.
   Operators may tune it. Whether the TTL belongs in the dispatch
   table or in a separate `[noodle.session_store]` block is
   deferred.
