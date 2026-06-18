# 060 — Turn-grouped hierarchical GenAI trace spans in the shipper

Make the shipper emit the `turn → frame → chat` trace tree instead of flat
sibling spans, per [ADR 057](../adrs/057-otel-genai-trace-export.md). The
substrate exists (`row_to_otlp_span`, deterministic id minting, `RollupsRow`
marks, `otel_genai::assemble_trace`) — `assemble_trace` is just never called and
spans carry no `parentSpanId`.

## Value

A collector/Tempo receives a navigable trace per turn — agent frames with nested
chat calls and per-span usage — so TraceQL can attribute cost by turn and frame
instead of seeing an undifferentiated list of chat spans.

## Acceptance criteria

- The exporter groups claimed `RollupsRow`s by `turn_id`; each group is passed
  through `otel_genai::assemble_trace`.
- `trace_id = hash(turn_id)` (16 bytes); `chat` `span_id = hash(event_id)`;
  `invoke_agent` `span_id = hash(frame_id)`.
- `parentSpanId` emitted: chat → its frame; sub-agent frame → `parent_frame_id`;
  root frame (depth 0) → none.
- One `invoke_agent` span per distinct `frame_id` in the turn; one `chat` span
  per row.
- `invoke_agent` span start/end = min/max over the turn's child rows.
- `gen_ai.*` + `session.id` attributes on the spans (ADR 052 §10).
- Wiremock OTLP receiver test (ADR 043) over the committed
  `analysis/claude-parallel-subagents` capture asserts: 1 `traceId`, 4
  `invoke_agent` + 12 `chat` spans, correct parent tree.
- `row_to_otlp_log` path unchanged (logs still ship).

## Dependencies

- `#9` (marks on the row), ADR 052 §10 (`otel_genai`), 042/043 (shipper + OTLP).

## Implementation notes

- Add §10(b) id minting to `otel_genai` (`turn_id`/`frame_id`/`event_id` → 16/8-byte
  hex), with `GenAiSpan` already in place.
- In `mapping.rs`, replace the per-row span path with a turn-batch builder; in
  `exporter.rs`, `build_resource_spans_payload` groups the claimed batch by
  `turn_id` before building spans. Rows with no `turn_id` (side-calls) stay
  off-tree — emit as a log only, never a span.
- Keep HTTP/JSON; no new proto dep (ADR 057 §7).
