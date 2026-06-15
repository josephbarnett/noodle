# Inspection, viewer & troubleshooting guide

**Companion to [`demos/end-to-end-demo.md`](../../demos/end-to-end-demo.md).**
That runbook is the place to start — it drives the full pipeline
(a real `claude` CLI → `noodle-proxy` → embellish → SQLite → shipper
→ OTLP) and verifies every shipped feature against real traffic.

This guide covers the standing surfaces you use *while* the proxy
runs and does not repeat the drive-traffic steps:

- **§1** inspecting the `tap.jsonl` wire log with `jq`
- **§2** verifying the minted TLS leaf
- **§3** the browser viewer (HTTP / SSE / OODA modes)
- **§4** the ops endpoints (`/healthz`, `/readyz`, `/metrics`, tap control)
- **§5** troubleshooting

Everything here assumes the proxy is already running — `make run-release`
(passthrough) or `make run-release-layered` (attribution engine on).
See the end-to-end runbook §3–§4 to get there. `make help` lists
every Makefile target.

---

## 1. Inspecting the wire log with `jq`

The proxy writes one `tap.jsonl` line per request **or** response.
The on-disk contract is `crates/noodle-tap/src/contract.rs` (`TapEntry`);
the fields that matter for inspection:

| Field | Present on | Meaning |
|---|---|---|
| `direction` | both | `"request"` or `"response"` |
| `timestamp` | both | RFC3339Nano UTC |
| `event_id` | both | pairs the request and response of one exchange (`nl-N`) |
| `provider` | both | resolved upstream (`anthropic`, `openai`, …) |
| `method`, `url` | request | HTTP method + reconstructed absolute URL |
| `status` | response | HTTP status code |
| `headers` | both | `name → [values]` map; sensitive values redacted |
| `body` | both | pre-mutation body (JSON object when parseable, else a string) |
| `body_out` | both | post-mutation body — **present only when noodle changed the bytes** |
| `content.blocks[]` | response | decoded `text` / `thinking` / `tool_use` blocks |
| `events[]` | response | parsed SSE stream — `{ts_offset_ms, type, …}` per event |
| `marks`, `usage`, `envelope` | varies | correlation IDs, token counts, observation context |

> `frames.jsonl` and `events.jsonl` no longer exist — ADR 023 / story
> 035 retired the sidecar files. The parsed SSE stream now lives inline
> on each response record's `events[]` field, and the typed blocks on
> `content.blocks[]`.

Tail the log live while traffic flows:

```sh
make tap-tail        # = tail -F ~/.noodle/tap.jsonl | jq .
```

Common queries (run against `$(make -s tap-path)`):

```sh
TAP="$(make -s tap-path)"

# One-line summary per record.
jq -c '{ts: .timestamp, dir: .direction, id: .event_id,
        m: .method, st: .status, url: .url}' "$TAP"

# Only request bodies with a JSON content-type. Header keys are
# case-preserved from the wire (HTTP/2 lowercases, HTTP/1 may not),
# so match case-insensitively.
jq 'select(.direction == "request"
           and ([.headers | to_entries[]
                 | select(.key | ascii_downcase == "content-type")
                 | .value[]] | any(test("json"))))
    | {url, body}' "$TAP"

# Where did noodle mutate the bytes? body_out is present only on
# records the injector or marker-strip touched — the diff body→body_out
# is the audit trail of what changed.
jq -c 'select(.body_out != null)
       | {id: .event_id, dir: .direction, before: .body, after: .body_out}' "$TAP"

# Confirm API keys are redacted on the request side.
jq '.headers["X-Api-Key"]' "$TAP"
# → ["sk-ant-secre...<redacted>"]   (prefix-preserving, ADR 027 §9)

# Decoded model output, in arrival order, from the inline SSE stream.
jq -r '.events[]? | select(.type == "content_block_delta")
       | .delta.text // empty' "$TAP"

# Typed response blocks (text / thinking / tool_use).
jq -c '.content.blocks[]? | {type, name}' "$TAP"
```

For the **correlation- and round-trip-specific** queries
(`marks.session_id` / `turn_id` / `agent_run_id`, `roundtrips.jsonl`,
the attribution `side_effects.jsonl` bus), the validated recipes live
in the end-to-end runbook §5.1–§5.3 — they are not duplicated here.

---

## 2. Verify the minted TLS leaf

To see the per-host leaf the relay mints for an upstream:

```sh
echo | openssl s_client -showcerts -connect api.anthropic.com:443 \
  -proxy 127.0.0.1:62100 2>/dev/null \
  | openssl x509 -noout -issuer -subject -ext subjectAltName
```

Expected:

```
issuer=CN=noodle MITM root CA, O=noodle
subject=CN=api.anthropic.com
X509v3 Subject Alternative Name:
    DNS:api.anthropic.com
```

The `subject` and SAN list are mirrored from the real upstream cert;
the `issuer` is noodle's root. Full CA lifecycle, per-leaf minting,
trust model, and threat analysis (with diagrams):
[`docs/adrs/011-tls-mitm-and-ca.md`](../adrs/011-tls-mitm-and-ca.md).

Inspecting Electron apps (Claude Desktop, VS Code, Cursor) needs extra
setup for HTTP/3, IPv6 happy-eyeballs, and Chromium trust handling —
see [`inspecting-claude-desktop.md`](inspecting-claude-desktop.md).

---

## 3. The browser viewer — HTTP / SSE / OODA modes

**What it does.** Browser-based viewer at `http://localhost:9092` that
reads the same `tap.jsonl` stream. Three modes:

- **HTTP** — flat exchange list with request/response detail.
- **SSE** — per-frame waterfall with relative arrival times.
- **OODA** — conversation reconstruction with sessions, turns, blocks,
  sub-agent linking, tool-bucket badges, and tool-call clustering.

**Drive it.**

```sh
make viewer-build     # one-time: npm install + vite build
make viewer           # opens at http://localhost:9092
```

Then drive any traffic through the proxy in another terminal.

**What proves each mode works.**

1. **HTTP mode** — exchange rows appear within ~1 s of the proxy
   logging them; clicking a row opens a detail panel with parsed
   request + response bodies.
2. **SSE mode** — the rail lists every request that produced frames;
   selecting one shows a waterfall with `+Δms` deltas monotonic from
   frame 0.
3. **OODA mode** — sessions are grouped; turns and round-trips match
   what was sent; sub-agents (e.g. `Agent` tool calls) link to their
   child run via the inline `↳ run #N · view →` button.

**Tool-bucket badges.** Every TOOL row in OODA mode carries a colored
pill: `built-in` (orange — `Read`/`Bash`/`Edit`/`Agent`), `<server>`
(green — MCP tools, e.g. `mcp__claude_ai_Gmail__create_draft` shows
`claude_ai_Gmail`), or `skill` (amber — the `Skill` meta-tool). Hover
a green pill to see the parsed `<tool>` name.

**Tool-call clustering.** Runs of N≥2 consecutive tool-use blocks in
the same round-trip collapse into one `TOOL CLUSTER (×N)` row;
click to expand. A tool followed by a `thinking` / `agent-text` block
followed by more tools does **not** cluster — the agent's pause is
preserved.

---

## 4. Ops endpoints — `/healthz`, `/readyz`, `/metrics`

The proxy serves a small ops HTTP API on a separate listener —
`NOODLE_OPS_LISTEN`, default `127.0.0.1:9091`. The split keeps probe
and scrape traffic off the proxy data path. ADR 043 §2.7 specifies the
contract; the Kubernetes runbook
([`kubernetes-gateway-deployment.md`](kubernetes-gateway-deployment.md) §6)
wires `httpGet` probes against `/readyz` and `/healthz`.

| Route | Method | Purpose | Response |
|---|---|---|---|
| `/healthz` | GET | Liveness — process is running | `200 ok` |
| `/readyz` | GET | Readiness — engine wired and accepting traffic | `200 ready` once `noodle_proxy::start()` returns; `503 not ready` before then |
| `/metrics` | GET | Prometheus text exposition | `noodle_proxy_uptime_seconds`, `noodle_proxy_build_info{version=...}`, `noodle_proxy_tap_enabled` |
| `/debug/tap/status` | GET | Current tap state | `{"enabled":bool,"file":"..."}` |
| `/debug/tap/enable` | POST | Resume tap writing (viewer Start Capture) | `{"enabled":true,"file":"..."}` |
| `/debug/tap/disable` | POST | Pause tap writing (viewer Stop Capture) | `{"enabled":false}` |

**Drive it.**

```sh
make run-release &
# off-machine: NOODLE_OPS_LISTEN=0.0.0.0:9091 make run-release &

curl -sS http://127.0.0.1:9091/healthz    # → ok
curl -sS http://127.0.0.1:9091/readyz     # → ready  (503 "not ready" before bind)
curl -sS http://127.0.0.1:9091/metrics
```

`/metrics` returns Prometheus exposition (verbatim from a running
release build):

```
# HELP noodle_proxy_uptime_seconds Seconds since the proxy started.
# TYPE noodle_proxy_uptime_seconds gauge
noodle_proxy_uptime_seconds 14.405874917
# HELP noodle_proxy_build_info Build metadata. Value is always 1.
# TYPE noodle_proxy_build_info gauge
noodle_proxy_build_info{version="0.0.1"} 1
# HELP noodle_proxy_tap_enabled Whether the tap debugger is currently writing (1) or paused (0).
# TYPE noodle_proxy_tap_enabled gauge
noodle_proxy_tap_enabled 1
```

The tap defaults to **enabled** (`1`). Toggle the gauge to prove it's
live without touching the wire:

```sh
curl -sS -X POST http://127.0.0.1:9091/debug/tap/disable
curl -sS         http://127.0.0.1:9091/metrics | grep noodle_proxy_tap_enabled
# → noodle_proxy_tap_enabled 0
curl -sS -X POST http://127.0.0.1:9091/debug/tap/enable
curl -sS         http://127.0.0.1:9091/metrics | grep noodle_proxy_tap_enabled
# → noodle_proxy_tap_enabled 1
```

**Why this matters.** This is the surface a Kubernetes operator (or
anyone running noodle as a service) sees first. A green `/readyz`
means a deploy succeeded; a red one means the process is up but cannot
serve traffic — the distinction that keeps a half-wired Pod out of the
Service endpoint list during rollouts.

---

## 5. Troubleshooting

**`bind tcp proxy` panic at startup.** Port 62100 already in use.
`make stop` to kill the previous noodle, or set `NOODLE_LISTEN` to a
free address.

**Ops listener fails to bind.** Port 9091 already in use (e.g. a stale
proxy). `make stop`, or set `NOODLE_OPS_LISTEN` to a free address.

**`502 Bad Gateway` on plain HTTP.** DNS or upstream connectivity.
Check `noodle.err` for `upstream request failed`. The wire log shows a
synthesized `status=502` so the record is complete.

**`curl: (35) … certificate verify failed`** (or a similar TLS error
from a Node client). The client doesn't trust noodle's CA. Either:
- `--cacert ~/.config/noodle/ca/ca.pem` (curl)
- `export NODE_EXTRA_CA_CERTS=$HOME/.config/noodle/ca/ca.pem` (Node)
- `export REQUESTS_CA_BUNDLE=$HOME/.config/noodle/ca/ca.pem` (Python)
- `make ca-trust-macos` (macOS system trust; broader blast radius)

**`curl: (56) Recv failure: Connection reset by peer` on HTTPS.** The
upstream rejected our re-originated TLS — almost always SNI/ALPN
mismatch when an upstream pins on something we don't mirror. Capture
stderr at debug (`RUST_LOG=rama_tls=debug,rama_http=debug make run-release`)
and read the handshake error.

**Anthropic returns `400 … credit balance is too low`.** Account-side,
not proxy-side. The MITM is fine — the body and response round-tripped.

**Wire-log entries appear out of order with client stderr.** Expected.
The proxy logs as soon as a body is buffered, which can race with the
client's own output. `event_id` is the source of truth for ordering.
