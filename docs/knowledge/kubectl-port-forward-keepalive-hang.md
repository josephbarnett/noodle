# `kubectl port-forward` hangs idle keep-alive connections

**Status:** Hard-won discovery. Captured 2026-06-14 after a multi-hour
rabbit hole. This is a **test-harness gotcha**, not a noodle defect — it
exonerates the proxy and tells you how to test the gateway correctly.

## Symptom

Driving Claude (or any HTTP/1.1 keep-alive client) at the noodle gateway
running in a cluster, **through `kubectl port-forward`** (e.g. the default
`scripts/watch-gateway.sh` path, or Rancher Desktop's port-forward):

- The first request on a fresh connection works.
- A subsequent request, after a short idle gap, **hangs for ~10 minutes**.
- The client eventually times out, resets the connection, retries on a
  fresh one, and **succeeds in seconds** — then the cycle repeats.

It looks exactly like a proxy bug "failing to finish the turn." It is not.

## Root cause

`kubectl port-forward` tunnels your TCP connection over a **multiplexed
SPDY stream** (`client → kube-apiserver → kubelet → pod`). It mishandles
**idle HTTP/1.1 keep-alive** connections:

1. The gateway finishes a response; the server-side h1 connection goes
   `keep_alive: Idle` and waits for the next request (correct behavior —
   noodle's default `h1_header_read_timeout` is 30s).
2. **~1 second later, port-forward tears down the pod-side of the idle
   stream** — the gateway sees a clean `read eof` and shuts the connection
   down correctly.
3. **port-forward does NOT propagate that close back to the local client.**
   The client's connection pool (e.g. Node/undici) still believes the
   connection is alive.
4. The next request is written into the dead pooled connection. No bytes
   reach the gateway (no new flow appears in its logs). The client waits
   until its own ~10-minute timeout, resets, and retries on a fresh
   stream — which works.

## Evidence (2026-06-14)

Same release binary, two paths:

| Path | Result |
|------|--------|
| Client → `kubectl port-forward` → in-cluster gateway | hang on ~every reuse; cluster trace shows ingress `read eof` **1.0s** after `keep_alive: Idle`, client never notified |
| Client → `127.0.0.1` → local `make run-release` (no port-forward) | **6/6 prompts clean**, slowest turn 81s, **zero** turns ≥5min; client closes idle connections cleanly in **~0.4ms** |

The proxy's turn-finish is identical in both cases (`keep_alive: Idle` →
clean shutdown). Only the transport in front of it differs.

## How to test the gateway correctly

- **Locally:** `make run-release`, point the client straight at
  `127.0.0.1:<port>`. No SPDY tunnel, no artifact.
- **In-cluster:** reach the gateway via **NodePort**, LoadBalancer, or an
  Ingress/Gateway — never `kubectl port-forward` for real traffic. Rancher
  Desktop exposes NodePorts on `localhost`, which bypasses the tunnel.
- A hang that appears **only** through port-forward is a harness artifact.
  Confirm against a non-port-forward path before suspecting the proxy.

## Diagnosing this class of issue

The per-flow trace built during this hunt is the tool:

- `FlowTrace` (`crates/noodle-proxy/src/flow_trace.rs`,
  `crates/noodle-macos-tproxy/src/flow_trace.rs`) tags every line of a
  connection with one `flow.id`; a wedged flow shows `flow.start` with no
  `flow.end`.
- `RUST_LOG=info,noodle_proxy=debug,rama_net::proxy::forward=trace,rama_http_backend=debug,rama_http_core::proto::h1::conn=trace`
  surfaces the h1 keep-alive/close state machine and bridge close reasons.
- Full recipe: `docs/guides/proxy-flow-trace.md`.
