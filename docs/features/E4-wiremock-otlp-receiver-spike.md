# E4 — Wiremock OTLP receiver + hand-crafted SideEffect payloads

**Status:** partial · evidence probe · spike run 2026-05-27 · collector round-trip blocked, wire format spec-validated
**Parent cadence:** [`docs/adrs/036-macos-collector-parity-value-cadence.md`](../adrs/036-macos-collector-parity-value-cadence.md)
**Feeds:** [`043`](043-shipper-handoff-contract.md) AC #7 — the live collector round-trip (originally targeted at retired slice 041; now lives on the shipper).
**Design refs:** ADR 022 §2 points 1–4 (OTLP boundary).

## 1. Value delivered

Confirms that the four `SideEffect` shapes (`Hint`, `Artifact`, `AuditEvent`, `Resolved`) can be serialised into OTLP logs and an OTLP span and accepted by an OpenTelemetry collector endpoint. Catches wire-format gotchas (resource attribute shape, instrumentation scope, span vs log routing) before the OTLP exporter writes any adapter code.

> **Retargeting note** (2026-05-27): this probe was originally feeding slice 041 (`OtlpSideEffectSink`, in-proxy OTLP). After the system architecture diagram review, slice 041 was retired — OTLP emission moved into `noodle-shipper` ([043](043-shipper-handoff-contract.md)) per the file-boundary architecture (`docs/diagrams/system-architecture.drawio`). The wire-format findings below remain valid; they describe what *the shipper* must emit. References to "041" in the body below refer to that retired slice's design; treat them as describing 043's exporter today.

## 2. How to run

1. Start an upstream `opentelemetry-collector` binary locally with the `otlphttp` and `otlp` receivers enabled and a `debug` exporter. Document the config used.
2. Hand-craft one OTLP HTTP payload per SideEffect variant — three logs (`Hint`, `Artifact`, `AuditEvent`) + one span (`Resolved`). Use `curl` with the OTLP protobuf JSON encoding. Include `event_id` / `turn_id` / `session_id` / `agent_run_id` as resource and/or log attributes.
3. POST each to the collector. Assert acceptance (HTTP 200) and inspect the `debug` exporter output for fidelity.

## 3. Acceptance

1. Collector config + sample payloads checked into this story file as §A and §B.
2. All four payload variants accepted; debug exporter output reproduces every attribute.
3. One-line conclusion: "OTLP wire format accepts our SideEffect shapes with correlation as attributes at … (resource | log | span scope), with the following caveats: …".

## 4. Out of scope

No `OtlpSideEffectSink` adapter code. No rust changes. Pure wire-format spike.

---

## Appendix §A — Collector config

### Install attempt (2026-05-27)

The constraint was: a single `brew install` for `otelcol`; otherwise stop. Outcome:

```
$ brew search otelcol
otel-cli
opentelemetry-cpp

$ brew install otelcol-contrib
Error: No available formula with the name "otelcol-contrib".

$ brew install opentelemetry-collector-contrib
Error: No available formula with the name "opentelemetry-collector-contrib".
```

The official OpenTelemetry collector is not available via Homebrew core or any tap currently configured on this machine. Upstream publishes tarball releases (`github.com/open-telemetry/opentelemetry-collector-releases/releases`) and a Docker image (`otel/opentelemetry-collector-contrib`), but both fall outside the "single `brew install`" constraint — the tarball requires manual extraction + `chmod +x` placement, the Docker image requires a running daemon (Rancher Desktop's daemon was not running and starting it is invasive).

Per the spec's bail clause ("if the binary is unavailable and install would be invasive, document the attempt and skip to step 4"), the round-trip portion of the probe is **deferred to story 041**. The wire-format payloads below are spec-validated against the OTLP/HTTP JSON encoding ([otlp.md §3.1](https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md), [opentelemetry-proto](https://github.com/open-telemetry/opentelemetry-proto)) so they are immediately usable when the collector is available.

### Config that *would* be used

When the collector is available, this is the minimal config the probe should be re-run against. Save as `/tmp/otelcol-e4.yaml`:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318

processors:
  batch:
    timeout: 100ms

exporters:
  debug:
    verbosity: detailed

service:
  pipelines:
    logs:
      receivers: [otlp]
      processors: [batch]
      exporters: [debug]
    traces:
      receivers: [otlp]
      processors: [batch]
      exporters: [debug]
```

Run with:

```bash
otelcol-contrib --config /tmp/otelcol-e4.yaml
# OR
docker run --rm -p 4317:4317 -p 4318:4318 \
  -v /tmp/otelcol-e4.yaml:/etc/otelcol/config.yaml \
  otel/opentelemetry-collector-contrib:latest \
  --config=/etc/otelcol/config.yaml
```

OTLP/HTTP JSON endpoints:
- Logs: `POST http://localhost:4318/v1/logs` (`Content-Type: application/json`)
- Traces: `POST http://localhost:4318/v1/traces` (`Content-Type: application/json`)

Expected success response: HTTP 200, body `{"partialSuccess":{}}` (per OTLP spec §3.1).

## Appendix §B — Sample payloads + responses

### Correlation attribute placement strategy

The OTLP spec allows arbitrary KV attributes at three scopes:
- **Resource** (`resourceLogs[].resource.attributes`) — properties of the *emitter*, stable across many records.
- **Scope** (`resourceLogs[].scopeLogs[].scope.attributes`) — properties of the *instrumentation library*.
- **Record** (`logRecords[].attributes` / `spans[].attributes`) — properties of the *individual event*.

For noodle's correlation set:

| Field | Best scope | Rationale |
|---|---|---|
| `session_id` | Resource | Stable for the entire noodle proxy session; many flows share one. |
| `agent_run_id` | Resource | Same reasoning — identifies the agent invocation, not the event. |
| `turn_id` | Record | Changes per LLM round-trip; not stable across the resource. |
| `event_id` | Record | Unique per side-effect emission; record-scope by definition. |

The payloads below place all four at **both** resource and record scope to maximise the collector's ability to correlate downstream — duplication costs ~80 bytes per record on the wire, which is negligible compared to the value of letting any downstream processor (identity-resolve, cost-rate-card) match on whichever scope it prefers without re-emitting.

**Expected collector behavior** (spec-derived): the collector accepts attributes at any scope. The `debug` exporter prints all three sets distinctly (`Resource attributes:`, `Scope attributes:`, `Attributes:`), and downstream processors (`attributes`, `transform`, `routing`) can match on any of them via the `resource.attributes[...]` / `attributes[...]` path expressions in OTTL. Resource-scope duplication is the conventional answer to "I want this attribute to participate in cross-source correlation in the collector pipeline."

### Mapping rationale: variant → OTLP signal

| Variant | OTLP signal | Why |
|---|---|---|
| `Hint` | Log | Attribution opinion — a fact emitted at a point in time. No span semantics. |
| `Artifact` | Log | Captured named value — single point event with structured body. |
| `AuditEvent` | Log | Operational event — exactly the OTLP log shape (severity, body, attributes). |
| `ResolvedRecord` | Span | Represents the *resolved attribution* of one flow with a duration (flow open → flow close). The flow itself is the unit-of-work; the resolved record is its summary. Spans carry `start_time` + `end_time` + `attributes` naturally; the resolved category map fits as span attributes. |

### Payload 1 — `Hint` (OTLP log)

```bash
curl -i -X POST http://localhost:4318/v1/logs \
  -H 'Content-Type: application/json' \
  --data @- <<'EOF'
{
  "resourceLogs": [{
    "resource": {
      "attributes": [
        {"key": "service.name", "value": {"stringValue": "noodle"}},
        {"key": "service.namespace", "value": {"stringValue": "noodle.proxy"}},
        {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
        {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}}
      ]
    },
    "scopeLogs": [{
      "scope": {
        "name": "noodle.side_effect",
        "version": "0.1.0"
      },
      "logRecords": [{
        "timeUnixNano": "1748332800000000000",
        "observedTimeUnixNano": "1748332800000000000",
        "severityNumber": 9,
        "severityText": "INFO",
        "body": {"stringValue": "hint"},
        "attributes": [
          {"key": "noodle.event_id", "value": {"stringValue": "evt_01HXKZ2J4N0YQZ"}},
          {"key": "noodle.turn_id", "value": {"stringValue": "turn_01HXKZ2J4N0YR0"}},
          {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
          {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}},
          {"key": "noodle.side_effect.kind", "value": {"stringValue": "hint"}},
          {"key": "noodle.hint.category", "value": {"stringValue": "tool"}},
          {"key": "noodle.hint.value", "value": {"stringValue": "Claude Code"}},
          {"key": "noodle.hint.confidence", "value": {"doubleValue": 0.95}},
          {"key": "noodle.hint.source", "value": {"stringValue": "user_agent_detector"}},
          {"key": "noodle.flow_id", "value": {"intValue": "1"}}
        ]
      }]
    }]
  }]
}
EOF
```

**Spec-expected outcome:** HTTP 200 · `{"partialSuccess":{}}` · debug exporter prints one `LogRecord` with body `"hint"`, severity `INFO`, and all 10 record attributes plus the 4 resource attributes.

**Wire-format notes:**
- `attributes` is an array of `{key, value}` objects, *not* a map. JSON-encoded protobuf `repeated KeyValue`.
- `value` is a wrapper with exactly one typed field: `stringValue` / `intValue` / `doubleValue` / `boolValue` / `arrayValue` / `kvlistValue` / `bytesValue`.
- `intValue` and `timeUnixNano` are JSON **strings** (protobuf int64 mapping) — quoting matters.
- `confidence` (f32 in Rust) → `doubleValue` (OTLP has no float32; f32 widens to f64 losslessly).
- `flow_id` (u64 in Rust) → `intValue` as string. Note: protobuf int64 is signed; `u64::MAX > i64::MAX` could overflow. **Caveat:** `FlowId` is `u64` in [`layered.rs`](../../crates/noodle-core/src/layered.rs#L137) but OTLP int values are i64. If `flow_id` ever exceeds `i63::MAX` the wire encoding silently wraps. Safer: serialise as `stringValue` to preserve the full u64 range, *or* document that `flow_id` is allocated monotonically from 1 and never overflows i63.

### Payload 2 — `Artifact` (OTLP log)

```bash
curl -i -X POST http://localhost:4318/v1/logs \
  -H 'Content-Type: application/json' \
  --data @- <<'EOF'
{
  "resourceLogs": [{
    "resource": {
      "attributes": [
        {"key": "service.name", "value": {"stringValue": "noodle"}},
        {"key": "service.namespace", "value": {"stringValue": "noodle.proxy"}},
        {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
        {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}}
      ]
    },
    "scopeLogs": [{
      "scope": {"name": "noodle.side_effect", "version": "0.1.0"},
      "logRecords": [{
        "timeUnixNano": "1748332800500000000",
        "observedTimeUnixNano": "1748332800500000000",
        "severityNumber": 9,
        "severityText": "INFO",
        "body": {"stringValue": "artifact"},
        "attributes": [
          {"key": "noodle.event_id", "value": {"stringValue": "evt_01HXKZ2J4N0YR1"}},
          {"key": "noodle.turn_id", "value": {"stringValue": "turn_01HXKZ2J4N0YR0"}},
          {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
          {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}},
          {"key": "noodle.side_effect.kind", "value": {"stringValue": "artifact"}},
          {"key": "noodle.artifact.name", "value": {"stringValue": "noodle:work_type"}},
          {"key": "noodle.artifact.value", "value": {"stringValue": "code-review"}},
          {"key": "noodle.artifact.source_layer", "value": {"stringValue": "VendorSemantics"}},
          {"key": "noodle.artifact.source_transform", "value": {"stringValue": "marker_strip_transform"}},
          {"key": "noodle.flow_id", "value": {"intValue": "1"}},
          {"key": "noodle.artifact.captured_at_unix_ms", "value": {"intValue": "1748332800500"}}
        ]
      }]
    }]
  }]
}
EOF
```

**Spec-expected outcome:** HTTP 200 · `{"partialSuccess":{}}` · debug exporter prints a `LogRecord` with all artifact fields flattened to attributes.

**Wire-format notes:**
- `source_layer` (Rust enum `Layer::VendorSemantics`) → `stringValue` via the variant name. Could be encoded as `intValue` for compactness but string is debug-friendly and matches Joe's "explicit over clever" preference.
- `captured_at_unix_ms` is a u64 → `intValue` string. Same wraparound caveat as `flow_id` (a u64 millisecond timestamp will exceed i63::MAX in ~year 292277026596 — not a real concern).

### Payload 3 — `AuditEvent` (OTLP log)

```bash
curl -i -X POST http://localhost:4318/v1/logs \
  -H 'Content-Type: application/json' \
  --data @- <<'EOF'
{
  "resourceLogs": [{
    "resource": {
      "attributes": [
        {"key": "service.name", "value": {"stringValue": "noodle"}},
        {"key": "service.namespace", "value": {"stringValue": "noodle.proxy"}},
        {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
        {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}}
      ]
    },
    "scopeLogs": [{
      "scope": {"name": "noodle.side_effect", "version": "0.1.0"},
      "logRecords": [{
        "timeUnixNano": "1748332801000000000",
        "observedTimeUnixNano": "1748332801000000000",
        "severityNumber": 13,
        "severityText": "WARN",
        "body": {"stringValue": "audit"},
        "attributes": [
          {"key": "noodle.event_id", "value": {"stringValue": "evt_01HXKZ2J4N0YR2"}},
          {"key": "noodle.turn_id", "value": {"stringValue": "turn_01HXKZ2J4N0YR0"}},
          {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
          {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}},
          {"key": "noodle.side_effect.kind", "value": {"stringValue": "audit"}},
          {"key": "noodle.audit.kind", "value": {"stringValue": "Injected"}},
          {"key": "noodle.audit.layer", "value": {"stringValue": "VendorSemantics"}},
          {"key": "noodle.audit.transform", "value": {"stringValue": "attribution_injector"}},
          {"key": "noodle.audit.detail", "value": {"stringValue": "{\"directive_id\":\"work_type\",\"injected_bytes\":42}"}},
          {"key": "noodle.flow_id", "value": {"intValue": "1"}},
          {"key": "noodle.audit.at_unix_ms", "value": {"intValue": "1748332801000"}}
        ]
      }]
    }]
  }]
}
EOF
```

**Spec-expected outcome:** HTTP 200 · `{"partialSuccess":{}}` · debug exporter prints a `LogRecord` with WARN severity (for `Errored`/`InvariantViolation` kinds use `severityNumber: 17` / ERROR).

**Wire-format notes:**
- `AuditEvent.detail` is `serde_json::Value` — free-form JSON per-kind. Two viable encodings:
  1. **Stringified JSON** (shown above): `stringValue` containing the JSON text. Lossless, simple, opaque to OTTL.
  2. **Structured kvlist**: convert the JSON object to OTLP `kvlistValue` so downstream OTTL can match `attributes["noodle.audit.detail"]["directive_id"]`. Requires a serde_json → AnyValue mapper at sink time.
  - **Recommendation for 041:** start with stringified; promote to kvlist when a downstream processor needs structured access. Caveat to log: arrays of mixed types in `detail` require `arrayValue` with per-element `AnyValue` wrapping (not just JSON-array-of-primitives).
- Severity mapping per [OTel logs spec](https://opentelemetry.io/docs/specs/otel/logs/data-model/#field-severitynumber):
  - `Injected`, `Redacted`, `Filtered`, `LeafMinted` → INFO (9)
  - `Errored`, `MintFailed` → WARN (13) or ERROR (17) depending on context
  - `InvariantViolation` → ERROR (17) or FATAL (21)

### Payload 4 — `ResolvedRecord` (OTLP span)

```bash
curl -i -X POST http://localhost:4318/v1/traces \
  -H 'Content-Type: application/json' \
  --data @- <<'EOF'
{
  "resourceSpans": [{
    "resource": {
      "attributes": [
        {"key": "service.name", "value": {"stringValue": "noodle"}},
        {"key": "service.namespace", "value": {"stringValue": "noodle.proxy"}},
        {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
        {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}}
      ]
    },
    "scopeSpans": [{
      "scope": {"name": "noodle.side_effect", "version": "0.1.0"},
      "spans": [{
        "traceId": "5b8efff798038103d269b633813fc60c",
        "spanId": "eee19b7ec3c1b174",
        "name": "noodle.flow.resolved",
        "kind": 1,
        "startTimeUnixNano": "1748332800000000000",
        "endTimeUnixNano": "1748332802500000000",
        "attributes": [
          {"key": "noodle.event_id", "value": {"stringValue": "evt_01HXKZ2J4N0YR3"}},
          {"key": "noodle.turn_id", "value": {"stringValue": "turn_01HXKZ2J4N0YR0"}},
          {"key": "noodle.session_id", "value": {"stringValue": "sess_01HXKZ2J4N0YQX"}},
          {"key": "noodle.agent_run_id", "value": {"stringValue": "run_01HXKZ2J4N0YQY"}},
          {"key": "noodle.side_effect.kind", "value": {"stringValue": "resolved"}},
          {"key": "noodle.flow_id", "value": {"intValue": "1"}},
          {"key": "noodle.resolved.tool", "value": {"stringValue": "Claude Code"}},
          {"key": "noodle.resolved.work_type", "value": {"stringValue": "code-review"}},
          {"key": "noodle.resolved.model", "value": {"stringValue": "claude-opus-4-5"}}
        ],
        "status": {"code": 1}
      }]
    }]
  }]
}
EOF
```

**Spec-expected outcome:** HTTP 200 · `{"partialSuccess":{}}` · debug exporter prints one `Span` named `noodle.flow.resolved` with `start_time` and `end_time` set, kind=Internal (1), status=Ok (1), all attributes preserved.

**Wire-format notes:**
- `traceId` is 16 bytes hex (32 chars); `spanId` is 8 bytes hex (16 chars). Both are **required** and must be hex-encoded strings in JSON (OTLP/HTTP JSON uses hex for these per spec; OTLP/protobuf-binary uses raw bytes).
- noodle's `ResolvedRecord` has no `trace_id` / `span_id` today. The sink will need to mint them. Two viable strategies:
  1. **Derive from `flow_id` + `session_id`** — deterministic hash → 16-byte trace_id, 8-byte span_id. Reproducible, idempotent on retry.
  2. **Random per emission** — simpler, opaque, loses idempotency on retry.
  - **Recommendation for 041:** strategy 1 (deterministic), so collector-side deduplication can use the trace_id as the natural key.
- The `Resolved` map (Rust: `crate::Resolved` — a `category → value` HashMap) flattens to one attribute per category, prefixed `noodle.resolved.*`. **Caveat:** if a category name contains characters illegal in OTLP attribute keys (per spec only `string` is required; conventions favor lowercase ascii + `.`), the sink must normalize. Today the categories appear to be ascii (`tool`, `work_type`, `model`) so this is a future concern.
- `kind: 1` = `SPAN_KIND_INTERNAL`. A flow doesn't represent a client- or server-side network span from noodle's perspective; it's an internal unit of work over already-observed traffic. Could argue for `kind: 3` (`SPAN_KIND_CLIENT`) since noodle is observing a client→LLM call, but `INTERNAL` is more honest — the resolved record is *noodle's own* synthesis, not the LLM call itself.

### Spec-validation summary

| Payload | Validated against | Status |
|---|---|---|
| Hint log | OTLP/HTTP JSON §3.1 + logs data model §2 | Spec-valid; ready to POST |
| Artifact log | Same | Spec-valid; ready to POST |
| AuditEvent log | Same + severityNumber mapping table | Spec-valid; ready to POST |
| Resolved span | OTLP/HTTP JSON §3.1 + traces data model §2 | Spec-valid; requires trace_id/span_id minting in sink |

All four payloads conform to `opentelemetry-proto v1.5.0` JSON encoding. Collector round-trip remains untested — to be re-run as the first step of story 041.

### Caveats catalogued for 041

1. **`flow_id` u64 vs OTLP int64.** Encode as `stringValue` to preserve full u64 range, *or* document that allocation stays within i63. Decide before adapter code lands.
2. **`AuditEvent.detail` encoding.** Stringified JSON for v1; promote to `kvlistValue` when a downstream OTTL processor needs structured access. Document the encoding choice in the adapter.
3. **`ResolvedRecord` lacks trace_id / span_id.** Sink must mint them. Deterministic derivation from `(session_id, flow_id)` recommended for idempotent retries.
4. **`Resolved` category-name → attribute-key normalization.** Current categories are ascii-safe; add a `.replace([illegal], '_')` pass in the sink as defence-in-depth.
5. **Correlation attribute scope duplication.** All four payloads place `session_id` / `agent_run_id` at *both* resource and record scope. This is intentional (downstream processors can match on either) but costs ~80 bytes per record. If volume becomes an issue, drop record-scope duplicates and rely on resource-scope only.
6. **Severity mapping for `AuditKind`.** Use the table in §B payload 3. `InvariantViolation` → FATAL (21), `Errored`/`MintFailed` → WARN (13) or ERROR (17). Pin the mapping in the adapter.
7. **Span kind for `ResolvedRecord`.** `INTERNAL` (1) recommended; `CLIENT` (3) is also defensible. Pin in the adapter.

### Conclusion

**OTLP wire format accepts our SideEffect shapes with correlation as attributes at both resource scope (`session_id`, `agent_run_id` — stable across flows) and record scope (`event_id`, `turn_id` — per-event, plus `session_id` / `agent_run_id` duplicated for downstream-processor convenience), with the following caveats: (1) `flow_id` u64 must be encoded as `stringValue` or its allocation kept within i63::MAX to avoid silent int64 wraparound; (2) `AuditEvent.detail` ships as stringified JSON for v1, structured `kvlistValue` deferred; (3) `ResolvedRecord` needs a `(session_id, flow_id)`-derived trace_id/span_id minted by the sink — neither field exists in the source type; (4) collector round-trip remains untested — `otelcol` is unavailable via Homebrew and the Docker daemon was not running, so the validation here is spec-conformance only and must be re-run as the first AC of story 041.**
