# 042 — `noodle-embellish` maps `tap.jsonl` → `ai-telemetry` v0.0.2

**Status:** Shipped (PR #94)
**Depends on:** [040.b](040.b-roundtripsink-and-roundtrips-jsonl.md), [E2](E2-ai-telemetry-fixture-mapping.md)
**Cadence:** [`docs/adrs/036`](../adrs/036-macos-collector-parity-value-cadence.md)
**Design refs:**
[`docs/adrs/031-embellishment-processor.md`](../adrs/031-embellishment-processor.md) (the processor's contract),
[`docs/adrs/027-tap-jsonl-boundary-format.md`](../adrs/027-tap-jsonl-boundary-format.md) (the source format),
[`docs/adrs/029-noodle-domain-crate.md`](../adrs/029-noodle-domain-crate.md) (`ProviderDecoder` consumed here).
**External reference:** `(external reference removed)/docs/design/ai-telemetry-event-schema.md` — the target schema (fixed external contract).

---

## 1. Value delivered

After this slice ships, running `noodle-embellish` against a fresh `tap.jsonl` produces a local SQLite database whose rows match the `ai-telemetry` v0.0.2 schema verbatim — the same schema the macOS the telemetry backend shipper already speaks. The rust pipeline becomes drop-in compatible with the existing shipper. This is the "validating consumer" ADR 031 specifies.

## 2. Acceptance criteria

1. New mapper module `crates/noodle-embellish/src/mapping/ai_telemetry_v0_0_2.rs` produces one `ai-telemetry` event per `tap.jsonl` record (joined with the matching `roundtrips.jsonl` line by `flow_id` / `event_id`).
2. SQLite schema: one table per `ai-telemetry` event type; primary key `(event_id)` so re-runs are idempotent. Indices on `(session_id, turn_id)` for downstream queries.
3. Schema parity test: for each event type in `ai-telemetry-event-schema.md`, the produced row's field names + types match exactly. Wire schema parity is the test, not internal types.
4. Idempotency: running the processor twice over the same `tap.jsonl` does not duplicate rows.
5. Correlation-quality stamp: each row carries `correlation_quality ∈ {full, wire_only, attribution_only}` so downstream consumers filter incomplete legacy records.
6. E2E: real `claude -p` → `noodle-embellish` → SQLite database matches expected row counts + field shapes.

## 3. Abstractions introduced or refined

- **`AiTelemetryV0_0_2Mapper`** (new): takes one `tap.jsonl` decoded record + the matching `roundtrips.jsonl` line; emits one `ai-telemetry` event (or several, per event-type fan-out the schema dictates).
- **SQLite schema for v0.0.2** (new in `noodle-embellish::schema`): tables + indices per AC #2.
- **`Mapper` trait** (extracted if a second target schema is ever needed; not introduced speculatively here): keep mapping concrete to v0.0.2 until a second consumer arrives.

DI seam: the mapper takes a `Clock` for any time-derived fields the schema may compute.

## 4. Patterns applied

- **Adapter** — `tap.jsonl` typed records → `ai-telemetry` v0.0.2 events. Canonical use.
- **Idempotent producer** — primary-key-on-insert so re-runs are safe.

## 5. Test plan

- **Unit:** for each `ai-telemetry` v0.0.2 event type, a golden fixture pinning the mapper output. Regenerate fixture on schema bump.
- **Property:** idempotency — run the mapper twice over the same fixture; assert row counts are equal and no duplicates exist. AC #4.
- **Integration:** new `crates/noodle-embellish/tests/e2e_ai_telemetry_v0_0_2_parity.rs` runs against a captured `tap.jsonl` fixture; asserts AC #3 against the schema doc.

## 6. PR scope

One PR. Mapper module + SQLite schema + golden fixtures + parity test. Target: under 800 reviewable lines (the schema surface dominates).

## 7. Out of scope

- Shipper handoff (file-drop vs sqlite-cursor) — [043](043-shipper-handoff-contract.md).
- Identity resolution (`device_id` → "this person") — collector's job per ADR 022 §2 point 4; [044](044-otel-collector-separate-repo.md).
- Cost-rate-card application — same as above.
- A second target schema — speculative; introduce a `Mapper` trait only when a second consumer arrives.
