# ADR 027 — The `tap.jsonl` boundary format

**Status:** current. Defines the wire format of the boundary file that
the shipped `WireSink` implementation (`noodle-tap`) writes and that
every downstream consumer reads.

**Related:** ADR 001 §1.2 (the boundary), ADR 001 §2 principle 7
(one boundary out — a sink), ADR 015 (the types that source the
record — `NormalizedRequest`, `NormalizedEvent`, `Marker`), ADR 020
(`SideEffect` variants — §5 below pins the relationship between
`tap.jsonl` and the separate `SideEffectSink`).

---

## 1. Context

`tap.jsonl` is the only artefact noodle externalises to a destination
the operator can read. Every downstream consumer — the TAP viewer,
the telemetry collector, security analysis, behaviour and usage
analysis, ad-hoc tooling — reads this file. The file is the API
contract noodle publishes to the world.

The schema deserves its own ADR for four reasons:

1. **Wire formats evolve independently of architecture.** New fields
   land in the file without changing the proxy's internal types.
   Schema-level changes must be reviewable independently from
   architectural changes.
2. **Downstream consumers depend on the spec.** Multiple consumers,
   including consumers written in other languages (Python, Go,
   TypeScript), need an unambiguous spec they can read without
   reading the architecture document.
3. **Schema specifications need machinery — JSON Schema, canonical
   examples, foreign-consumer guidance, versioning policy.** Folding
   all of that into the architecture doc bloats it.
4. **The side-channel question (§5)** — whether `Hint` / `Artifact` /
   `AuditEvent` / `ResolvedRecord` land in `tap.jsonl` or on a sibling
   sink — is a schema question, not an architecture question. It lives
   here.

This ADR is the canonical reference for what's in `tap.jsonl`.

## 2. Record framing

- **One JSON object per line.** Line-delimited JSON (NDJSON / JSONL).
  Each record is a complete, self-contained JSON object terminated by
  `\n` (U+000A). No partial records; no multi-line objects.
- **UTF-8.** The file is UTF-8. Records that carry non-UTF-8 binary
  bodies encode them as base64 (§4.3) — the JSON document itself is
  always valid UTF-8.
- **Append-only.** The proxy never edits a record once written. Lines
  are appended in observation order.
- **No header line.** A consumer that tails the file from `EOF` is
  immediately positioned to read new records. A consumer that reads
  the file from offset 0 sees a stream of records starting at the
  oldest retained record.
- **Rotation.** The shipped `noodle-tap` implementation rotates the
  file at a configured size / age boundary. Rotated files are
  numbered (`tap.jsonl`, `tap.jsonl.1`, `tap.jsonl.2`, …) following
  the standard `logrotate` convention. Cross-file ordering is
  consumer-responsible.

### 2.1 `WireSink` and `WireSource` — the boundary's two roles

The `tap.jsonl` record schema defines a **boundary contract**, not
a file format. The same record schema flows in two directions
across the boundary:

| Role | Direction | Implementations |
|---|---|---|
| **`WireSink`** | Records flow **in** (the boundary receives). The proxy emits via a `WireSink`. | File (`noodle-tap` → `tap.jsonl`), TCP socket, in-memory channel, message queue (Kafka, NATS), OTLP endpoint, RDBMS writer. |
| **`WireSource`** | Records flow **out** (the boundary emits). Consumers read via a `WireSource`. | File reader (tails `tap.jsonl`), TCP socket reader, in-memory channel reader, message-queue consumer, RDBMS query, OTLP receiver. |

The two roles are duals of the same contract: same record schema,
same field semantics, opposite I/O direction. An implementation
that opens a file for write and tails another file for read is
two implementations stacked, not one — `WireSink::File` writes;
`WireSource::FileTail` reads.

Any `WireSink` implementation pairs with the corresponding
`WireSource` implementation: a file sink writes a file that a file
source reads; a TCP sink emits to a socket that a TCP source
consumes; a queue sink publishes to a topic that a queue source
subscribes to.

**Implications:**

- **Decoders are source-agnostic.** Per-provider decoder libraries
  (ADR 029 §7) take a `WireSource`, not a file path. They work
  identically against `tap.jsonl`, a live TCP stream, an in-memory
  test channel, or a queue subscription.
- **Bridge programs are first-class.** "Read from `WireSource` A,
  transform, write to `WireSink` B" is the canonical shape of every
  export tool, replay tool, and backfill tool. They compose without
  reinventing read- or write-side logic.
- **`tap.jsonl` is the canonical default**, but the design does not
  privilege it. Deployments that prefer Kafka, OTLP, or a database
  swap the `WireSink` implementation; their consumers swap the
  `WireSource` implementation. The record schema this ADR specifies
  is unchanged.

The shipped `noodle-tap` crate is the file-based `WireSink`. A
file-based `WireSource` (a tailing reader) is the natural sibling;
both consume / produce the same JSONL record format defined in §3
onward.

## 3. Per-direction record shape

Records come in two shapes — **request** and **response** — paired by
`request_id`. Both shapes share an identification block and a marks
block; only the response shape carries `extractions`.

### 3.1 Request record

```json
{
  "request_id":         "01HQ8F3K2X9JEMR7B5W4N2VYCG",
  "direction":          "request",
  "ts_unix_ms":         1716123456789,
  "domain":             "api.anthropic.com",
  "endpoint":           "/v1/messages",
  "headers":            [
    {"name": "Content-Type",  "value": "application/json"},
    {"name": "Authorization", "value": "<redacted>"},
    {"name": "User-Agent",    "value": "claude-cli/0.4.2"}
  ],
  "session_id":         "abc123def4",
  "parent_session_id":  null,
  "turn_id":            "turn-7c2a",
  "body_in":            "{\"model\":\"claude-haiku-4-5\",\"messages\":[...],...}",
  "body_out":           "{\"model\":\"claude-haiku-4-5\",\"messages\":[...],\"system\":[{\"type\":\"text\",\"text\":\"<noodle:work_type/>\"}]}"
}
```

### 3.2 Response record

```json
{
  "request_id":         "01HQ8F3K2X9JEMR7B5W4N2VYCG",
  "direction":          "response",
  "ts_unix_ms":         1716123459012,
  "domain":             "api.anthropic.com",
  "endpoint":           "/v1/messages",
  "headers":            [
    {"name": "Content-Type", "value": "text/event-stream"}
  ],
  "session_id":         "abc123def4",
  "parent_session_id":  null,
  "turn_id":            "turn-7c2a",
  "body_in":            "event: message_start\ndata: {...}\n\nevent: content_block_delta\ndata: {\"delta\":{\"text\":\"<noodle:work_type>code-review</noodle:work_type>The function...\"}}\n\n...",
  "body_out":           "event: message_start\ndata: {...}\n\nevent: content_block_delta\ndata: {\"delta\":{\"text\":\"The function...\"}}\n\n...",
  "extractions":        {
    "work_type": "code-review"
  }
}
```

The shapes are deliberately close: a consumer reading `tap.jsonl`
without knowing the direction can find the body and headers in the
same place. The discriminator is the `direction` field. `extractions`
is present only on response records.

## 4. Field reference

### 4.1 Identification and transport metadata (both directions)

| Field | Type | Required | Description |
|---|---|---|---|
| `request_id` | string (ULID) | yes | Correlates the request and response records of the same exchange. ULID format — 26 characters, monotonically sortable by timestamp prefix. |
| `direction` | enum `"request"` \| `"response"` | yes | Discriminator. |
| `ts_unix_ms` | integer | yes | Observation timestamp in milliseconds since Unix epoch. Request records: the moment the proxy received the bytes on the client side. Response records: the moment the proxy received the bytes on the upstream side. |
| `domain` | string | yes | Request host (e.g. `api.anthropic.com`). For requests, the host the client targeted. For responses, the same host (the upstream the response came from). |
| `endpoint` | string | yes | Request path including query string (e.g. `/v1/messages`). Same value on both records of a pair. |
| `headers` | array of `{name, value}` objects | yes | Headers in original on-wire order. Sensitive headers (`Authorization`, `X-Api-Key`, cookies) have their `value` set to the marker string `"<redacted>"`. Redaction policy is per-deployment; the default redaction list is documented in §9. |

### 4.2 Marks (both directions)

Per ADR 001 §5.4 ("Mark" responsibility) — populated by the per-cell
marking detector at flow open and stamped on both records of the
pair.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | string | yes | Proxy-stamped session identifier. Identifies one conversation. |
| `parent_session_id` | string \| `null` | yes (nullable) | Present (non-null) when the marking detector identifies the request as a child of another session (sub-agent invocation). `null` for top-level sessions. |
| `turn_id` | string | yes | Identifies one user-intent-to-final-response cycle. One or more `request_id` pairs share the same `turn_id` when the marking detector recognises them as the same turn. |
| `<per-cell fields>` | varies | optional | Provider-specific correlation fields the cell's marking detector defines (e.g. an Anthropic cell may emit `x_claude_code_session_id`, a claude.ai cell may emit `conversation_uuid`, `parent_message_uuid`). Field names are scoped per cell; the cell's spec documents what it emits. |

### 4.3 Body fields (both directions)

| Field | Type | Required | Description |
|---|---|---|---|
| `body_in` | string | yes | Bytes received by the proxy on this direction. For requests: the bytes the client sent. For responses: the bytes the upstream sent. |
| `body_out` | string | yes | Bytes the proxy forwarded on this direction. For requests: the bytes forwarded to the upstream (differs from `body_in` if the request was injected). For responses: the bytes forwarded to the client (differs from `body_in` if markers were stripped). On passthrough cells, `body_out == body_in`. |

**Encoding.** Bodies that are valid UTF-8 are stored as JSON strings
directly. Bodies that are not valid UTF-8 (binary payloads, gzipped
content the proxy chose not to decompress) are base64-encoded and
prefixed with the marker `"data:base64;"`. A consumer recognises
binary bodies by the prefix:

```
"body_in": "data:base64;H4sIAAAAAAAAAytJLS4BAAx+f9gEAAAA"
```

Decompression is the proxy's decision per cell. If decompression
happened, `body_in` and `body_out` are the **decompressed** bytes
(both directions). The original on-wire `content-encoding` header is
preserved in `headers` so a consumer can detect the case.

### 4.4 Captured-during-mutation (response direction only)

| Field | Type | Required | Description |
|---|---|---|---|
| `extractions` | object `{name → value}` | response only | Values the proxy captured during in-band response mutation. The canonical example: `MarkerStripTransform` captures `<noodle:work_type>code-review</noodle:work_type>` and emits `{"work_type": "code-review"}`. The marker bytes are removed from `body_out` but the captured value is preserved here. Empty object `{}` when no extraction fired; **field absent** on records produced before extractions were possible (pre-ADR-017 records). |

`extractions` is the **only** class of derived data the proxy
externalises. It is byte-aligned with a wire record (we stripped
something from this response; here is what we stripped). All other
derived facts (`Hint`, `AuditEvent`, `ResolvedRecord`) land on the
separate `SideEffectSink` — see §5.

## 5. The side-channel question — where `Hint` / `AuditEvent` / `ResolvedRecord` land

This ADR pins `doc-gaps.md` cross-cutting issue #2.

**Decision: `tap.jsonl` carries `extractions` only. `Hint`,
`AuditEvent`, and `ResolvedRecord` land on a separate
`SideEffectSink`** (ADR 020) at a sibling location — by default,
`side-effects.jsonl` in the same directory.

| Side-effect type | Sink | Why |
|---|---|---|
| `Artifact` (captured-during-mutation values) | **`tap.jsonl`** (as the `extractions` field on the response record) | Byte-aligned with a wire record: the artifact is *what was removed* from this response's bytes. The record cannot be reconstructed without the artifact. |
| `Hint` (confidence-ranked attribution opinions) | `side-effects.jsonl` | Derived fact; not byte-aligned with a single wire record. May be emitted by detectors at flow open before any bytes have been observed. Multiple hints per flow per category; the relationship to a single record is many-to-one at most. |
| `AuditEvent` (operational events: `Injected`, `Redacted`, `Filtered`, `Errored`, `Overflow`, …) | `side-effects.jsonl` | Derived fact; describes what *the proxy did*, not what *was on the wire*. Separate observability stream. |
| `ResolvedRecord` (the `Resolver`'s end-of-flow output) | `side-effects.jsonl` | Derived fact; produced at flow-end from accumulated hints. Not byte-aligned with any single record. |

**Why this split.** `tap.jsonl` is the wire-record stream: what
crossed the boundary, in both directions, byte-faithful. Mixing in
derived facts that have no byte alignment makes the stream less
useful for the consumers that want the raw wire view (security
analysis, replay, packet-level audit). The `SideEffectSink` is the
right home for derived facts — they correlate to `tap.jsonl` records
via `flow_id` / `request_id` when correlation is needed.

`side-effects.jsonl` schema is out of scope for this ADR; it follows
the same JSONL framing convention (one JSON object per line, UTF-8,
`\n` terminated). The schema lives in ADR 020.

## 6. Versioning posture

- **No per-line version number.** Every record is the current schema.
- **Append-only.** New fields land as **additions**. Existing fields
  never change semantics; an existing field name never gets repurposed.
  If the meaning of a field needs to change, a new field name is
  introduced and the old one stays valid for a deprecation window.
- **Optional fields may become required.** A new required field that
  wasn't in the original schema means **older `tap.jsonl` files won't
  contain it**. Consumers must tolerate missing fields on old records.
- **Required fields never become optional.** Once required, always
  required.
- **Consumer contract: tolerate unknown fields.** Foreign consumers
  must ignore fields they don't recognise. This is the standard
  forward-compatibility posture for JSONL streams and is the only way
  to evolve the schema without coordination with every consumer at
  once.

## 7. JSON Schema

A canonical JSON Schema document for each direction is maintained at
`schemas/tap-jsonl/request.schema.json` and
`schemas/tap-jsonl/response.schema.json` in the noodle repository.
The schemas are versioned by file mtime in git history; the
append-only rule (§6) means newer schemas accept everything older
schemas accept.

The schemas are the **machine-readable specification** of this ADR.
A consumer that needs to validate records, generate types, or build a
parser starts from the schema, not from the prose in this document.
If the prose and the schema disagree, the schema is the source of
truth and the prose is updated.

(Schema files to be added in the implementation slice; this ADR pins
the schema-first commitment.)

## 8. Foreign consumer guidance

### 8.1 Python

```python
import json, base64

def read_records(path):
    with open(path, 'r', encoding='utf-8') as f:
        for line in f:
            yield json.loads(line)

def body_bytes(record, field):
    raw = record[field]
    if raw.startswith('data:base64;'):
        return base64.b64decode(raw[len('data:base64;'):])
    return raw.encode('utf-8')
```

### 8.2 Go

```go
type Record struct {
    RequestID       string             `json:"request_id"`
    Direction       string             `json:"direction"`
    TsUnixMs        int64              `json:"ts_unix_ms"`
    Domain          string             `json:"domain"`
    Endpoint        string             `json:"endpoint"`
    Headers         []Header           `json:"headers"`
    SessionID       string             `json:"session_id"`
    ParentSessionID *string            `json:"parent_session_id"`
    TurnID          string             `json:"turn_id"`
    BodyIn          string             `json:"body_in"`
    BodyOut         string             `json:"body_out"`
    Extractions     map[string]string  `json:"extractions,omitempty"`
}
```

Unknown fields are tolerated by default with `encoding/json`; no
extra effort required for forward compatibility.

### 8.3 TypeScript

```ts
type Direction = "request" | "response";

interface Record {
  request_id: string;
  direction: Direction;
  ts_unix_ms: number;
  domain: string;
  endpoint: string;
  headers: Array<{name: string, value: string}>;
  session_id: string;
  parent_session_id: string | null;
  turn_id: string;
  body_in: string;
  body_out: string;
  extractions?: Record<string, string>;  // response only
}
```

### 8.4 `jq` recipes

Pair request and response by `request_id`:
```
jq -s 'group_by(.request_id) | map({rid: .[0].request_id, pair: .})' tap.jsonl
```

All response-side extractions of `work_type`:
```
jq -c 'select(.direction == "response" and (.extractions.work_type // null) != null) | {turn: .turn_id, work_type: .extractions.work_type}' tap.jsonl
```

All requests for a session:
```
jq -c 'select(.session_id == "abc123def4" and .direction == "request")' tap.jsonl
```

## 9. Security considerations

- **Bodies are sensitive.** `body_in` and `body_out` contain the
  user's prompts and the model's responses verbatim. Treat
  `tap.jsonl` as confidential. File permissions on the shipped
  `noodle-tap` implementation default to mode `0600`
  (read-only-to-owner) on Unix; on Windows the equivalent ACL grants
  read only to the service account and the operator's user.
- **Header redaction with prefix preservation.** The default
  redaction list strips `Authorization`, `Cookie`, `Set-Cookie`,
  `X-Api-Key`, `Anthropic-Api-Key`, `Proxy-Authorization`, and any
  header matching the configured per-cell sensitive-header list.
  Redacted values are **not** wholly replaced — the first N visible
  characters of the value are preserved, followed by an ellipsis
  and a redaction marker. Default N is **12**, chosen to capture
  the provider tag plus enough body to identify the specific
  credential (`sk-ant-api03-wcq...<redacted>`,
  `sk-ant-sid02-abcd...<redacted>`, `sk-1234abcd...<redacted>`).
  N is configurable per-cell via the dispatch table; setting N=0
  yields full opacity (`<redacted>`) for cells that require it.

  The preserved prefix is what consumers use for billing
  reconciliation and credential correlation: it is **not a
  secret**, it cannot be used to authenticate, but it can be
  joined against an Anthropic Console dashboard or a credential
  inventory. This is the value the embellishment plane extracts
  to populate `ApiKeyFingerprint.prefix` (ADR 029 §2.4).

  Sensitive headers whose values are not prefix-meaningful
  (`Cookie`, `Set-Cookie`, opaque bearer tokens that aren't
  vendor-tagged) default to N=0 — full redaction. The default
  table specifies N per-header so the operator does not have to
  decide per cell.

  | Header | Default N | Rationale |
  |---|---|---|
  | `Authorization` | 12 | Vendor-tagged credential prefix; reconciliation value. |
  | `X-Api-Key` | 12 | Vendor-tagged credential prefix; reconciliation value. |
  | `Anthropic-Api-Key` | 12 | Vendor-tagged credential prefix; reconciliation value. |
  | `Cookie` | 0 | Opaque session blob; no prefix value. |
  | `Set-Cookie` | 0 | Opaque session blob; no prefix value. |
  | `Proxy-Authorization` | 0 | Opaque; preserve nothing. |
- **Marks are PII-adjacent.** `session_id`, `turn_id`, and per-cell
  correlation fields can identify users in combination with other
  data. Retention and access controls apply to the same standard as
  the bodies.
- **Extractions are intentional.** Whatever a `MarkerStripTransform`
  captures is intentionally externalised — that's the proxy's job —
  but the operator should review the catalog of markers in use to
  confirm the extracted content is what they expect.
- **Rotation, retention, deletion.** The proxy does not encrypt
  `tap.jsonl` at rest. Disk encryption and retention policy are
  operator-controlled. The proxy emits no records during the rotation
  swap; consumer tailers should reopen the file on rotation.
- **No integrity stamp today.** Records carry no signature or HMAC.
  An attacker with write access to the file can alter or remove
  records without detection from the proxy. File-system permissions
  are the only integrity protection. §10 lists this as a forward
  question.

## 10. Open questions deferred

- **Per-record signature / HMAC.** Would let consumers detect
  tampering. Cost: every record carries a signature; key distribution
  to consumers. Not specified; revisit if a real integrity threat
  surfaces.
- **Per-record compression.** Bodies dominate record size; on-the-fly
  gzip per record would shrink the file 5–10× for typical text
  bodies. Cost: every consumer must decompress. Not specified;
  revisit if `tap.jsonl` size becomes operationally painful.
- **Streaming versions of `body_in` / `body_out`.** A large streaming
  response currently lands as a single multi-MB JSON string. A
  variant that splits a streaming body across multiple per-frame
  records would let consumers process tokens before the response
  completes. Substantial schema change; out of scope here.
- **Schema-version sidecar.** A `tap.jsonl.schema-version` file in
  the same directory naming the schema commit hash. Operationally
  useful for archived files; not pinned today.
- **Rotation policy specifics.** Size / age threshold defaults,
  rotation-file naming, compression of rotated files. Implementation
  detail of `noodle-tap`; specified there, not here.
