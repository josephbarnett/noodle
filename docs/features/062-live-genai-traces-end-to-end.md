# 062 — Live GenAI traces end-to-end (proxy → Tempo → TraceQL)

Drive real Claude Code / OpenCode sessions through the proxy and see them as
GenAI traces in Grafana TraceQL, closing the loop from
[ADR 057](../adrs/057-otel-genai-trace-export.md): new-detector proxy →
embellish (SQLite `ai_telemetry` rows with marks) → shipper traces → dev
collector → Tempo → Grafana.

## Value

The product's actual output — a live agent session — appears as a navigable trace
tree (turns, nested sub-agent frames, per-span token usage) in an OTel-native
backend. This is the end-state proof that ADR 052 + 057 deliver portable GenAI
telemetry, not just internal marks.

## Acceptance criteria

- Production proxy (new detector) → embellish writes rows with
  `turn_id`/`role`/`frame_id`/`parent_frame_id`/`depth` populated → shipper
  exports traces to the dev collector (`NOODLE_OTLP_ENDPOINT`).
- A live session that spans ≥3 prompts and spawns a Task sub-agent renders in
  TraceQL as ≥3 turn-traces, each with its `invoke_agent` frames and `chat`
  leaves; sub-agent frames nest under the spawning turn; side-calls do **not**
  appear as spans.
- Per-turn cost is recoverable in TraceQL (sum `gen_ai.usage.*` by `trace_id`).
- Fail-before/pass-after captured.

## Dependencies

- 060 (hierarchical spans), 061 (dev harness), 059 (proxy/marks verified live).

## Implementation notes

- Point `NOODLE_OTLP_ENDPOINT` at the story-061 collector (default
  `http://127.0.0.1:4318`); run the shipper against the embellish SQLite the
  proxy populates — `~/.noodle/rollups.db` (`NOODLE_ROLLUPS_DB`, WAL; embellish
  writes, shipper reads the same path).
- Confirm the embellish mapping carries the marks from `tap.jsonl` → `TelemetryRow`
  → the `ai_telemetry_v_0_0_2` lineage columns (already present post `#9`).
- This is the same path the production collector (ADR 044) will serve; only the
  collector deployment differs (dev compose vs the separate prod repo).

## Out of scope

- Production collector deployment (ADR 044's repo), cost-rate-card, sampling,
  redaction — those live at the collector, not here.
