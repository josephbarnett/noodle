# E1 — Live correlation-gap audit

**Status:** Shipped (PR #86) · evidence probe · 2h · first move
**Parent cadence:** [`docs/adrs/036-macos-collector-parity-value-cadence.md`](../adrs/036-macos-collector-parity-value-cadence.md)
**Feeds:** [`040.a`](040.a-side-effect-correlation-block.md) AC #1 baseline.

## 1. Value delivered

Hard numbers, not impression: per-variant counts of records in `~/.noodle/tap.jsonl` and `~/.noodle/side_effects.jsonl` that carry each of `event_id`, `turn_id`, `session_id` (full vs prefix), `agent_run_id`, and a non-zero `at_unix_ms`. Every downstream story's acceptance criteria is calibrated against these numbers.

## 2. How to run

```bash
# Coverage on side_effects.jsonl
jq -s '
  group_by(.kind)
  | map({
      kind: .[0].kind,
      total: length,
      with_event_id:        ([.[] | select(.event_id        != null)] | length),
      with_turn_id:         ([.[] | select(.turn_id         != null)] | length),
      with_session_id_full: ([.[] | select(.session_id      != null)] | length),
      with_session_prefix:  ([.[] | select(.session_prefix  != null)] | length),
      with_agent_run_id:    ([.[] | select(.agent_run_id    != null)] | length),
      with_nonzero_ts:      ([.[] | select((.at_unix_ms // .captured_at_unix_ms // 0) > 0)] | length)
    })
' ~/.noodle/side_effects.jsonl

# Coverage on tap.jsonl
jq -s '
  {
    total: length,
    with_event_id:    ([.[] | select(.event_id        != null)] | length),
    with_turn_id:     ([.[] | select(.marks.turn_id   != null)] | length),
    with_session_id:  ([.[] | select(.marks.session_id!= null)] | length),
    with_agent_run_id:([.[] | select(.marks.agent_run_id!= null)] | length)
  }
' ~/.noodle/tap.jsonl
```

## 3. Acceptance

1. Two markdown tables (one per file) committed to this story file as appendix §A.
2. Each `side_effects.jsonl` variant row shows its current ID coverage explicitly.
3. The `tap.jsonl` row shows current ID coverage on the `marks` block.
4. A one-line conclusion: "Of the four correlation IDs ADR 023 requires, N are emitted today on M% of records."

## 4. Output landing

Appendix §A on this file; copy of the conclusion line into [`040.a`](040.a-side-effect-correlation-block.md) AC #1's "baseline" annotation.

## 5. Out of scope

No code changes. No schema changes. No proposals. Just measurement.

---

## Appendix §A — Results (to be filled by probe run)

Capture snapshot: `~/.noodle/tap.jsonl` (39 records) and `~/.noodle/side_effects.jsonl` (23 records), both last written 2026-05-26 21:06.

### `side_effects.jsonl` — per-variant coverage

| kind     | total | event_id | turn_id | session_id (full) | session_prefix | agent_run_id | nonzero ts |
|----------|-------|----------|---------|-------------------|----------------|--------------|------------|
| artifact | 2     | 0        | 0       | 0                 | 0              | 0            | 0          |
| audit    | 5     | 0        | 0       | 0                 | 0              | 0            | 0          |
| hint     | 5     | 0        | 0       | 0                 | 0              | 0            | 0          |
| resolved | 11    | 0        | 0       | 0                 | 11             | 0            | 11         |
| **all**  | **23**| **0**    | **0**   | **0**             | **11 (48%)**   | **0**        | **11 (48%)**|

### `tap.jsonl` — marks-block coverage

| total | event_id     | marks.turn_id | marks.session_id | marks.agent_run_id |
|-------|--------------|---------------|------------------|---------------------|
| 39    | 39 (100%)    | 6 (15%)       | 6 (15%)          | 0 (0%)              |

### Conclusion (AC #4)

Of the four correlation IDs ADR 023 requires, 3 are emitted today on ~21% of records (combined 62 records across both files; per-ID coverage: event_id 63%, turn_id 10%, session_id 10%, agent_run_id 0%).
