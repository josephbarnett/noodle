# OTel GenAI trace harness (dev) — runbook

Stand up a local `otel-collector → Tempo → Grafana` stack and replay a committed
capture into it, so a reconstructed GenAI trace is viewable in TraceQL **without**
a live proxy + Claude run. This is the fastest verify-against-real-product for
[ADR 057](../adrs/057-otel-genai-trace-export.md) (turn = trace, frame =
`invoke_agent` span). Dev-only; the production collector is separate per
[ADR 044](../features/044-otel-collector-separate-repo.md).

## Components

- **`docker/otel-genai/docker-compose.yml`** — three containers, all bound to
  `127.0.0.1`, no auth:
  - `collector` — otel-collector-contrib, OTLP/HTTP receiver on `:4318`,
    exports traces to Tempo over OTLP/gRPC. Config: `collector.yaml`.
  - `tempo` — single-binary Tempo, OTLP/gRPC ingest, query API on `:3200`,
    local block storage. Config: `tempo.yaml`.
  - `grafana` — anonymous-admin Grafana on `:3000` with the Tempo datasource
    auto-provisioned (`grafana/provisioning/datasources/tempo.yaml`).
- **`crates/noodle-trace-emitter`** (`cargo run -p noodle-trace-emitter`) —
  reads `analysis/claude-parallel-subagents`, runs the real ADR 052 §5
  frame-tree detector, maps each round-trip to a `RollupsRow`, and POSTs the
  batch through the production `noodle-shipper` OTLP exporter to the collector
  (`/v1/traces` + `/v1/logs`).

## Bring-up

```sh
docker compose -f docker/otel-genai/docker-compose.yml up -d
# Wait until Tempo is ready (~15 s):
until [ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:3200/ready)" = 200 ]; do sleep 2; done
```

## Emit a trace

```sh
cargo run -p noodle-trace-emitter -- --endpoint http://127.0.0.1:4318
# → reconstructed 12 round-trips ... exported 12 rows (/v1/traces + /v1/logs)
```

`--dry-run` prints the assembled `/v1/traces` payload instead of POSTing.
The capture carries no wall-clock timestamps, so the emitter synthesises an
ordered clock anchored just before *now* (1 s spacing, fixed 800 ms latency) so
the trace lands inside Tempo's recent-time window. The timings are ordering, not
real latency.

## View / verify

Grafana: <http://127.0.0.1:3000> → **Explore** → **Tempo** → **TraceQL**:

```
{ .gen_ai.operation.name = "invoke_agent" }
```

Drilling into the returned trace shows **4 `invoke_agent` spans** (ROOT + three
sub-agents, the sub-agents parented to ROOT) with **12 nested `chat` spans**, and
`gen_ai.usage.*` on the chat leaves.

Verify headlessly via Tempo's API:

```sh
# Search:
curl -s -G http://127.0.0.1:3200/api/search \
  --data-urlencode 'q={ .gen_ai.operation.name = "invoke_agent" }'
# Full trace (16 spans = 12 chat + 4 invoke_agent):
curl -s http://127.0.0.1:3200/api/traces/<traceID>
```

Other useful TraceQL:

| Query | Shows |
|---|---|
| `{ .gen_ai.operation.name = "chat" }` | every model round-trip (the leaves) |
| `{ .gen_ai.frame.role = "sub_agent" }` | only sub-agent frames |
| `{ .gen_ai.frame.role = "side_call" }` | off-tree side-calls (own single-span traces) |
| `{ .gen_ai.usage.output_tokens > 1000 }` | high-output round-trips |

## Verified — live in-cluster (2026-06-21)

The chain is proven end-to-end against the **real product**, not just the offline
emitter. Deployment: a `rancher-desktop` (k3s) cluster, namespace `noodle` —
`noodle-gateway` pod (containers `proxy` / `embellish` / `shipper` / `viewer`),
`noodle-otlp-sink` collector, `noodle-tempo`, `noodle-grafana`. The shipper image
is `noodle-shipper:local-hier` (the turn-grouped hierarchical span builder), args
`--db /home/nonroot/.noodle/rollups.db --endpoint http://noodle-otlp-sink:4318
--poll-secs 5`.

A live Claude Code session (with parallel `Task` sub-agents) driven through the
proxy produced, in Tempo, a per-turn trace tree. Example turn trace
`45f52e1569dbb7257dbac272ae3d7a8a` — **3 `invoke_agent` spans + 7 `chat` spans**:

```
invoke_agent ROOT
├─ chat claude-opus-4-8                    (ROOT's own round-trip)
├─ invoke_agent a783ac27e8361af43          (sub-agent frame, parent = ROOT)
│  ├─ chat × 3
└─ invoke_agent afd663a7bfc453b0b          (sub-agent frame, parent = ROOT)
   └─ chat × 3
```

Sub-agent frame ids are the real `x-claude-code-agent-id` values nesting under
ROOT; side-calls (quota / title / monitor) appear as **standalone single-span
`chat` traces** off the turn tree, as designed. Shipper delivery was clean —
`tick complete claimed=N delivered=N failed=0` across ticks. `context.*`
(ADR 056) and `brain.*` (ADR 047) attributes ride the spans
(`/api/search/tags` includes `context.cache_read_tokens`, `context.preamble_bytes`,
`context.tool`, etc.).

Headless re-check (from inside the cluster):

```sh
kubectl exec -n noodle deploy/noodle-tempo -- \
  wget -qO- 'http://localhost:3200/api/search?limit=10'        # turn traces
kubectl exec -n noodle deploy/noodle-tempo -- \
  wget -qO- 'http://localhost:3200/api/traces/<traceID>'        # full span tree
kubectl logs -n noodle deploy/noodle-gateway -c shipper --tail=10 # delivered=N failed=0
```

> Note: the dev `docker/otel-genai` Tempo uses `local` storage on an `emptyDir` —
> traces are lost on pod reschedule. Fine for dev verification; durable storage
> is the production collector's concern ([ADR 044](../features/044-otel-collector-separate-repo.md)).

## Teardown

```sh
docker compose -f docker/otel-genai/docker-compose.yml down -v   # -v drops the Tempo volume
```

## Troubleshooting

- **TraceQL returns nothing** — confirm the emitter exported (`exported N rows`)
  and that you searched the recent window (Grafana defaults to last 1 h; the
  emitter anchors to now). Check the collector forwarded:
  `docker compose -f docker/otel-genai/docker-compose.yml logs collector`
  should show `Traces ... spans: 16`.
- **Collector 404 on `GET /`** — expected; it only serves `/v1/traces` +
  `/v1/logs`.
- **Tempo `/ready` 503 at startup** — normal for ~10–15 s during WAL replay.
