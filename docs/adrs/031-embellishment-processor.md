# ADR 031 — Embellishment processor: tap.jsonl → SQLite → ship

**Status:** current. Specifies a reference implementation of the
embellishment plane: a standalone application that consumes
`tap.jsonl` via `WireSource` (ADR 027 §2.1), maps records into
`ai-telemetry` v0.0.2 events, writes them to a local SQLite
database, and hands the database off to a separate shipper.

**Resolves:** the gap flagged after ADR 029 + ADR 030 drafting — the
proxy now emits enough on `tap.jsonl` for downstream consumers to
reconstruct any target telemetry shape, but no ADR specifies the
reference processor that proves the boundary delivers value
end-to-end. Without this, the architecture is a set of file
formats with no validating consumer.

**Related:** ADR 001 §3 (component architecture — embellishment
plane is downstream of the proxy), ADR 001 principle 7 (proxy
does not ship telemetry directly), ADR 027 §2.1 (`WireSink` /
`WireSource` duality), ADR 029 (`noodle-domain` vocabulary used
throughout), ADR 030 (the decoded layer this processor reads), ADR
028 (marks the processor consumes). External reference:
`(external reference removed)/docs/design/ai-telemetry-event-schema.md`
(a generic AI-telemetry schema — the target shape).

---

## Goal

The goal of this ADR is to specify a **standalone embellishment
processor** that consumes noodle's `tap.jsonl` output and produces
target-schema-shaped telemetry events, with `ai-telemetry` v0.0.2
as the reference target. The processor is the validating consumer
that proves noodle's boundary delivers consumable value without
forcing the proxy itself to ship telemetry.

The processor:

1. Reads `tap.jsonl` (or any `WireSource`) record-by-record.
2. Applies a per-target **mapping function** to each record,
   producing one event in the target schema.
3. Writes events into a **local SQLite database**.
4. Hands the SQLite file off to a separate shipper process (out
   of scope — the telemetry backend's existing shipper, a custom uploader,
   OTLP exporter, or anything that reads SQLite).

### Why

Three reasons this is the right next iteration after the format
ADRs:

1. **Closes the value loop.** Every preceding ADR specifies *what
   noodle observes and what shape it externalises*. None of them
   prove that shape is **consumable**. A reference processor that
   produces `ai-telemetry` v0.0.2 from `tap.jsonl` is the
   end-to-end test of the boundary.

2. **Preserves the proxy's protocol-pure shape.** ADR 001
   principle 7: the proxy does not ship telemetry directly. A
   separate binary that does the shipping respects that boundary.
   The proxy emits `tap.jsonl`; the processor emits target events.
   Two binaries, two contracts.

3. **Decouples noodle from any specific target schema.** Cloud
   Zero's `ai-telemetry` v0.0.2 is one possible target.
   OpenTelemetry, OTLP-spans, Prometheus metrics, custom
   pipelines — all are downstream targets reached by writing a
   different mapping function against the same `WireSource` and
   the same SQLite handoff. The processor is target-agnostic by
   design; the mapping is target-specific.

### What this ADR specifies

1. The **architecture** (§1) — three independent components, three
   independent contracts.
2. The **WireSource consumption pattern** (§2) — how the processor
   tails `tap.jsonl` (or any `WireSource`) for live and at-rest
   reads.
3. The **SQLite schema** (§3) — one canonical events table per
   target schema, with `provider_metadata` and other free-form
   bags landing in JSON columns.
4. The **mapping function shape** (§4) — the interface a per-target
   mapping implements; the registration model.
5. The **`ai-telemetry` v0.0.2 mapping** (§5) — the concrete,
   field-by-field translation table for the reference target.
6. The **configuration surface** (§6) — how operators choose a
   target, point at a `WireSource`, point at a SQLite path.
7. The **failure modes** (§7) — what happens on DB lock, disk
   full, mapping errors, schema drift.

### Non-goals

- **The shipper.** Reading SQLite and emitting to a remote
  pipeline (the telemetry backend, OTLP, Kafka) is out of scope. This
  processor's contract ends at the SQLite file.
- **Cost computation.** `estimated_cost_usd` in `ai-telemetry`
  requires a pricing-table that varies per provider and tier.
  The mapping function may emit a placeholder; authoritative
  pricing is the shipper's or downstream pipeline's concern.
- **Cross-record state.** The processor processes each record
  independently. Cross-round-trip aggregation (per-turn token
  totals, per-session cost summaries) is downstream-pipeline
  work.
- **Identity resolution.** Resolving `client_username` to a
  provider email or a directory user is the embellishment plane
  (this ADR is **part of** the embellishment plane), but actual
  identity-source integration (Anthropic Console API, MDM, HRIS)
  is per-deployment configuration outside this processor.

---

## 1. Architecture

```
┌──────────────┐    tap.jsonl     ┌────────────────────┐    SQLite     ┌─────────────────┐
│ noodle-proxy │ ───────────────▶ │ noodle-embellish   │ ────────────▶ │ shipper         │
│  (WireSink)  │   (WireSource)   │ (this ADR)         │  (.sqlite)    │ (out of scope)  │
└──────────────┘                  └────────────────────┘               └─────────────────┘
        │                                  │                                   │
        │                                  │                                   │
   tap.jsonl                          SQLite file                       remote pipeline
   schema                             schema                            (the telemetry backend /
   (ADR 027, 030)                     (this ADR §3)                      OTLP / Kafka)
```

Three independent components. Three independent contracts. Each
arrow is a swap point:

- The left arrow swaps when `WireSink` swaps implementation (file,
  TCP, queue) — the processor consumes via `WireSource`
  regardless.
- The right arrow swaps when the shipper changes (different
  destination, different protocol) — the SQLite file is the
  uniform handoff.

The processor binary is **`noodle-embellish`**. It lives in the
noodle workspace as a new crate, depends on `noodle-core` and
`noodle-domain`, and ships as a standalone binary alongside
`noodle-proxy`.

### 1.1 Why a separate binary

Combining the processor with `noodle-proxy` would violate ADR 001
principle 7 (proxy does not ship telemetry directly) and ADR 001
§5.4 (proxy boundary ends at `WireSink`). Separating preserves the
proxy's protocol-pure shape and lets the processor be deployed
independently — operators who already have a downstream pipeline
that reads `tap.jsonl` directly skip the processor entirely.

### 1.2 Why a SQLite handoff (not direct shipping)

A SQLite intermediate gives the shipper a **queryable, durable,
re-readable** input. Three concrete benefits:

- **Re-shipping.** If the shipper fails mid-batch, it re-reads
  unsent rows from SQLite. The processor doesn't need to maintain
  a send queue.
- **Multiple shippers.** Two shippers can read the same SQLite
  file concurrently (one to the telemetry backend, one to OTLP) without
  coordination.
- **Operator inspection.** The operator can query the database
  with `sqlite3` to debug what was captured. JSONL is grep-friendly;
  SQLite is filter-and-aggregate-friendly.

A direct stream from the processor to the shipper (no SQLite
intermediate) is a valid alternative; it trades durability for
latency. Specifying SQLite as the default; alternative transports
remain valid future work.

---

## 2. WireSource consumption

The processor consumes records via `WireSource` (ADR 027 §2.1) —
the read-side dual of `WireSink`. Two consumption modes:

| Mode | Use case | Implementation |
|---|---|---|
| **Tailing** | Live processing while the proxy is running | `WireSource::FileTail` — opens `tap.jsonl`, reads existing records, then follows new appends (inotify on Linux, FSEvents on macOS, polling fallback elsewhere) |
| **Batch** | Re-processing historical records, backfill, replay | `WireSource::FileRead` — opens `tap.jsonl` (or rotated `tap.jsonl.N`), reads to EOF, exits |

Other `WireSource` implementations work without code changes in
the processor: a TCP `WireSource` lets the processor consume a
remote noodle deployment; an in-memory `WireSource` is the test
fixture.

### 2.1 Record stream guarantees

The processor relies on three guarantees from `WireSource`
(specified in ADR 027 and inherited here):

1. **Ordering preserved.** Records arrive in observation order.
2. **Pair correlation.** Request and response records share a
   `request_id`. The processor waits for both before emitting an
   event when the target schema requires the response (which
   `ai-telemetry` does — latency, tokens, status code all come
   from the response).
3. **Patch events.** Tool-use pairing references (ADR 030 §4.3)
   may arrive after the original record via patch events. The
   processor applies patches before emitting the affected event.

### 2.2 Buffering

Between request arrival and response arrival the processor holds
the request record in memory keyed by `request_id`. Bounded by
**N seconds** (default 300) and **M MB** (default 64) — records
older / larger than the bounds are emitted as **partial events**
with `error_type = "no_response_observed"`. The bounds are
configurable.

---

## 3. SQLite schema

One canonical events table per target schema. The
`ai-telemetry` v0.0.2 target ships as `ai_telemetry_v_0_0_2`.
Future target schemas add their own table.

### 3.1 Table shape — `ai_telemetry_v_0_0_2`

```sql
CREATE TABLE ai_telemetry_v_0_0_2 (
    -- Envelope (the telemetry backend ai-telemetry §Envelope)
    event_id              TEXT     PRIMARY KEY,    -- ULID, processor-minted
    schema_id             TEXT     NOT NULL,       -- "ai-telemetry"
    schema_version        TEXT     NOT NULL,       -- "0.0.2"
    event_type            TEXT     NOT NULL,       -- "api_call"
    timestamp             INTEGER  NOT NULL,       -- unix epoch ms

    -- Request
    request_id            TEXT,
    provider              TEXT     NOT NULL,
    model                 TEXT     NOT NULL,
    endpoint_path         TEXT     NOT NULL,
    endpoint_params_json  TEXT,                    -- JSON object
    streaming             INTEGER  NOT NULL,       -- 0/1
    status_code           INTEGER  NOT NULL,
    error_type            TEXT,
    latency_ms            INTEGER  NOT NULL,

    -- Cost
    input_tokens          INTEGER  NOT NULL,
    output_tokens         INTEGER  NOT NULL,
    estimated_cost_usd    REAL,                    -- nullable; pricing is downstream
    cost_model_version    TEXT,

    -- Credentialed identity
    api_key_prefix        TEXT,
    api_key_type          TEXT,                    -- api_key | session | oauth
    user_id               TEXT,                    -- enrichment-plane
    session_id            TEXT,
    session_hash          TEXT,

    -- Client / source
    client_user_agent     TEXT,
    client_username       TEXT,                    -- enrichment-plane
    client_hostname       TEXT,
    client_app            TEXT,
    client_lang           TEXT,
    client_runtime        TEXT,
    client_runtime_version TEXT,
    client_os             TEXT,
    client_arch           TEXT,
    client_sdk_name       TEXT,
    client_sdk_version    TEXT,
    client_retry_count    INTEGER,
    client_timeout_seconds INTEGER,
    client_user_name      TEXT,                    -- enrichment-plane
    client_department     TEXT,                    -- enrichment-plane

    -- Agent (noodle build identity)
    agent_version         TEXT     NOT NULL,
    agent_arch            TEXT     NOT NULL,
    agent_build_date      TEXT,
    agent_git_sha         TEXT,

    -- Rate limiting (summary)
    rate_limit_utilization     REAL,
    rate_limit_window_seconds  INTEGER,

    -- Business context
    context_json          TEXT,                    -- JSON object

    -- Provider-verbatim bag
    provider_metadata_json TEXT,                   -- JSON object, vendor-shaped

    -- Processor bookkeeping
    processor_emitted_at  INTEGER  NOT NULL,       -- when this row was written
    processor_version     TEXT     NOT NULL,       -- noodle-embellish version
    shipped_at            INTEGER                  -- null until shipper marks it
);

CREATE INDEX idx_timestamp     ON ai_telemetry_v_0_0_2 (timestamp);
CREATE INDEX idx_session_id    ON ai_telemetry_v_0_0_2 (session_id);
CREATE INDEX idx_api_key       ON ai_telemetry_v_0_0_2 (api_key_prefix);
CREATE INDEX idx_unshipped     ON ai_telemetry_v_0_0_2 (shipped_at) WHERE shipped_at IS NULL;
```

### 3.2 JSON columns vs flattened

`provider_metadata` is a verbatim provider wire shape and grows
per Anthropic / OpenAI / Google additions. Flattening into
columns would force a schema migration on every wire-shape change
upstream, defeating the telemetry backend's "pass-through" design. JSON
columns keep upstream changes free.

SQLite has native JSON operators (`json_extract`,
`json_array_length`); shippers query JSON columns directly
without parsing the whole document.

`context` and `endpoint_params` are similarly JSON columns —
free-form key-value bags that don't warrant their own tables.

### 3.3 Multiple-target databases

When the processor is configured for multiple targets, each target
gets its own table in the same SQLite file. Tables are independent.
A shipper for `ai-telemetry` v0.0.2 reads `ai_telemetry_v_0_0_2`; a
shipper for OTLP reads `otlp_spans` (or wherever the OTLP mapping
lands).

### 3.4 Shipped-watermark column

`shipped_at` is `NULL` until a shipper marks the row sent. Shippers
update this with their own batch-completion timestamp. The
processor never reads `shipped_at`; it's purely shipper-side
bookkeeping. The partial index `idx_unshipped` makes the "find
unsent rows" query fast even for large databases.

A retention policy (delete rows where `shipped_at < now - 30d`) is
operator-configurable; the processor implements deletion when
configured, otherwise leaves rows in place.

---

## 4. Mapping function shape

Per-target mapping is a function from a paired noodle record set
to a target-schema row:

```rust
pub trait TargetMapping: Send + Sync {
    /// Target schema identifier — matches the SQLite table name.
    fn target(&self) -> &'static str;

    /// Process one request+response pair into a target-schema row.
    /// Returns None if the pair should not produce a row (e.g.,
    /// non-API-call records, filtered traffic).
    fn map(
        &self,
        request: &TapRecord,
        response: &TapRecord,
        envelope: &Envelope,
    ) -> Option<TargetRow>;
}
```

Mappings are registered at processor startup. Each registered
mapping receives every paired record set; mappings that don't
recognise the record return `None`. Registration is configuration,
not code — operators select which targets to run.

### 4.1 Partial-event mapping

When the response is missing (per §2.2 timeout), the processor
calls `map` with a synthesised response carrying
`status_code = 0`, `error_type = "no_response_observed"`, and
empty body. The mapping can decide whether to emit a partial row.
The reference `ai-telemetry` mapping emits the row with sentinel
values; alternative mappings may skip.

---

## 5. `ai-telemetry` v0.0.2 mapping

The reference mapping. Every the telemetry schema field has one
entry: source on the noodle side → transform → SQLite column.

| telemetry field | Source | Transform | SQLite column |
|---|---|---|---|
| `event_id` | minted | ULID at row write time | `event_id` |
| `schema_id` | constant | `"ai-telemetry"` | `schema_id` |
| `schema_version` | constant | `"0.0.2"` | `schema_version` |
| `event_type` | constant | `"api_call"` | `event_type` |
| `timestamp` | request `ts_unix_ms` | identity | `timestamp` |
| `request_id` | request `headers[X-Client-Request-Id]` | header lookup | `request_id` |
| `provider` | `envelope.provider` (ADR 025 §3.7) | enum to string | `provider` |
| `model` | request `body_in.model` | JSON field extraction | `model` |
| `endpoint_path` | `endpoint` (ADR 027) | identity | `endpoint_path` |
| `endpoint_params` | `endpoint` query string | parse | `endpoint_params_json` |
| `streaming` | response `headers[Content-Type]` | `== "text/event-stream"` | `streaming` |
| `status_code` | response status | identity | `status_code` |
| `error_type` | decoded `events[type=error]` (ADR 030 §3.2) | `error.type` or null | `error_type` |
| `latency_ms` | response `ts_unix_ms` − request `ts_unix_ms` | subtraction | `latency_ms` |
| `input_tokens` | `usage.tokens.input` (ADR 029 §2.4) | identity | `input_tokens` |
| `output_tokens` | `usage.tokens.output` | identity | `output_tokens` |
| `estimated_cost_usd` | (downstream) | null placeholder | `estimated_cost_usd` |
| `cost_model_version` | (downstream) | null placeholder | `cost_model_version` |
| `api_key_prefix` | `envelope.subscription.api_key.prefix` (ADR 029 §2.4) | identity | `api_key_prefix` |
| `api_key_type` | `envelope.subscription.api_key.kind` | enum to string | `api_key_type` |
| `user_id` | (enrichment plane) | null placeholder | `user_id` |
| `session_id` | `envelope.session_id` (ADR 028) | identity | `session_id` |
| `session_hash` | `envelope.session_id` | SHA-256 truncated to 12 chars | `session_hash` |
| `client_user_agent` | request `headers[User-Agent]` | identity | `client_user_agent` |
| `client_username` | (enrichment plane) | null placeholder | `client_username` |
| `client_hostname` | `envelope.machine.hostname` (ADR 029 §2.4) | identity | `client_hostname` |
| `client_app` | request `headers[X-App]` | identity | `client_app` |
| `client_lang` | request `headers[X-Stainless-Lang]` | identity | `client_lang` |
| `client_runtime` | request `headers[X-Stainless-Runtime]` | identity | `client_runtime` |
| `client_runtime_version` | request `headers[X-Stainless-Runtime-Version]` | identity | `client_runtime_version` |
| `client_os` | request `headers[X-Stainless-Os]` or `envelope.machine.os_family` | header > envelope fallback | `client_os` |
| `client_arch` | request `headers[X-Stainless-Arch]` or `envelope.machine.architecture` | header > envelope fallback | `client_arch` |
| `client_sdk_name` | `"stainless"` when any `X-Stainless-*` header is present | constant when condition met | `client_sdk_name` |
| `client_sdk_version` | request `headers[X-Stainless-Package-Version]` | identity | `client_sdk_version` |
| `client_retry_count` | request `headers[X-Stainless-Retry-Count]` | parse integer | `client_retry_count` |
| `client_timeout_seconds` | request `headers[X-Stainless-Timeout]` | parse integer | `client_timeout_seconds` |
| `client_user_name` | (enrichment plane) | null placeholder | `client_user_name` |
| `client_department` | (enrichment plane) | null placeholder | `client_department` |
| `agent.version` | `envelope.collector_app.version` (ADR 029 §2.4) | identity | `agent_version` |
| `agent.arch` | `envelope.collector_app` (architecture inferred from build) | identity | `agent_arch` |
| `agent.build_date` | `envelope.collector_app.build_date` | ISO-8601 string | `agent_build_date` |
| `agent.git_sha` | `envelope.collector_app.build_hash` | identity | `agent_git_sha` |
| `rate_limit_utilization` | response `headers[Anthropic-Ratelimit-Unified-Utilization]` (or computed) | parse float | `rate_limit_utilization` |
| `rate_limit_window_seconds` | response `headers[Anthropic-Ratelimit-Unified-Reset]` | parse window | `rate_limit_window_seconds` |
| `context.*` | (enrichment plane / detector output) | JSON object | `context_json` |
| `provider_metadata.provider` | `envelope.provider` | identity | inside `provider_metadata_json` |
| `provider_metadata.request_id` | response `headers[Request-Id]` | identity | inside `provider_metadata_json` |
| `provider_metadata.usage` | decoded `events[type=message_delta].usage` + ADR 029 `TokenUsage` | structure to JSON | inside `provider_metadata_json` |
| `provider_metadata.usage.api_key_prefix` | `envelope.subscription.api_key.prefix` (duplicated) | identity | inside `provider_metadata_json` |
| `provider_metadata.usage.api_key_type` | `envelope.subscription.api_key.kind` (duplicated) | enum to string | inside `provider_metadata_json` |
| `provider_metadata.stop_reason` | decoded `events[type=message_delta].delta.stop_reason` or `turn_end_reason` | identity | inside `provider_metadata_json` |
| `provider_metadata.stop_details` | decoded `events[type=message_delta].delta.stop_details` | identity | inside `provider_metadata_json` |
| `provider_metadata.context_management` | response body `context_management` field | identity | inside `provider_metadata_json` |
| `provider_metadata.rate_limit` | response `headers[Anthropic-Ratelimit-Unified-*]` | structure 9 headers into object | inside `provider_metadata_json` |
| `provider_metadata.organization_id` | `envelope.subscription.organization.organization_id` (ADR 029 §2.4) | identity | inside `provider_metadata_json` |
| `provider_metadata.parent_organization_id` | `envelope.subscription.organization.parent_organization_id` | identity | inside `provider_metadata_json` |
| `provider_metadata.organization_type` | `envelope.subscription.organization.account_type` | enum to string | inside `provider_metadata_json` |
| `provider_metadata.session_key_prefix` | `envelope.subscription.api_key.prefix` (same value as `api_key_prefix`) | identity | inside `provider_metadata_json` |
| `provider_metadata.beta_features` | request `headers[Anthropic-Beta]` | parse comma-separated list | inside `provider_metadata_json` |

### 5.1 Enrichment-plane placeholders

Five fields are deliberately `null` from this processor:

- `user_id` — requires Console API resolution by `api_key_prefix`.
- `client_username` — requires provider email > git email > OS
  username chain.
- `client_user_name`, `client_department` — require MDM /
  Settings UI provisioning.
- `estimated_cost_usd`, `cost_model_version` — require a pricing
  table.

The shipper (or a separate enrichment pass downstream of this
processor) fills these in. The processor leaves the columns
nullable so enrichment is non-destructive.

### 5.2 Mapping versioning

The mapping itself is versioned independently from the target
schema. `ai-telemetry` v0.0.2 → SQLite is mapping version 1; if
the telemetry backend ships v0.0.3 and the mapping changes, mapping version
2 ships alongside. The processor records the mapping version in
`processor_version` so consumers can detect mapping drift.

---

## 6. Configuration

The processor reads a TOML config at startup. Skeleton:

```toml
[source]
kind = "file_tail"                       # file_tail | file_read | tcp
path = "/var/log/noodle/tap.jsonl"

[buffer]
pair_timeout_seconds = 300
max_buffer_mb        = 64

[output]
sqlite_path = "/var/lib/noodle-embellish/events.sqlite"

[[targets]]
name    = "ai_telemetry_v_0_0_2"
enabled = true

# Multiple targets can be enabled simultaneously:
# [[targets]]
# name    = "otlp_spans"
# enabled = true

[retention]
delete_shipped_after_days = 30           # 0 disables deletion
```

A single processor binary handles multiple targets concurrently;
each enabled target gets its own SQLite table (§3.3).

---

## 7. Failure modes

| Failure | Behaviour | Audit trail |
|---|---|---|
| SQLite locked by another writer | Processor backs off (exponential, max 30s) then retries. After 60s of continuous lock, emits operational alert and continues attempting. | Lock-wait events logged. |
| Disk full | Processor pauses ingestion (stops reading from `WireSource`), emits operational alert, retries every 30s. Resumes when disk frees. No records lost — they remain in `tap.jsonl` until processor catches up. | Disk-full event logged with `WireSource` offset at pause. |
| Mapping error (e.g., malformed `provider_metadata` JSON) | Processor logs the error, writes a partial row with `error_type = "mapping_error"`, continues. | Row carries error detail in `context_json`. |
| Schema migration needed (target schema bumped) | Processor refuses to start. Operator runs the migration tool, then restarts. | Pre-startup version check. |
| Patch event arrives for a record already emitted | Processor updates the affected SQLite row with the patch. `processor_emitted_at` reflects the last update. | Patch-applied event logged. |
| `WireSource` reset (file truncated, TCP reconnect) | Processor logs the reset, resumes from the new starting offset. Records before the reset that weren't emitted are lost. | Reset event logged with timestamp and `WireSource` identity. |

---

## 8. Open questions

1. **Partial events for never-paired records.** A request with no
   response (proxy killed mid-flow) emits a partial event after
   the timeout. Whether the shipper should send these or treat
   them as data-quality noise is shipper-side policy. The
   processor emits; the shipper decides.

2. **Schema migration tool.** Bumping the target schema (e.g.,
   `ai-telemetry` v0.0.3) requires adding columns or new tables.
   A standalone migration binary is implied but not specified
   here. Deferred until a real schema bump arrives.

3. **Cross-target deduplication.** Two enabled targets receiving
   the same record produce two rows in two tables. Whether to
   deduplicate at the processor (skip emitting to target B if
   the record already mapped to target A successfully) or leave
   it to shippers is open. Default: no deduplication; each
   target gets every applicable record.

4. **Streaming-only consumers.** A future consumer might prefer
   to receive events as they're written rather than poll SQLite.
   A `WireSink` adapter that re-emits processor output to a
   downstream `WireSink` (chaining sinks) is straightforward but
   not specified here. Deferred until a real streaming consumer
   surfaces.

5. **In-process embellishment.** Operators with simple needs may
   want the processor embedded in the proxy as a feature flag,
   skipping the separate binary entirely. This violates ADR 001
   principle 7 but reduces operational complexity. Not
   specified; revisit if a real deployment forces the trade-off.

6. **Shipper handoff contract.** This ADR ends at the SQLite
   file. A separate ADR specifying the shipper's read-side
   contract (which columns are guaranteed populated, how to mark
   rows as shipped, retention semantics) is the natural next
   document if a the telemetry backend (or other) shipper integration is
   scoped.
