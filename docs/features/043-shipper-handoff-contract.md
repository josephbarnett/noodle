# 043 — OTel shipper: rollups SQLite → OTLP → collector

**Status:** Shipped (PR #95)
**Depends on:** [042](042-ai-telemetry-v0-0-2-mapping.md)
**Cadence:** [`docs/adrs/036`](../adrs/036-macos-collector-parity-value-cadence.md)
**Design refs:**
[`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md) §2 (file boundary, separate-process shipper),
[`docs/adrs/031-embellishment-processor.md`](../adrs/031-embellishment-processor.md) §"hand off" (the SQLite-cursor contract this slice pins).
**External reference:** `(external reference removed)/` (existing the telemetry backend shipper — the reference implementation of the cursor-on-flag pattern this slice mirrors).
**System diagram:** [`docs/diagrams/system-architecture.drawio`](../diagrams/system-architecture.drawio) — the OTel Shipper component sits between **Telemetry / Aggregated Rollups** (SQLite) and **OTel Collector API** (external).

---

## 1. Value delivered

After this slice ships, noodle owns the **shipper component** drawn in `system-architecture.drawio` — a separate-process executable that polls `noodle-embellish`'s rollups SQLite (`delivery_status='pending'` cursor per E5 (removed)), serialises each row to OTLP (logs + spans matching the `ai-telemetry` v0.0.2 record shape), pushes to a configurable OTel collector endpoint, and updates the row's `delivery_status` on ack. This is the rust replacement for the macOS the telemetry backend shipper — same database contract, same at-least-once semantics, but emitting OTLP rather than the macOS shipper's proprietary protocol.

The shipper is **out-of-proxy** by design (ADR 022 §2 / §3): proxy stays lean and crash-safe; rollups DB is the durable buffer; shipper survives collector outages without dropping telemetry; rollups survive shipper outages without dropping telemetry.

## 2. Acceptance criteria

1. New crate `noodle-shipper` in the noodle workspace; binary produces OTLP from rollups SQLite. The shipper is **not** wired into the proxy process — it's a separate executable per ADR 022 §2 ("the collector is a separate process — not bundled").
2. Polls the rollups SQLite at the path `noodle-embellish` wrote to (default mirrors macOS shipper: `~/.noodle/rollups.db`, configurable via `NOODLE_ROLLUPS_DB`). Cursor: `SELECT * FROM telemetry_events WHERE delivery_status = 'pending' ORDER BY event_id LIMIT N` per E5 (removed) §A.
3. Each row maps to one OTLP record: `ai-telemetry` event → OTLP Log record (or Span where the schema's `kind` warrants); attributes carry the [040.a](040.a-side-effect-correlation-block.md) correlation block at resource + record scope per [E4](E4-wiremock-otlp-receiver-spike.md) §B placement strategy. **Wire-format caveats from [E4](E4-wiremock-otlp-receiver-spike.md) §B**: `flow_id` as `stringValue` (not `intValue` — `u64` overflows OTLP signed int64); `AuditEvent.detail` as stringified JSON for v1; sink-minted deterministic `trace_id` / `span_id` from `(session_id, flow_id)` so retries are idempotent.
4. At-least-once delivery: a row moves through `pending → in_flight → delivered | failed → retry → poison` per E5's existing macOS shipper state machine. Crashed shipper restart re-consumes any `in_flight` rows without dropping or duplicating downstream.
5. Configurable OTel collector endpoint via `NOODLE_OTLP_ENDPOINT` env var; default off (consistent with `NOODLE_LAYERED_CORE` pattern).
6. Drop-on-full posture: if the collector is unreachable, rows stay `pending` (not dropped). Backpressure surfaces as growing `pending` count; ops alerting on `delivery_status='pending' count > N` is the operator's monitoring contract.
7. **Live collector round-trip** against a locally-running `opentelemetry-collector-contrib` (or upstream `otelcol`) confirms each row type is accepted at OTLP/HTTP `/v1/logs` and `/v1/traces` endpoints. This is the round-trip [E4](E4-wiremock-otlp-receiver-spike.md) deferred — collector binary was not on PATH and Docker daemon was down. The first thing 043 must do is run E4 end-to-end with the prepared payloads in [E4](E4-wiremock-otlp-receiver-spike.md) §B.
8. `docs/guides/shipper-runbook.md` covers: starting the shipper, the cursor contract, the at-least-once / idempotency semantics, monitoring `pending` count, the `retry` / `poison` transitions, recovery from a wedged shipper.
9. End-to-end smoke test: real `claude -p` → noodle proxy → tap.jsonl → noodle-embellish → rollups SQLite → noodle-shipper → wiremock OTLP receiver; assert one OTLP record per `/v1/messages` round trip with correlation + usage + attribution attributes intact.

## 3. Abstractions introduced or refined

- **`noodle-shipper` crate** (new). Binary entrypoint + library surface for testability.
- **`RollupsCursor`** (new, in `noodle-shipper::cursor`): wraps the SQLite cursor-on-flag protocol per E5 §A. Generic over the event row type so future schemas (v0.0.3, custom shapes) can reuse the same cursor.
- **`OtlpExporter`** (new, in `noodle-shipper::otlp`): batches `ai-telemetry` rows into OTLP requests via `opentelemetry-otlp`. Configurable transport (gRPC or HTTP) — pick what the target collector accepts; E4 §B suggests HTTP/JSON as the lowest-friction option.
- **`ai-telemetry::Event` → OTLP mapping** (new, internal to the shipper). The mapping table matches the macOS shipper's mapping for protocol parity: each `ai-telemetry` event-type maps to an OTLP Log or Span based on schema `kind`. Correlation block rides as resource-scope (`session_id`, `agent_run_id`) + record-scope (`event_id`, `turn_id`, `flow_id`) attributes per [E4](E4-wiremock-otlp-receiver-spike.md) §B.

DI seam: `OtlpExporter::spawn(endpoint, transport, capacity)` — transport (`Grpc` | `Http`) is an enum; tests substitute an in-process collector / wiremock receiver.

## 4. Patterns applied

- **Cursor-on-flag** — the canonical pattern the existing macOS shipper uses; E5 confirmed it as the contract noodle should adopt.
- **At-least-once with idempotent event_id** — primary-key `event_id` (ULID) on the rollups table ensures retries don't duplicate downstream.
- **Adapter** — translating the schema-stable `ai-telemetry` v0.0.2 shape to OTLP. Canonical use.

## 5. Test plan

- **Unit:** mapping table from §3 — for each `ai-telemetry` event type, assert the produced OTLP protobuf matches a golden fixture. Attribute presence + naming + correlation placement.
- **Unit:** cursor state-machine — `pending → in_flight → delivered` happy path + every failure transition + crash recovery (in_flight rows reset to pending on startup).
- **Integration:** wiremock OTLP receiver in-process; drive synthetic rollups rows through the shipper; assert receiver captures one record per row with correlation attributes intact.
- **Integration:** failure-injection — kill the wiremock receiver mid-stream; assert shipper retries; rows do not leave `pending` until ack.
- **E2E:** real `claude -p` → full chain → wiremock OTLP receiver; AC #9.

## 6. PR scope

One or two PRs depending on how clean the `noodle-shipper` crate boot is. New crate + cursor + OTLP exporter + mapping table + operations runbook + tests. Target: under 1500 reviewable lines (the OTLP protobuf surface + the shipper boot dominate).

## 7. Out of scope

- The OTel collector itself (ADR 022 §2 says it lives in a separate repo) — [044](044-otel-collector-separate-repo.md).
- Identity resolution / cost-rate-card / redaction / sampling — collector's job per ADR 022 §2 point 4.
- In-proxy OTLP emission — explicitly **not** the architecture (see [`docs/diagrams/system-architecture.drawio`](../diagrams/system-architecture.drawio) and ADR 022 §2 point 1 file boundary). Slice 041 was originally scoped as an in-proxy `OtlpSideEffectSink`; it has been **retired** in favor of this slice's out-of-proxy shipper.
- A second target schema (v0.0.3, OpenTelemetry-native semantic conventions, custom backend shape) — speculative; the mapper stays concrete to v0.0.2 until a second consumer arrives.
