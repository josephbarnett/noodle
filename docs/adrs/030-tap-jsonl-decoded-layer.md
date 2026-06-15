# ADR 030 — `tap.jsonl` decoded layer and cross-record pairing

**Status:** current. Specifies the decoded-content fields,
cross-record pairing references, and `noodle-domain` type
annotations that extend the `tap.jsonl` record shape defined in
ADR 027.

**Resolves:** the gap flagged in `doc-gaps-status.md` §4 — ADR 027
pins the HTTP-level envelope (headers, body bytes, marks,
extractions) but leaves the decoded layer unspecified. Without
decoded fields, every consumer re-parses SSE streams and re-pairs
`tool_use_id` chains. The viewer already does (`ooda.ts:270+`); a
second consumer would do it differently.

**Related:** ADR 027 (envelope, marks, side-channel split — this
ADR extends without revising), ADR 028 (marks block, session and
turn identifiers, sub-agent stack semantics), ADR 029
(`noodle-domain` type vocabulary — every domain-typed field on
`tap.jsonl` carries types defined there), ADR 015 §L4–L5 (codec
output structures that the decoded layer mirrors),
`crates/noodle-viewer/web/src/store/derived/ooda.ts` (the
reference implementation that this ADR replaces with a
record-level contract).

---

## Goal

The goal of this ADR is to make `tap.jsonl` a **complete evidence
boundary** — every fact a consumer needs is recorded in typed
fields, and no consumer re-parses wire data or re-derives
relationships the proxy already determined.

Concretely: a `tap.jsonl` reader builds either the **HTTP view**
(envelope + headers + body bytes — already specified by ADR 027)
or the **OODA view** (decoded content blocks + tool-use lineage +
turn membership + domain-typed annotations — specified here) by
reading the record's fields and following cross-record references.
Neither view requires SSE parsing, content-block grammar
knowledge, or string-matching on body bytes.

### Why

The proxy already decodes wire content for every request and
response — the codec layer (ADR 015 L0–L5) parses SSE streams and
content blocks before the marking detector can read
`stop_reason`. Throwing the decoded structure away after the
proxy has used it, and forcing every downstream consumer to
re-do the work, is wasteful and incoherent: every consumer would
arrive at a slightly different decoding, the viewer's logic
already diverges from any future consumer, and the canonical
record stops being canonical the moment a consumer disagrees
with another consumer about what a stream contained.

Persisting the decoded layer on `tap.jsonl` reverses that:

1. **Decode once.** The proxy parses; the record carries the
   parsed structure.
2. **Pair once.** The proxy knows which `tool_use` was answered
   by which `tool_result` (they appear in adjacent flows on the
   same session); the records cross-reference each other.
3. **Annotate once.** `noodle-domain` (ADR 029) classifies every
   content block; the classifications travel on the record.

Consumers downstream of `tap.jsonl` read typed fields and follow
pointers. No re-parsing.

### What this ADR specifies

1. The **relationship to ADR 027** (§1) — extension, not
   revision. ADR 027 fields remain authoritative; this ADR adds
   fields.
2. The **decoded content blocks** (§2) — the parsed structure of
   request and response bodies as typed JSON.
3. The **SSE event stream representation** (§3) — parsed events
   as a typed list, not raw event-stream bytes.
4. **Tool-use cross-record pairing** (§4) — the bidirectional
   references between `tool_use` blocks and `tool_result` blocks
   that appear in adjacent records.
5. **Sub-agent spawn references** (§5) — the `spawn_tool_use_id`
   pointer that links a sub-agent's first round-trip back to the
   parent's spawn site.
6. **Domain-typed annotations** (§6) — which `noodle-domain` type
   family annotates which decoded field.
7. **Schema versioning** (§7) — how decoded-layer additions are
   versioned so older consumers don't break.

### Non-goals

- **Identity resolution.** Tying records to specific humans or
  teams is the embellishment plane (story 028). The decoded
  layer carries content typing, not actor identity.
- **Side-effects.** `Hint`, `AuditEvent`, `ResolvedRecord` are
  on `side-effects.jsonl` per ADR 027 §5. The decoded layer is
  about wire content, not derived facts.
- **Wire-codec internals.** How the proxy decodes is owned by
  ADR 015. This ADR pins what the record carries after decoding.

---

## 1. Relationship to ADR 027

ADR 027 is the authoritative envelope spec. This ADR adds three
field groups to every record:

| Field group | Where on the record | Added by this ADR |
|---|---|---|
| `content` | both request and response records | structured decoded content blocks (§2) |
| `events` | response records only | parsed SSE event list (§3) |
| `pairing` | both, when applicable | cross-record references (§§4–5) |

ADR 027's fields — `request_id`, `direction`, `ts_unix_ms`,
`domain`, `endpoint`, `headers`, `body_in`, `body_out`,
`session_id`, `turn_id`, `parent_session_id`, `extractions` —
remain exactly as specified. None of them change.

Records can be read at the HTTP projection level using only
ADR 027 fields, or at the OODA projection level using ADR 027
fields plus the additions below. A consumer chooses which
projection to consume; the record carries both.

---

## 2. Decoded content blocks

The `content` field carries the parsed structure of the body.
Its shape mirrors the canonical content-block grammar (Anthropic's
`messages[].content[]` array, OpenAI's `choices[].delta.content`,
etc.) normalised to a single typed list.

### 2.1 Field shape

```json
"content": {
  "schema_version": 1,
  "blocks": [
    {
      "kind": "text",
      "text": "Find a bug in this code",
      "annotations": {
        "speech_act":  "Instruction",
        "category":    "Prose",
        "trust":       "UserTrusted"
      }
    },
    {
      "kind": "tool_use",
      "tool_use_id": "tu_01ABC...",
      "tool_name": "Read",
      "input": { "path": "/repo/main.rs" },
      "annotations": {
        "capability":  "ReadFile"
      },
      "pairing": {
        "resolved_by_request_id": "01HQ8F..."
      }
    },
    {
      "kind": "tool_result",
      "tool_use_id": "tu_01ABC...",
      "is_error": false,
      "content": [
        { "kind": "text", "text": "fn main() { ... }" }
      ],
      "pairing": {
        "resolves_tool_use_in_request_id": "01HQ8E..."
      }
    },
    {
      "kind": "thinking",
      "text": "The user wants me to find a bug...",
      "annotations": {
        "speech_act": "Reasoning",
        "category":   "Reasoning"
      }
    }
  ]
}
```

### 2.2 Block kinds

The canonical kinds, drawn from cross-vendor recurrence (per
ADR 029 §3):

| Kind | Carries | Notes |
|---|---|---|
| `text` | UTF-8 text | The default content block. Annotated with `speech_act` and `category`. |
| `tool_use` | `tool_use_id`, `tool_name`, `input` (JSON value) | Annotated with `capability`. Carries pairing reference (§4). |
| `tool_result` | `tool_use_id`, `is_error`, nested `content` | Carries pairing reference (§4). |
| `thinking` | UTF-8 text | Model reasoning channel. Annotated as `Reasoning`. |
| `image` | media type, base64 bytes or URI | Used in vision-capable round-trips. |
| `system_reminder` | UTF-8 text, `reminder_subtype` annotation | Host-injected per-turn payloads (ADR 028 §1.1). |
| `redacted` | reason | A block whose original content was removed by a policy redaction transform. The reason is human-readable. |

Vendor-specific kinds that the recurrence rule has not yet
admitted (ADR 029 §3) carry `kind: "vendor_specific"` with a
`vendor_kind` field that records the vendor's name verbatim.
Consumers that do not know the vendor treat the block as opaque
text using `closest_canonical` if provided.

### 2.3 Annotations

Annotations carry `noodle-domain` types. The mapping:

| Annotation field | Family (ADR 029 §2) | Applies to kinds |
|---|---|---|
| `speech_act` | `speech_act` | `text`, `thinking` |
| `category` | `content_category` | `text`, `thinking`, nested `tool_result` content |
| `trust` | `trust_level` | All kinds (where determinable) |
| `capability` | `capability` | `tool_use` |
| `reminder_subtype` | `reminder_subtype` | `system_reminder` |
| `citations` | array of `citation_ref` | `text`, `thinking` (where references are detected) |

Annotation absence is meaningful: a missing `speech_act` means
the classifier produced no result, not "unknown". Consumers
treat missing annotations as "skip classification-aware
behaviour for this block."

---

## 3. SSE event stream representation

Response records carry the parsed event stream as a typed list.
The raw bytes remain in `body_in` / `body_out` (ADR 027); the
parsed list is additional.

### 3.1 Field shape

```json
"events": [
  { "ts_offset_ms": 12, "type": "message_start",
    "message": { "id": "msg_01XYZ...", "model": "claude-haiku-4-5",
                 "usage": { "input_tokens": 1024 } } },
  { "ts_offset_ms": 18, "type": "content_block_start",
    "index": 0, "content_block": { "type": "text" } },
  { "ts_offset_ms": 22, "type": "content_block_delta",
    "index": 0, "delta": { "type": "text_delta", "text": "Hello" } },
  { "ts_offset_ms": 156, "type": "content_block_stop", "index": 0 },
  { "ts_offset_ms": 158, "type": "message_delta",
    "delta": { "stop_reason": "end_turn" },
    "usage": { "output_tokens": 42 } },
  { "ts_offset_ms": 159, "type": "message_stop" }
]
```

`ts_offset_ms` is the offset from the response record's
`ts_unix_ms`. Consumers reconstruct absolute timestamps by
addition. Storing offsets keeps the field compact for long
streaming responses.

### 3.2 Event types

The canonical event types are normalised across vendors. The
table mirrors Anthropic's SSE event names (the recurrence-anchor
vendor for this surface; ADR 029 §3); vendor-specific events
that have not been admitted to canonical use a
`type: "vendor_specific"` variant with the vendor's name.

| Type | When | Carries |
|---|---|---|
| `message_start` | beginning of response | `message.id`, `message.model`, `usage` (input tokens) |
| `content_block_start` | new block opens | `index`, `content_block` metadata |
| `content_block_delta` | block content arrives | `index`, `delta` |
| `content_block_stop` | block ends | `index` |
| `message_delta` | mid- or end-stream | `delta` (carries `stop_reason`), `usage` |
| `message_stop` | end of response | — |
| `ping` | keepalive | — |
| `error` | stream-level error | `error.type`, `error.message` |

### 3.3 Why both raw bytes AND parsed events?

The raw bytes (`body_in` / `body_out`) carry **fidelity** — they
preserve the exact wire encoding for replay, debugging, and
audit. The parsed event list carries **consumability** — it
removes the SSE framing and JSON-payload parsing burden from
every consumer. The two are redundant by design.

When the proxy mutates a response (e.g. strips markers), the
raw `body_out` reflects the mutation; the parsed `events` list
reflects the **post-mutation** structure (what the client saw).
A consumer that wants the pre-mutation structure reads
`body_in` and re-parses; this is a rare audit-level case, not
the consumer-friendly path.

---

## 4. Tool-use cross-record pairing

`tool_use` blocks in response records appear in adjacent
request records as `tool_result` blocks. The pairing is on
`tool_use_id`. The proxy knows both records exist on the same
session; the record schema cross-references them so consumers
never scan.

### 4.1 Forward reference (response → next request)

When a response record's `content.blocks[]` contains a
`tool_use` block, the block carries:

```json
"pairing": {
  "resolved_by_request_id": "01HQ8F..."  | null
}
```

The value is the `request_id` of the record where the matching
`tool_result` was observed. `null` if the matching
`tool_result` has not yet been observed at write time (the
proxy writes the record immediately; the back-reference is
filled in by a back-patch step when the matching record is
written, or remains `null` if the conversation ends before the
tool_result arrives).

### 4.2 Backward reference (request → prior response)

When a request record's `content.blocks[]` contains a
`tool_result` block, the block carries:

```json
"pairing": {
  "resolves_tool_use_in_request_id": "01HQ8E..."  | null
}
```

The value is the `request_id` of the record where the matching
`tool_use` was observed. `null` only in the pathological case
where the proxy first saw the `tool_result` without ever having
seen the originating `tool_use` (proxy restart mid-session;
ADR 028 §10 #1).

### 4.3 Implementation: the proxy's back-patch step

The forward reference (response → next request) requires
back-patching because the response is written before the next
request arrives. The proxy maintains a `pending_tool_uses`
table keyed by `(session_id, tool_use_id)`. When a request
record carries a `tool_result`, the proxy:

1. Looks up the originating response record's `request_id`.
2. Writes the request record with its
   `resolves_tool_use_in_request_id` back-reference filled.
3. Updates the prior response record's
   `resolved_by_request_id` field via an idempotent rewrite
   (append-only `tap.jsonl` consumers handle this via a
   "patch event" — see §7.3).

If the proxy cannot rewrite in place (the sink is append-only
by policy), the back-reference is emitted as a side-effect
record on `side-effects.jsonl` (ADR 027 §5) with `kind:
"pairing_resolved"`. Consumers that need the forward reference
join across the two files.

---

## 5. Sub-agent spawn references

Sub-agents share their parent's wire-level session header but
have their own noodle `session_id` (ADR 028). The relationship
between parent and sub-agent is two pointers:

1. **`parent_session_id`** on the sub-agent's marks block
   (already specified by ADR 027 §4.2 and ADR 028).
2. **`spawn_tool_use_id`** on the sub-agent's first round-trip,
   pointing to the parent's `Agent` tool_use_id that spawned
   it.

### 5.1 Field shape

```json
"pairing": {
  "spawn_tool_use_id":         "tu_01PARENT...",
  "spawn_request_id":          "01HQ8E...",
  "parent_session_id":         "sess_parent_abc"
}
```

This block lives on the sub-agent's first round-trip's marks
section (extending ADR 027 §4.2's `<per-cell fields>`).
Subsequent round-trips of the same sub-agent run carry only
`parent_session_id` from the marks block (per ADR 028); the
spawn pointer appears only on the first round-trip.

### 5.2 Why the spawn pointer is separate from `parent_session_id`

`parent_session_id` answers: "which parent session is this
sub-agent's parent?" — a session-level question.
`spawn_tool_use_id` answers: "which specific spawn point inside
the parent's reasoning is this sub-agent?" — a turn-level
question.

A consumer reconstructing the OODA tree wants both: the parent
session for ancestry, the spawn point for placement in the
parent's reasoning timeline.

---

## 6. Domain-typed annotations — summary

Every place in this ADR where a `noodle-domain` type appears as
a field value, the binding is one-to-one with an ADR 029
family. Consolidated for reference:

| Record field path | ADR 029 family | Variant set |
|---|---|---|
| `content.blocks[].annotations.speech_act` | `speech_act` | `Instruction`, `Claim`, `Question`, `HedgedClaim`, `Suggestion`, `Acknowledgement`, `Refusal`, `Clarification`, `VendorSpecific{...}` |
| `content.blocks[].annotations.category` | `content_category` | `Code`, `Command`, `Credential`, `Pii`, `Secret`, `Prose`, `StructuredData`, `Path`, `Url`, `Reasoning`, `Plan`, `VendorSpecific{...}` |
| `content.blocks[].annotations.trust` | `trust_level` | `SystemTrusted`, `UserTrusted`, `ModelOutput`, `ToolOutput`, `InjectedReminder`, `VendorSpecific{...}` |
| `content.blocks[].annotations.capability` | `capability` | `ReadFile`, `WriteFile`, `Execute`, `NetworkRequest`, `NetworkListen`, `SpawnAgent`, `SystemQuery`, `EnvironmentRead`, `VendorSpecific{...}` |
| `content.blocks[].annotations.reminder_subtype` | `reminder_subtype` | `SkillCatalogue`, `ToolAvailability`, `ContextRefresh`, `WorkingDirState`, `SafetyClassifier`, `LongConversation`, `VendorSpecific{...}` |
| `content.blocks[].annotations.citations[]` | `citation_ref` | `FilePath`, `UrlReference`, `LineRange`, `CommitHash`, `IssueRef`, `VendorSpecific{...}` |
| `envelope.provider`, `envelope.endpoint`, `envelope.direction` | `envelope_metadata` | record-level dispatch facts |
| `envelope.agent_app`, `envelope.machine`, `envelope.collector_app` | `observation_context` | `AgentApp`, `Machine`, `CollectorApp` (struct shapes — ADR 029 §2.4) |
| `envelope.principal` | `principal_identity` | `PrincipalIdentity` (struct shape — ADR 029 §2.4) |
| `envelope.subscription.api_key` | `subscription_context` | `ApiKeyFingerprint { prefix, kind, source }` (struct shape — ADR 029 §2.4). Prefix populated from sensitive-header redaction (ADR 027 §9). |
| `envelope.subscription.organization` | `subscription_context` | `OrganizationContext { organization_id, parent_organization_id, account_type }` |
| `envelope.subscription.tier` | `subscription_context` | `SubscriptionTier` (often enrichment-plane populated; absent on the wire) |
| `usage.tokens`, `usage.latency`, `usage.retries` | `usage` | `TokenUsage`, `Latency`, `RetryCount` (struct shapes — ADR 029 §2.4) |
| `events[].vendor_extras` (where applicable) | `envelope_metadata` | per-vendor opaque fields preserved verbatim |
| (response-record-level) `turn_end_reason` | `turn_end` | `EndTurn`, `MaxTokens`, `ToolUsePending`, `StopSequence`, `ContentFiltered`, `VendorSpecific{...}` |

`task_plan` types (ADR 029 §2 family #8) annotate **block
sub-content** when a block's text contains a plan: the block's
annotations include a `plan_items: [TodoItem, …]` field whose
values come from ADR 029's `task_plan` family.

---

## 7. Schema versioning

`tap.jsonl` records are append-only and long-lived. Consumers
written today must continue reading records written years from
now without crashing on new fields or new variants.

### 7.1 Record-level version

Every record carries `schema_version: <integer>` at the top
level. The current version, inclusive of this ADR's additions,
is **2** (ADR 027 was version 1; this ADR's additions are
version 2). Future ADRs increment.

| Schema version | Anchor ADR | What was added |
|---|---|---|
| 1 | ADR 027 | Envelope, marks, body fields, extractions |
| 2 | ADR 030 (this) | `content`, `events`, `pairing` field groups |

### 7.2 Forward compatibility — additive only

Within a major version, only additive changes are permitted:

- New optional fields — allowed.
- New block kinds, new event types, new annotation families
  — allowed (consumers handle the unknown variant per ADR 029
  §4.1).
- Removing a field, renaming a field, or changing the semantics
  of an existing field — **forbidden** without a major-version
  bump.

A major-version bump triggers a new file (the sink rolls
`tap.jsonl` to `tap.v2.jsonl`); consumers detect the version
from the first record.

### 7.3 Patch events

To support the back-patching described in §4.3 on append-only
sinks, a `tap.jsonl` record of type `"patch"` may appear:

```json
{
  "schema_version": 2,
  "direction":      "patch",
  "target_request_id": "01HQ8E...",
  "patches": [
    {
      "path":  "content.blocks[2].pairing.resolved_by_request_id",
      "value": "01HQ8F..."
    }
  ]
}
```

Consumers maintaining an in-memory view apply patches as they
arrive. Consumers re-reading from disk apply patches by sorting
records and applying patches in order. Sinks that support
in-place rewrites may omit patch events and rewrite the
original record; the wire format supports both modes.

---

## 8. Worked example — one round-trip, both projections

A single Anthropic `/v1/messages` round-trip with one tool call.

### 8.1 Request record

```json
{
  "schema_version": 2,
  "request_id": "01HQ8E3K2X9JEMR7B5W4N2VYCG",
  "direction":  "request",
  "ts_unix_ms": 1716123456789,
  "domain":     "api.anthropic.com",
  "endpoint":   "/v1/messages",
  "headers":    [
    { "name": "Content-Type", "value": "application/json" },
    { "name": "X-Claude-Code-Session-Id",
      "value": "73f10dee-ea29-4d3b-8e34-9e0563cc0e15" }
  ],
  "session_id":        "sess_abc123",
  "parent_session_id": null,
  "turn_id":           "turn_7c2a",
  "body_in":           "{\"model\":\"claude-haiku-4-5\",\"messages\":[{\"role\":\"user\",\"content\":\"Read /repo/main.rs\"}],...}",
  "body_out":          "{\"model\":\"claude-haiku-4-5\",\"messages\":[{\"role\":\"user\",\"content\":\"Read /repo/main.rs\"}],...}",
  "content": {
    "schema_version": 1,
    "blocks": [
      {
        "kind": "text",
        "text": "Read /repo/main.rs",
        "annotations": {
          "speech_act": "Instruction",
          "category":   "Prose",
          "trust":      "UserTrusted"
        }
      }
    ]
  }
}
```

### 8.2 Response record

```json
{
  "schema_version": 2,
  "request_id": "01HQ8E3K2X9JEMR7B5W4N2VYCG",
  "direction":  "response",
  "ts_unix_ms": 1716123459012,
  "domain":     "api.anthropic.com",
  "endpoint":   "/v1/messages",
  "headers":    [ { "name": "Content-Type", "value": "text/event-stream" } ],
  "session_id":        "sess_abc123",
  "parent_session_id": null,
  "turn_id":           "turn_7c2a",
  "body_in":           "event: message_start\ndata: ...",
  "body_out":          "event: message_start\ndata: ...",
  "extractions":       {},
  "turn_end_reason":   "ToolUsePending",
  "content": {
    "schema_version": 1,
    "blocks": [
      {
        "kind": "thinking",
        "text": "I'll read the file to look for issues.",
        "annotations": {
          "speech_act": "Reasoning",
          "category":   "Reasoning"
        }
      },
      {
        "kind": "tool_use",
        "tool_use_id": "tu_01ABCDEF",
        "tool_name":   "Read",
        "input":       { "path": "/repo/main.rs" },
        "annotations": { "capability": "ReadFile" },
        "pairing":     { "resolved_by_request_id": "01HQ8F4M3Y0KEN8C6X5O3WZDH" }
      }
    ]
  },
  "events": [
    { "ts_offset_ms":   8, "type": "message_start",
      "message": { "id": "msg_01XYZ", "model": "claude-haiku-4-5" } },
    { "ts_offset_ms":  18, "type": "content_block_start",
      "index": 0, "content_block": { "type": "thinking" } },
    { "ts_offset_ms": 145, "type": "content_block_stop", "index": 0 },
    { "ts_offset_ms": 152, "type": "content_block_start",
      "index": 1, "content_block": {
        "type": "tool_use", "id": "tu_01ABCDEF", "name": "Read" } },
    { "ts_offset_ms": 198, "type": "content_block_stop", "index": 1 },
    { "ts_offset_ms": 200, "type": "message_delta",
      "delta": { "stop_reason": "tool_use" } },
    { "ts_offset_ms": 201, "type": "message_stop" }
  ]
}
```

Read at the HTTP projection: a consumer wants the wire — read
`headers`, `body_in`, `body_out`. Read at the OODA projection:
a consumer wants the activity — read `content.blocks`,
`turn_end_reason`, follow `pairing.resolved_by_request_id` to
the next record. Both views, one record. No re-parsing.

---

## 9. Open questions

1. **Vendor extras on `events`.** Some vendors emit
   server-specific events (Anthropic's `message_limit`,
   OpenAI's `delta.usage_metadata`) that don't yet recur across
   three vendors. They're preserved verbatim under
   `events[].vendor_extras`. Whether the recurrence rule
   eventually promotes any of them is open.
2. **Plan-parsing depth.** ADR 029's `task_plan` family
   recognises todo items and plan steps; the parsing of richer
   plan structures (nested goals, constraints with weights)
   is deferred to the classifier implementation.
3. **Annotation provenance.** A `speech_act` annotation may be
   the output of a deterministic rule, an ML classifier, or a
   classifier with confidence < 1.0. Whether the record carries
   provenance (`annotation_source: {rule | model | ensemble}`)
   is deferred until a consumer needs to disagree with one
   classifier and accept another.
4. **Patch event compaction.** If patches accumulate, a long
   `tap.jsonl` becomes patch-heavy. A compaction pass that
   rewrites the file with patches applied may be useful;
   deferred until a real consumer surfaces the cost.
5. **Size budgets.** Decoded content adds substantially to
   record size for large responses. Whether to support an
   `omit_content` cell-level config (record envelope only, no
   decoded layer) is deferred until a size-sensitive deployment
   surfaces.
