# 010 — Audit log and structured tracing

## Value

Every injection and every redaction emits a structured audit record
to a sink that downstream systems can consume. The proxy can prove
exactly what it did to which session, and at what time. Operators
can answer the question "did noodle redact a marker for session X?"
without reading byte-level logs.

## Acceptance criteria

- An `AuditSink` trait in `noodle-core` with one method:
  `fn record(&self, event: AuditEvent)`.
- Two implementations:
  - `JsonLinesSink` — writes one JSON object per line to a configured
    path. Atomic-rename rotation daily or by size.
  - `TracingSink` — emits via `tracing::event!` so existing
    OpenTelemetry pipelines pick it up.
- `AuditEvent` variants:
  - `Inject { session_id_prefix, adapter, ts }`
  - `TurnStart { session_id_prefix, turn_id, ts }`
  - `Redact { session_id_prefix, turn_id, marker, raw_bytes, ts }`
  - `TurnEnd { session_id_prefix, turn_id, finish, ts }`
- `session_id_prefix` is the first 8 hex chars of the SHA-256 — never
  the full key. This bounds correlation risk if logs leak.
- A configurable redaction list scrubs `Authorization`, `X-Api-Key`,
  and any user-named header from request-trace events.
- Documentation in `docs/guides/audit.md` covers log format,
  rotation, and how to query for "did noodle act on session X."

## Dependencies

- 005 (sessions exist).
- 006 (redactions exist).

## Implementation notes

- Sink writes happen from a dedicated background task fed by a bounded
  MPSC channel. Inspection-path code must never block on disk.
- If the audit channel is full, drop the audit record and increment a
  counter — never block the proxy on audit. Document this trade-off
  loudly. (Alternative — backpressure into the proxy — is unacceptable
  in v1; a misconfigured disk should not stall traffic.)
- Don't try to be a SIEM. JSON lines is enough. Downstream systems
  can ship to S3, Loki, Splunk, or whatever.
