# 061 — Dev observability harness + offline trace emitter

Stand up a local `otel-collector → Tempo → Grafana` stack and an offline emitter
so a real reconstructed GenAI trace is viewable in TraceQL **without** a live
proxy+Claude run — the fastest verify-against-real-product for
[ADR 057](../adrs/057-otel-genai-trace-export.md). Dev-only; the production
collector stays separate per ADR 044.

## Value

One command, and you open Grafana and run TraceQL against a real trace
reconstructed from a committed capture — proving the whole
`correlate → assemble_trace → OTLP → collector → Tempo → Grafana` path end to end
before any production wiring.

## Acceptance criteria

- `docker/otel-genai/docker-compose.yml`: otel-collector + Tempo + Grafana, with
  a provisioned Tempo datasource and a collector config that receives OTLP/HTTP
  and exports to Tempo. Binds localhost; no auth (dev).
- An offline emitter (a `cargo run` bin or tool) reads
  `analysis/claude-parallel-subagents` → correlates → `assemble_trace` → POSTs
  OTLP to the collector `/v1/traces`.
- In Grafana, TraceQL `{ .gen_ai.operation.name = "invoke_agent" }` returns the
  turn trace; drilling in shows 4 agent spans with 12 nested chat spans and
  `gen_ai.usage.*` on the leaves.
- A runbook in `docs/operations/` documents bring-up, the TraceQL queries, and
  teardown.

## Dependencies

- 060 (the hierarchical span builder the emitter reuses).

## Implementation notes

- Collector config: `otlp` receiver (http) → `otlp`/`tempo` exporter.
- Grafana: provision the Tempo datasource via `grafana/provisioning/datasources`.
- The emitter reuses the §6 correlation already proven in
  `scripts/tap_correlate.py` (port the relevant bit to the Rust bin, or shell out)
  + the story-060 span builder.
- Keep the corpus committed (no new capture data); the harness reads what `#9`
  already shipped.
