# `noodle-shipper` — operations runbook

The shipper is the out-of-proxy OTel exporter that drains
`noodle-embellish`'s `ai-telemetry` rollups SQLite to a
configurable OTel collector endpoint. It runs as a long-lived
process. This runbook covers starting it, monitoring it,
diagnosing common failures, and recovering from a wedged state.

## What it does

1. Polls `ai_telemetry_v_0_0_2` for rows with `delivery_status IN ('pending', 'retry')`.
2. Atomically claims up to `--batch` rows by flipping them to `'in_flight'`.
3. Maps each row to one OTLP `LogRecord` (record-scope attributes for `event_id` / `turn_id`; resource-scope for `session_id` / `agent_run_id`).
4. POSTs one `ResourceLogs` envelope per cycle to `${NOODLE_OTLP_ENDPOINT}/v1/logs`.
5. On HTTP 2xx → `delivery_status = 'delivered'`, `shipped_at = now`.
6. On HTTP non-2xx or transport failure → `retry_count++`, `delivery_status = 'retry'` (or `'poison'` past `--max-retries`).
7. Sleep `--poll-secs`. Repeat.

## Starting it

```sh
NOODLE_OTLP_ENDPOINT=http://collector.internal:4318 \
NOODLE_ROLLUPS_DB=$HOME/Library/.noodle/rollups.db \
noodle-shipper --batch 100 --poll-secs 5
```

All CLI flags are also environment variables (`--db` ↔ `NOODLE_ROLLUPS_DB`, `--endpoint` ↔ `NOODLE_OTLP_ENDPOINT`).

## Monitoring

The shipper's primary operational signal is the **per-state row count** on the rollups DB. Run:

```sh
noodle-shipper --endpoint http://unused --status
```

(Endpoint is required by the CLI but not actually contacted in `--status` mode.)

Output:

```
noodle-shipper: pending=12 in_flight=0 delivered=8347 retry=2 poison=0
```

Healthy steady state: `pending` near zero (or stably small), `in_flight` near zero between cycles, `delivered` monotonically growing.

### Alert thresholds

| Signal | Threshold | Meaning |
|---|---|---|
| `pending` count | growing over several poll cycles | Collector is unreachable or rejecting our payload — start with `collector reachable from shipper host?` |
| `retry` count | non-zero and growing | Same root cause as growing `pending` — every failed tick increments `retry`. |
| `poison` count | non-zero | A row has failed `--max-retries` times. Manual review required — see "Inspecting poison rows" below. |
| `in_flight` count | non-zero between polls | The shipper process crashed mid-export. Restart picks them up via `recover_in_flight`. |

## At-least-once + idempotency contract

- **At-least-once.** A row can be sent to the collector more than once across crashes. The collector dedupes on `event_id` (slice 042 reuses the proxy's per-flow event_id as the SQLite PK; the OTLP record carries it as a top-level attribute).
- **Idempotent on `event_id`.** `INSERT OR IGNORE` in the embellisher means re-running over the same `tap.jsonl` is safe; the shipper's `ack_delivered` only flips state — it never inserts.
- **No data loss on shipper crash.** Rows left `'in_flight'` are reset to `'pending'` on the next shipper startup. They will be re-sent. Duplicates are the collector's problem to handle.
- **No data loss on collector outage.** Rows stay `'pending'` (or `'retry'`) in the DB. The shipper retries on every poll cycle. When the collector recovers, the backlog drains.

## State transitions

```
                      claim_batch
       ┌─────────────────────────────────────┐
       ▼                                     │
  ┌────────┐  export ok    ┌───────────┐    │
  │pending │──────────────▶│delivered  │    │
  └────────┘               └───────────┘    │
       ▲                                     │
       │  export fail                        │
       │  (retry_count < cap)                │
       │                                     │
  ┌────────┐   ack_failed   ┌───────────┐    │
  │in_flight│──────────────▶│  retry    │────┘
  └────────┘                └───────────┘
       │                          │
       │ export fail              │ retry_count == cap
       │ (retry_count == cap)     ▼
       │                    ┌───────────┐
       └───────────────────▶│  poison   │
                            └───────────┘
```

## Recovering from a wedged shipper

### "The shipper process is dead but rows are stuck in `in_flight`."

```sh
# Restart the shipper. Startup calls recover_in_flight which resets
# every in_flight row back to pending.
noodle-shipper --endpoint $NOODLE_OTLP_ENDPOINT
```

That's it. The next claim cycle picks them up again. The collector sees the re-delivered records; deduping is on its side via `event_id`.

### "Rows piling up in `retry` but the collector is back up."

Retry rows are claimed alongside `pending` on the next cycle. Just wait — the shipper will drain. If the `retry_count` on those rows is already high, watch `poison` carefully.

### "Inspecting poison rows."

```sql
SELECT event_id, retry_count, last_attempt_at, last_attempt_error, session_id
FROM ai_telemetry_v_0_0_2
WHERE delivery_status = 'poison'
ORDER BY last_attempt_at DESC
LIMIT 50;
```

The `last_attempt_error` column carries the error message from the final attempt. Common patterns:

- `collector returned 400: invalid encoding` — the payload was malformed. Likely a shipper bug; capture the row and file an issue.
- `collector returned 401` — auth misconfigured. The shipper does not currently support auth headers; future work.
- `connection refused` / `dns failure` — network issue between shipper and collector.

To re-queue a poisoned row manually:

```sql
UPDATE ai_telemetry_v_0_0_2
SET delivery_status = 'pending', retry_count = 0, last_attempt_error = NULL
WHERE event_id = '<event_id>';
```

### "Database file is locked."

`SQLite` is in WAL mode and uses a 5-second `busy_timeout`. If another process is holding a long write, the shipper's claim will fail with `SQLITE_BUSY` and back off. If the lock persists, identify the holder:

```sh
lsof -- "$HOME/.noodle/rollups.db"
```

`noodle-embellish` should appear in the list when it's actively writing. If a stale process is holding it, kill it and restart the shipper.

## Schema drift

The shipper expects the slice 043 schema additive (`delivery_status` + `retry_count` + `last_attempt_at` + `last_attempt_error` columns). `noodle-embellish` creates these on first open. If you point `noodle-shipper` at a database created by a pre-043 `noodle-embellish` build, the first `claim_batch` will fail with a `no such column` error. The fix is to run a current `noodle-embellish` once against the DB — it will add the columns idempotently.

## Limits and known gaps

- **HTTP/JSON only.** OTLP/gRPC is not supported. If your collector accepts only gRPC, deploy `opentelemetry-collector-contrib`'s `otlphttp` receiver as a sidecar.
- **No auth.** The shipper sends every request with no auth headers. For collectors behind an auth proxy, terminate auth at a local sidecar (nginx, envoy).
- **No compression.** Payloads ride uncompressed. Batch size is the main lever for keeping per-cycle payload bytes manageable.
- **One row maps to one Log record.** Future schema bumps may differentiate (some `event_type`s could map to spans). v1 emits Logs uniformly.
