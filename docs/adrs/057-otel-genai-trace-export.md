# ADR 057 — OTel GenAI trace export: turn-grouped hierarchical spans

**Status:** Proposed — builds directly on the existing shipper/OTLP substrate
(ADR 042–046) and the GenAI mapping in [ADR 052 §10](052-turn-run-lineage-frame-tree.md).

## 1. Context

The shipper already exports OTLP over HTTP/JSON to an external collector
(ADR 043/044): logs to `/v1/logs` and spans to `/v1/traces`
(`noodle-shipper` `mapping::row_to_otlp_log` / `row_to_otlp_span`). The §6 marks
(`turn_id`, `role`, `frame_id`, `parent_frame_id`, `depth`) ride on every
`RollupsRow`, the SQLite `ai_telemetry_v_0_0_2` table carries them, and the
`otel_genai` module (ADR 052 §10) already builds `chat`/`invoke_agent` spans and
`assemble_trace`.

But `row_to_otlp_span` emits **one flat span per round-trip row**: `traceId` is
hashed from `session_hash`, `parentSpanId` is omitted ("siblings in v1"), and
`assemble_trace` is never called. A backend therefore sees a flat list of chat
spans, not the `session → turn → frame → round-trip` tree — so TraceQL cannot
navigate the agent hierarchy or roll cost up by turn.

This ADR decides how the export path assembles the GenAI trace tree, and how we
view it (Grafana/Tempo/TraceQL). The substrate is ~90% there; this is grouping +
hierarchy + a dev viewing harness.

## 2. Decision

1. **Trace = turn.** `trace_id` is minted from `turn_id` (16-byte hash), not
   `session_hash` — one trace per turn. `session.id` is a span attribute that
   spans traces (§10), not the trace id.
2. **Hierarchy.** The exporter groups claimed rows by `turn_id` and calls
   `otel_genai::assemble_trace`:
   - one `invoke_agent` span per distinct `frame_id` — `span_id = hash(frame_id)`;
     root frame (`depth 0`) has no parent; a sub-agent frame sets
     `parentSpanId = hash(parent_frame_id)`.
   - one `chat` span per round-trip — `span_id = hash(event_id)`,
     `parentSpanId = hash(frame_id)`.
3. **Span timing.** A `chat` span is `[row.timestamp, +latency_ms]` (as today).
   An `invoke_agent` span is `[min(child.start), max(child.end)]`, derived over
   the turn's grouped rows.
4. **Content-free.** Spans carry ids, `gen_ai.*` attributes, and counts only —
   no prompt/response text (§5, ADR 003).
5. **Viewing (dev harness).** A dev-only `docker/otel-genai/` compose
   (otel-collector → Tempo → Grafana, provisioned Tempo datasource) for TraceQL
   inspection. This is a **dev/test** deployment; the production collector stays
   the separate repo of ADR 044.
6. **Transport unchanged.** HTTP/JSON to `/v1/traces` (ADR 043); the gRPC option
   (045) and auth headers (046) compose unchanged.

## 3. What changes

| Concern | Today (`row_to_otlp_span`) | This ADR |
|---|---|---|
| `trace_id` | `hash(session_hash)` | `hash(turn_id)` — one trace per turn |
| span model | flat siblings | tree: agent frames + chat leaves |
| `parentSpanId` | omitted | chat→frame, sub-frame→parent_frame |
| `invoke_agent` spans | none | one per distinct `frame_id` per turn |
| exporter input | per row | rows grouped by `turn_id` |
| `assemble_trace` | unused | called per turn group |

## 4. Collector routing (extends ADR 044)

The collector accepts OTLP `/v1/traces` and routes to a trace store (Tempo in the
dev harness). The **client** assembles the full tree — the collector does no
re-parenting. `trace_id` is turn-unique (turn ids are ULIDs), so there is no
cross-turn `trace_id` collision (the bug latent in the old `session_hash`-derived
id when one session has many turns).

## 5. Security

Content-free spans (ids, `gen_ai.*`, counts). Auth headers per ADR 046;
cost-rate-card and any redaction happen at the collector per ADR 044. The dev
harness binds localhost only and runs without auth (dev posture only).

## 6. Test approach

- **Unit:** id minting (`turn_id`/`frame_id`/`event_id` → trace/span hex) and the
  turn-grouped span builder.
- **Integration:** the wiremock OTLP receiver (ADR 043) asserts, over the
  committed `analysis/claude-parallel-subagents` capture, exactly one `traceId`,
  4 `invoke_agent` spans + 12 `chat` spans, the correct `parentSpanId` tree, and
  `gen_ai.*`/`session.id` attributes.
- **End-to-end:** the dev harness (story 061) renders that trace in TraceQL.

## 7. Alternatives

- **Flat sibling spans (status quo):** simplest, but no hierarchy — TraceQL can't
  navigate frames or attribute sub-agent cost. Rejected; it's the bug.
- **Native `opentelemetry-otlp` SDK:** heavier dependency; the hand-rolled
  HTTP/JSON path already works (043). Defer unless the gRPC option (045) forces a
  proto crate.

## 8. Boundaries

- **Builds on** ADR 052 §10 (`otel_genai` mapping), 042 (telemetry schema), 043
  (shipper handoff + HTTP/JSON OTLP), 044 (external prod collector), 046 (auth).
- **Out of scope:** server-side §6 deep nesting (the depth-1 cap stands); the
  OODA viewer (story 059); the production collector deployment (ADR 044's repo).
