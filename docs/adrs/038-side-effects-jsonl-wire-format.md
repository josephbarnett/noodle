# ADR 038 — The `side_effects.jsonl` boundary format

**Status:** current. Defines the wire format of the side-channel JSONL
file the proxy writes alongside `tap.jsonl` and `roundtrips.jsonl`.

**Related:** ADR 020 (`SideEffectSink` port + the per-flow drain that
sources every record this file carries), ADR 023 (correlation IDs and
the `roundtrips.jsonl` aggregation that embeds these records as
`evidence`), ADR 027 (the sibling `tap.jsonl` format).

**System diagram:** [`../diagrams/system-architecture.drawio`](../diagrams/system-architecture.drawio) — the **Side Effects** datastore between the proxy and the Endpoint Product / Security Scanner consumers.
![system architecture](../images/system-architecture.png)

---

## 1. Context

`side_effects.jsonl` is the durable mirror of the in-process
`SideEffect` bus. Every emission from every transform — `Hint`,
`Artifact`, `AuditEvent`, plus the engine's per-flow `Resolved`
output — lands on this file as one line at the moment the engine
drains the flow. Consumers tail the file for per-emission detail at
finer granularity than `roundtrips.jsonl` provides.

This ADR pins the on-disk shape because:

1. **Downstream consumers depend on stable keys.** The Endpoint
   Product's live monitors, the viewer's side-effects panel, and
   future Watchtower policy logic all read this file (or subscribe
   to the in-process bus that mirrors it). The wire format is the
   API contract.
2. **The four variants are discriminated on disk by a `kind` tag.**
   The shape per variant must be unambiguous so consumers can pattern-
   match without reading rust code.
3. **The ADR 023 §2.3 correlation block lands flattened at the top
   level.** Consumers join `side_effects.jsonl` ↔ `tap.jsonl` ↔
   `roundtrips.jsonl` by `event_id`; flattening (rather than nesting
   under `correlation`) keeps `jq` queries terse.
4. **One special case exists:** cert-mint audits emitted by the
   `ExternalCertMintService` carry `flow_id = 0` and **no
   correlation block** — they happen outside any inspection flow.
   This contract has to be spelled out so consumers don't filter
   them out by accident or fail-parse on missing keys.

## 2. Record framing

Identical to ADR 027 §2:

- One JSON object per line (NDJSON / JSONL), terminated by `\n`.
- UTF-8.
- Append-only; observation order.
- No header line.
- Default path: `$HOME/.noodle/side_effects.jsonl`.
- Rotation policy follows the same shipped writer mechanics as
  `tap.jsonl`.

## 3. Variants

Every record carries `"kind"`, a discriminator that takes one of four
values: `"hint"`, `"artifact"`, `"audit"`, `"resolved"`. The
discriminator selects the per-variant field set in §4.

```json
{"kind": "hint",     ...}
{"kind": "artifact", ...}
{"kind": "audit",    ...}
{"kind": "resolved", ...}
```

The four variants map 1:1 to the four `SideEffect` enum variants in
`noodle-core::layered` (ADR 020 §2.1). Adding a fifth variant is a
schema-evolution event — see §6.

## 4. Per-variant fields

### 4.1 `kind: "hint"`

A confidence-ranked opinion about an attribution category from a
detector or transform. Inputs to the per-flow Resolver.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | `"hint"` | yes | Discriminator. |
| `category` | string | yes | Attribution category (`"tool"`, `"work_type"`, etc.) per ADR 004. |
| `value` | string | yes | The category's candidate value (e.g. `"Claude Code"`). |
| `confidence` | number `[0.0, 1.0]` | yes | The emitter's confidence. |
| `source` | string | yes | Identifier of the transform / detector that produced the hint (e.g. `"user_agent"`, `"marker_strip"`). |
| `at_unix_ms` | integer | conditional | Engine drain-stamp wall-clock in milliseconds. Present whenever the correlation block is present. Omitted in the rare case the record bypasses the drain seam (see §5). |
| **correlation fields** (`event_id`, `turn_id`, `session_id`, `agent_run_id`) | see §5 | conditional | Present unless emitted outside any flow. |

`Hint` is the only variant whose native rust shape lacks a timestamp;
the JSONL exposes `at_unix_ms` as a top-level field for it, sourced
from the correlation block.

### 4.2 `kind: "artifact"`

A captured named value with chain-of-custody — the literal content
of a `<noodle:NAME>VALUE</noodle:NAME>` marker, extracted by
`MarkerStripTransform` (ADR 017 §2.3 / ADR 020 §1.1).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | `"artifact"` | yes | Discriminator. |
| `name` | string | yes | The marker name (e.g. `"work_type"`). |
| `value` | string | yes | The captured value (e.g. `"refactor"`). |
| `source_transform` | string | yes | The transform that captured the value (typically `"marker_strip"`). |
| `flow_id` | integer | yes | Engine-assigned per-flow identifier. |
| `captured_at_unix_ms` | integer | yes | Drain-stamp wall-clock in milliseconds. Sourced from the correlation block when the legacy slot is zero. |
| **correlation fields** | see §5 | conditional | Present unless emitted outside any flow. |

### 4.3 `kind: "audit"`

An operational event — request injection happened, response content
was redacted, a frame was filtered, a codec hit an error, a leaf cert
was minted. The audit channel surfaces "noodle did something" so
operators can verify the proxy's mutations (ADR 020 §1).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | `"audit"` | yes | Discriminator. |
| `kind_inner` | string | yes | One of `"injected"`, `"redacted"`, `"filtered"`, `"errored"`, `"invariant_violation"`, `"leaf_minted"`, `"mint_failed"`. The audit-channel sub-kind per ADR 020 §1.1. |
| `transform` | string | yes | The transform that emitted the audit (`"attribution_inject"`, `"marker_strip"`, the cert-mint backend name, etc.). |
| `flow_id` | integer | yes | Engine-assigned per-flow identifier. **`0` for cert-mint audits** (`leaf_minted` / `mint_failed`) — see §5. |
| `at_unix_ms` | integer | yes | Drain-stamp wall-clock in milliseconds. Cert-mint audits stamp their own wall-clock at emission (the only path that bypasses the drain seam). |
| `detail` | object | yes | Free-form structured payload per `kind_inner`. The schema-per-kind is the emitter's contract; the file format only requires that the value is a JSON object. |
| **correlation fields** | see §5 | conditional | Present unless emitted outside any flow (specifically: cert-mint audits omit them). |

### 4.4 `kind: "resolved"`

The Resolver's output for one flow — the category → canonical-value
map produced by running every collected `Hint` through ADR 004's
resolution algorithm at flow finish. One `Resolved` per drained flow.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `kind` | `"resolved"` | yes | Discriminator. |
| `session_prefix` | string (8 hex chars) | yes | First 8 hex characters of the hash-derived `SessionId` (ADR 020 §5.1). Pre-040.a convenience key; retained for back-compat. |
| `session_id` | string | conditional | The full ADR 028 `MarkingSessionId` value when the flow carried one. Distinct from `session_prefix` (which is hash-derived from request headers). |
| `flow_id` | integer | yes | Engine-assigned per-flow identifier. |
| `at_unix_ms` | integer | yes | Drain-stamp wall-clock in milliseconds. |
| `resolved` | object | yes | Category → canonical-value map (`{"tool": "Claude Code", "work_type": "refactor"}`). May be empty when the flow produced no hints. |
| **correlation fields** | see §5 | yes | `ResolvedRecord` is engine-emitted at the drain seam; the correlation block is always populated. |

## 5. The correlation block (ADR 023 §2.3)

Four optional top-level keys, flattened onto every variant by the
engine drain seam:

| Key | Type | Source |
|---|---|---|
| `event_id` | string | The proxy's per-round-trip identifier (today: `request_id`, e.g. `nl-42`). Matches `tap.jsonl`'s `event_id` and `roundtrips.jsonl`'s `event_id`. |
| `turn_id` | string (ULID, optional) | From the marking detector's decision (`MarkingDecision.turn_id`). Omitted when the cell has no marking detector or extraction failed. |
| `session_id` | string (optional) | The full `MarkingSessionId` value when present. **Not** the 8-char `session_prefix` (which lives on the `resolved` variant only). |
| `agent_run_id` | string (ULID, optional) | Reserved for story 040.c; currently always omitted. Consumers must tolerate absence. |

**Stamping seam.** The single point that fills these fields is
`InspectionEngine::drain_to_sink` (noodle-core, ADR 020 §2.3 +
§2.1). Transforms emit side-effects without correlation; the engine
decorates each record on drain. Bypass-resistant by construction —
no other path stamps.

**The cert-mint exception.** `ExternalCertMintService` records
`LeafMinted` / `MintFailed` audits **directly** to the sink — they
happen at TLS-handshake time, not inside any inspection flow.
These records:

- Carry `flow_id = 0` (sentinel; no inspection flow exists).
- Carry `at_unix_ms` stamped at emission, not at drain.
- Omit all four correlation keys (`event_id`, `turn_id`, `session_id`,
  `agent_run_id`).

Downstream filters key off `flow_id == 0` to recognise the
out-of-flow path. Consumers that want only LLM-round-trip audits
filter `flow_id != 0`.

## 6. Schema evolution

**Additive only.**

- New top-level keys may be added to any variant without bumping a
  version. Consumers must ignore unknown keys.
- New `kind_inner` values (audit sub-kinds) may be added without
  bumping a version. Consumers must tolerate unknown sub-kinds; the
  recommended default is to surface the record with its `detail`
  payload intact and not error.
- A new top-level `kind` discriminator value is a major schema
  event; it requires a separate ADR. Adding a fifth `SideEffect`
  enum variant is the same event.
- Existing field semantics are immutable. Renaming a field is a major
  schema event.

## 7. Producers and consumers

**Producer.** The shipped `SideEffectsJsonlSink` adapter in
`noodle-adapters::sink`. Composed under `MultiSideEffectSink`
alongside `TracingSink` and `RoundTripSink` by
`noodle-proxy::tap_setup::install`.

**Consumers.**

| Consumer | Purpose |
|---|---|
| `noodle-viewer` side-effects panel | Per-emission browse of live + replayed effects |
| Endpoint Product live monitors | Real-time UI updates on attribution / audit events |
| Future Watchtower policy logic | In-process subscription to the upstream bus; this file is the durable mirror |
| Operator `jq` queries | Ad-hoc forensic investigation |
| Cert-mint observability tooling | `flow_id == 0` filter for `leaf_minted` / `mint_failed` audits |

**Not a consumer:** `noodle-embellish` (slice 042). The
embellishment processor reads `tap.jsonl` + `roundtrips.jsonl` only —
the per-round-trip aggregation already embeds every contributing
side-effect in its `evidence` block (ADR 023 §4). `side_effects.jsonl`
exists for the streaming / per-emission / cert-mint use cases that
`roundtrips.jsonl` structurally cannot serve.

## 8. Relationship to sibling files

| File | Granularity | Records per round trip | Spec |
|---|---|---|---|
| `tap.jsonl` | per-direction wire capture | 2 (request + response) | ADR 027 |
| `side_effects.jsonl` | per-emission stream | N (hints + artifacts + audits + resolved) | this ADR |
| `roundtrips.jsonl` | per-round-trip aggregate | 1 | ADR 023 §4 |

All three are siblings in `~/.noodle/`. All three join by `event_id`.
`roundtrips.jsonl` duplicates the side-effects for a given round
trip inside its `evidence` block; that redundancy is intentional —
it makes the round-trip record self-contained for primary-feed
consumers, while `side_effects.jsonl` retains per-emission ordering
and the cert-mint records that `roundtrips.jsonl` structurally
cannot hold.
