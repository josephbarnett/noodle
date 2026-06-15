# End-to-end product demo — every feature shipped, exercised live

Drive a real `claude` CLI through the production `noodle-proxy` binary, then chase the data through the rest of the pipeline (embellish → SQLite → shipper → OTLP). Every step shows a concrete output you can eyeball. **No unit tests.** This is the actual product against actual inputs.

The runbook assumes macOS / Apple Silicon. Linux notes are inline where they diverge.

> For the standing surfaces you use *while* the proxy runs — the browser viewer, `jq` wire-log inspection, the ops endpoints (`/healthz` · `/readyz` · `/metrics`), and troubleshooting — see the companion guide [`docs/guides/demo.md`](../docs/guides/demo.md).

---

## Feature checklist — what this demo verifies

| # | PR | Feature | Verified by step |
|---|---|---|---|
| 040.a | #87 | Correlation block (`event_id`, `turn_id`, `session_id`, `agent_run_id`) on every data-plane record | §5.1, §5.2, §5.3 |
| 040.b | #88 | `roundtrips.jsonl` — one self-contained record per LLM round trip | §5.2 |
| 040.c | #93 | Turn + agent-run boundary detection (system-prompt hash → `agent_run_id`; `stop_reason` → `turn_id`) | §5.2 |
| 042 | #94 | `noodle-embellish` maps `tap.jsonl` → `ai-telemetry` v0.0.2 SQLite rows | §6 |
| 043 | #95 | `noodle-shipper` reads rollups SQLite, emits OTLP/HTTP to a configurable collector | §7 |
| #97 | A.4 | SSE frame buffer cap (4 MiB) — overflow counter + recovery | §8 |
| #98 | B.1 | `noodle-sinks` crate carved out — file-backed `SideEffectSink` adapters | §3 (build), §9 (crate graph) |
| #99 | B.2 | `noodle-cert-external` crate carved out — Vault PKI signer | §3, §9 |
| #100 | B.3 | `noodle-embellish-core` carved out — pure mapper library | §3, §6, §9 |
| #101 | B.4 | `noodle-detect` facade crate — synchronous `detect()` API for plugin embedding | §3, §9 |
| #102 | B.5 | `cargo build --target wasm32-unknown-unknown -p noodle-detect` succeeds — plugin topology buildable | §9 |
| ADR 041 | #104 | L5 coverage design contract — `tool_use` accumulation + usage on `TurnEnd` | docs only — read [`docs/adrs/041`](../docs/adrs/041-l5-coverage-tool-use-and-usage.md) |
| #105 | A.1.a | `tool_use` content blocks → first-class `ToolCall` events | §5.2 |
| #106 | A.1.b | `TurnUsage { input_tokens, output_tokens, cache_read, cache_write }` stamped on `TurnEnd` | §5.2 |
| ADR 042 | #107 | Codec side channel + `emit_errored` — §16 error contract on the layered path | docs only — read [`docs/adrs/042`](../docs/adrs/042-codec-side-channel-and-error-contract.md) |
| #107 | A.3 | SSE buffer overflow + tool_use accumulator overflow emit `AuditEvent::Errored` audits via the side channel | §8 |
| #108 | A.7 | Perf bench legacy vs layered — verbatim numbers in `docs/guides/codec-perf-bench.md` | §10 |
| #109 | A.8.a | Legacy `ProviderCodec` / `StreamingDecoder` / `AnthropicCodec` / `OpenAiCodec` / `OrderedCodecRegistry` deprecated — new uses get a compile warning | §11 |
| 037 / 038 / 039 / 040 | #92 / #96 | Foundation + componentization + post-parity cadence ADRs | docs only — read [`docs/adrs/`](../docs/adrs/) |
| E | #103 | Backlog hygiene — overview reflects shipped slices; stories 045/046 file 043 follow-ups | docs only — read [`docs/features/000-overview.md`](../docs/features/000-overview.md) |

Items omitted from the live demo because they aren't claude-CLI exercisable: the macOS endpoint product (Track C), enterprise CA + external signing (Track D), the OTel collector in its sister repo (story 044).

---

## 1. Prerequisites

One command verifies the whole host. One command installs what's installable.

```bash
make doctor                        # verify; non-zero exit if anything is missing
make tooling                       # install jq / sqlite / python3 via brew + wasm32 target via rustup
```

What `make doctor` checks:

| Tool | Why |
|---|---|
| `cargo` + `rustup` | Building the binaries |
| `claude` (Claude Code CLI) | The traffic generator |
| `jq` | Reading JSONL outputs |
| `sqlite3` | Reading the embellish rollups DB |
| `python3` | Tiny OTLP sink for §7 |
| `wasm32-unknown-unknown` target | Building the plugin facade (B.5) |

`make tooling` covers everything except `claude` (Node package — install with `npm i -g @anthropic-ai/claude-code`).

Claude auth: `claude` uses its own credential store (Keychain / enterprise login). If `claude -p hi` works in a fresh shell, you're set — no env var needed.

---

## 2. Workspace prep — clean slate

Wipe last run's data files; preserve the CA (it's tied to the trust chain your clients are configured against).

```bash
cd $HOME/business/code/josephbarnett/noodle    # adjust to your checkout
rm -f ~/.noodle/tap.jsonl \
      ~/.noodle/roundtrips.jsonl \
      ~/.noodle/side_effects.jsonl \
      ~/.noodle/proxy.log
rm -rf /tmp/noodle-demo
mkdir -p /tmp/noodle-demo
ls ~/.noodle/    # expect: empty or only viewer.log / ca.pem remnants
```

---

## 3. Build the release binaries

```bash
make build                         # workspace native build (proxy, embellish, shipper, viewer)
make wasm                          # plugin facade for wasm32-unknown-unknown (B.5)
```

What landed:

```bash
ls -la target/release/noodle target/release/noodle-embellish \
       target/release/noodle-shipper target/release/noodle-viewer
ls -la target/wasm32-unknown-unknown/release/libnoodle_detect.rlib
```

The five files exist → **B.1 / B.2 / B.3 / B.4 / B.5 verified**: the crate carve-outs compile clean *and* the plugin facade builds for `wasm32-unknown-unknown` without `#[cfg]` source guards (ADR 039 §8 signal #1).

---

## 4. Start the proxy

```bash
make run-release-layered &        # start in background
sleep 2
make ca-path                       # print the CA path the layered proxy minted
```

The proxy listens on `127.0.0.1:62100`. CA mode defaults to `local` (auto-generates a self-signed root the first time).

`make demo-attribution-env` prints the exact `export` lines to paste into your client shell:

```bash
make demo-attribution-env          # ← copy-paste the four exports it prints
```

For convenience, those are:

```bash
export HTTPS_PROXY=http://127.0.0.1:62100
export NODE_EXTRA_CA_CERTS=$(make -s ca-path)
export REQUESTS_CA_BUNDLE=$(make -s ca-path)
export SSL_CERT_FILE=$(make -s ca-path)
```

Sanity check the proxy banner — `make run-release-layered` logs to stderr, redirected by the Makefile to `./noodle.err`:

```bash
grep -E "CA configuration|tap debugger|round-trips|NOODLE_LAYERED_CORE" ./noodle.err 2>/dev/null
```

You'll see the layered codec stack announcing itself (`L4 SseFrameCodec → L5 LayeredAnthropicCodec`) and the file paths for `tap.jsonl` + `roundtrips.jsonl`.

---

## 5. Drive real traffic — one tool-using prompt

The prompt deliberately invokes a Bash tool so the response carries a `tool_use` content block. That exercises **A.1.a** (`ToolCall` event emission) and **A.1.b** (`TurnUsage` extraction) in the same shot.

Open a second shell, paste the `export …` lines from §4, then:

```bash
claude -p "List the three largest files under /tmp by size, in MB" 2>&1 | tail -20
```

Optional — live-tail the attribution bus from a third shell so you watch the records land as claude runs:

```bash
make side-effects-tail             # Ctrl-C when claude finishes
# (alternatives: `make tap-tail` for raw wire records, `make roundtrips-tail` for per-turn summaries)
```

The proxy MITM'd the request and tapped both directions. The Makefile knows where each file lands:

```bash
wc -l "$(make -s tap-path)" \
      "$(make -s roundtrips-path)" \
      "$(make -s side-effects-path)"
```

Three non-zero counts → the proxy did see traffic. If they're all zero, the claude exec didn't go through the proxy (re-check `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS` from §4).

> Note: `events.jsonl` and `frames.jsonl` were retired by ADR 023 / story 035 — the parsed events live on `tap.jsonl`'s `events[]` field now. No separate sidecar files.

### 5.1 Correlation block on `tap.jsonl` (040.a)

Each tap record carries a `direction` (`request` or `response`), an `event_id` minted by the proxy (`nl-1`, `nl-2`, …), and — on records inside an Anthropic round trip — a `marks` block holding the four correlation IDs.

```bash
# Plain shape — every line
jq -c '{direction, event_id, marks}' "$(make -s tap-path)" | head -4

# The records inside an Anthropic round trip (marks populated)
jq -c 'select(.marks != null) | {direction, event_id, marks}' "$(make -s tap-path)" | head -3
```

You'll see `marks.session_id`, `marks.turn_id`, and `marks.agent_run_id` populated when the proxy hit the configured cell `(api.anthropic.com, /v1/messages, request→upstream)`. The early `nl-1` … `nl-4` records are typically `GET /v1/mcp_servers?…` calls Claude makes before the first chat — they have `event_id` but no `marks` because they don't carry user/turn context.

### 5.2 `roundtrips.jsonl` — one self-contained record per LLM round trip (040.b + 040.c + A.1.a + A.1.b)

The headline file. Every LLM round trip lands here pre-correlated, attribution-resolved, and usage-stamped.

```bash
jq -c '{event_id, session_id, turn_id, agent_run_id, model: .request.model, directive_injected: .request.directive_injected, stop_reason: .response.stop_reason, tools: (.response.tools_invoked // [] | map(.name)), input_tokens: .usage.tokens.input_tokens, output_tokens: .usage.tokens.output_tokens, cache_read: .usage.tokens.cache_read_tokens, attributions}' \
    "$(make -s roundtrips-path)"
```

What to look for, per row:

| Field | What it proves |
|---|---|
| `event_id` populated (`nl-…`) | 040.a — proxy-assigned per-round-trip ID |
| `session_id` populated | 040.a — wire-extracted session ID stamped on every record |
| `turn_id` populated (ULID) | 040.c — turn boundary detected on `stop_reason` |
| `agent_run_id` populated (ULID) | 040.c — system-prompt hash drives agent-run boundary |
| `directive_injected: true` | The attribution injector on the request path fired |
| `stop_reason: "tool_use"` AND `tools: ["Bash", …]` non-empty | A.1.a — `LayeredAnthropicCodec` accumulated `input_json_delta` chunks into a `ToolCall` and `.response.tools_invoked` records it |
| `input_tokens` / `output_tokens` / `cache_read` populated | A.1.b — `TurnUsage` extracted from `message_delta.usage` and stamped on `TurnEnd` |
| `attributions.tool = "Claude Code"` and (when the model emitted markers) `attributions.work_type` | The Resolver closed the attribution loop end-to-end |

If `tools` is empty, the model didn't invoke a tool for that turn — re-run §5 with a more directive prompt like `"Use the Bash tool to list files in /tmp"`.

### 5.3 `side_effects.jsonl` — the attribution bus (040.a + 038)

```bash
# Tally what got emitted
jq -r '.kind' "$(make -s side-effects-path)" | sort | uniq -c

# Per-record shape: kind + correlation IDs (top-level, not nested)
jq -c '{kind, event_id, session_id, turn_id, agent_run_id}' "$(make -s side-effects-path)" | head -10

# Hints (detected attributions before the Resolver runs)
jq -c 'select(.kind == "hint") | {category, value, confidence, source}' "$(make -s side-effects-path)" | head -5

# Audits (transforms recording what they did — injection, redaction, errors)
jq -c 'select(.kind == "audit") | {event_id, kind_inner, transform, detail}' "$(make -s side-effects-path)" | head -5

# Artifacts (marker-strip extractions)
jq -c 'select(.kind == "artifact") | {event_id, name, value, source_transform}' "$(make -s side-effects-path)" | head -5
```

Each record carries the same correlation IDs as the `tap.jsonl` / `roundtrips.jsonl` records that produced it. **038** pins this exact wire format.

---

## 6. Embellish — map the wire shape to `ai-telemetry` v0.0.2 (042 + B.3)

Run the mapper. It reads `tap.jsonl` (joined against `roundtrips.jsonl` for attribution context) and writes per-row rollups to SQLite.

```bash
make embellish                                       # writes to $(ROLLUPS_DB) → /tmp/noodle-rollups.db
# or override the destination:
# make embellish ROLLUPS_DB=/tmp/noodle-demo/rollups.db
```

The CLI prints a summary line — `read=N requests=R responses=R rows_written=W unpaired_req=0 orphan_resp=0`. Then verify the rows landed:

```bash
sqlite3 /tmp/noodle-rollups.db <<'SQL'
.headers on
.mode column
-- Headline: the LLM round trips, with usage + correlation quality
SELECT
    event_id,
    schema_version,
    provider,
    endpoint_path,
    model,
    status_code,
    input_tokens,
    output_tokens,
    correlation_quality,
    delivery_status
FROM ai_telemetry_v_0_0_2
WHERE endpoint_path = '/v1/messages'
LIMIT 5;

-- Tallies that exercise each piece of the pipeline
SELECT 'total rows', COUNT(*) FROM ai_telemetry_v_0_0_2;
SELECT 'messages turns', COUNT(*) FROM ai_telemetry_v_0_0_2 WHERE endpoint_path = '/v1/messages';
SELECT '  with usage', COUNT(*) FROM ai_telemetry_v_0_0_2 WHERE endpoint_path = '/v1/messages' AND output_tokens > 0;
SELECT 'correlation_quality breakdown' AS _, correlation_quality, COUNT(*) FROM ai_telemetry_v_0_0_2 GROUP BY correlation_quality;
SQL
```

What to look for:

| Column | What it proves |
|---|---|
| `schema_version = "0.0.2"` | 042 — mapper writes the external `ai-telemetry-event-schema.md` contract |
| `input_tokens` / `output_tokens` populated on `/v1/messages` rows | A.1.b — `TurnUsage` from the layered codec flows through the embellish core (B.3) into SQLite |
| `correlation_quality = "full"` on the turns where the marker-strip artifact joined | Pinned by #94 — the mapper grades each row on how complete the correlation evidence is (`full`, `wire_only`, `attribution_only`, `minimal`) |
| `delivery_status = "pending"` | 043 — every row lands in the cursor-on-flag state machine ready for the shipper to claim |

---

## 7. Ship — OTLP/HTTP to a collector (043)

Stand up a one-shot OTLP sink that just prints what arrives. Python's `http.server` is enough:

```bash
make otlp-sink-kill                # frees 4318 if a stale sink from a previous run is still bound
make otlp-sink &                   # foregrounded if you prefer: drop the &
```

That target invokes [`demos/otlp_sink.py`](otlp_sink.py) — a small, tested receiver that:

- captures the full body of every `POST /v1/logs` to `/tmp/noodle-demo/otlp-bodies/NNN.json`,
- prints one stderr line per POST: `OTLP <- /v1/logs bytes=… log_records=… -> …/001.json`,
- exits non-zero with `port already in use by pid X — kill it and retry` if the port is taken (no AddrInUse traceback to debug),
- shuts down cleanly on `SIGINT` / `SIGTERM`.

Self-tested by [`demos/test_otlp_sink.sh`](test_otlp_sink.sh) (4 checks: clean POST, malformed body, SIGINT exit, AddrInUse). Run `bash demos/test_otlp_sink.sh` to re-verify the four checks pass.

Run the shipper against it. The shipper walks the `pending` cursor over `rollups.db` and POSTs `/v1/logs`:

```bash
make ship 2> /tmp/noodle-demo/shipper.log &
SHIPPER=$!
echo "$SHIPPER" > /tmp/noodle-demo/shipper.pid
sleep 4
# (override the endpoint:  make ship OTLP_ENDPOINT=http://collector.internal:4318)
```

As the shipper POSTs, the sink (still running in the terminal where you started `make otlp-sink`) prints one line per request:

```
OTLP <- /v1/logs bytes=102904 log_records=25 -> /tmp/noodle-demo/otlp-bodies/001.json
```

The real artifact is the file it points at — the full POST body captured on disk:

```bash
# What the sink captured this session
ls -la /tmp/noodle-demo/otlp-bodies/

# Resource attributes the shipper stamped on the batch
jq '.resourceLogs[0].resource.attributes | map({(.key): .value.stringValue}) | add' \
    /tmp/noodle-demo/otlp-bodies/001.json
# → {"agent.version":"0.0.1","agent.arch":"aarch64","schema_id":"ai-telemetry","schema_version":"0.0.2","service.name":"noodle-shipper"}

# Log-record count (one per rollup row)
jq '.resourceLogs[0].scopeLogs[0].logRecords | length' /tmp/noodle-demo/otlp-bodies/001.json
# → 25

# One Log record, in full — the per-row attribute set the collector will consume
jq '.resourceLogs[0].scopeLogs[0].logRecords[0]' /tmp/noodle-demo/otlp-bodies/001.json
```

If your queue was already drained from a previous run (`make ship-status` shows `delivered = N`, `pending = 0`), `make ship` will do nothing this turn — use `make ship-rewind` to flip everything back to `pending`, then re-run `make ship`.

The shipper status view (drives the cursor-on-flag state machine from the runbook):

```bash
make ship-status                   # → "pending=0 in_flight=0 delivered=N retry=0 poison=0"
```

Expected: `pending=0 in_flight=0 delivered=N retry=0 poison=0`.

---

## 8. SSE buffer cap + audit emission (A.4 + A.3)

A.4 caps the SSE parser's per-stream buffer at 4 MiB. A.3 wires that overflow to emit an `AuditEvent::Errored` on the side channel.

Easiest way to exercise it: drive a synthetic oversize POST through the proxy (Anthropic won't respond, but the codec sees the bytes and the overflow path fires):

```bash
# Send a 5 MiB body of x's to a fake SSE endpoint
head -c 5242881 < /dev/urandom | base64 | head -c 5242880 > /tmp/noodle-demo/big.bin
curl -sk -x http://127.0.0.1:62100 \
     -H 'Content-Type: text/event-stream' \
     -X POST --data-binary @/tmp/noodle-demo/big.bin \
     https://api.anthropic.com/v1/messages 2>&1 | tail -3 || true
```

Now check that the audit landed on the side-effect bus — `make side-effects-tail` will surface it live, or grep retroactively:

```bash
jq -c 'select(.kind == "audit" and .kind_inner == "errored") | {event_id, transform, detail}' \
    "$(make -s side-effects-path)" | head -3
```

You'll see an audit with `transform = "sse-frame"` and `detail.reason = "frame_buffer_overflow"` carrying `bytes_dropped`, `cap = 4194304`, and `overflow_total`. **That single audit covers both A.4 (the cap exists) and A.3 (the dual-method codec routes the overflow through `emit_errored`).**

For sanity, here's a routine audit shape that always shows up on a clean Anthropic round trip — `attribution-inject` records the directive it added, `marker-strip` records each `<noodle:…>` marker it pulled out:

```bash
jq -c 'select(.kind == "audit") | {event_id, kind_inner, transform, detail}' "$(make -s side-effects-path)" | head -5
```

---

## 9. Plugin topology — `noodle-detect` builds for WASM (B.4 + B.5)

Already built in §3. Inspect the dep graph to confirm the host-coupled crates aren't reaching the plugin facade:

```bash
cargo tree --target wasm32-unknown-unknown -p noodle-detect | head -20
cargo tree --target wasm32-unknown-unknown -p noodle-detect 2>/dev/null \
    | grep -E "rama|reqwest|rcgen|tokio$" && \
    echo "FAIL: host-coupled dep leaked into the plugin graph" || \
    echo "OK: plugin graph is rama/reqwest/rcgen/tokio-free (B.4/B.5 honoured)"
```

The artifact:

```bash
ls -la target/wasm32-unknown-unknown/release/libnoodle_detect.rlib
file target/wasm32-unknown-unknown/release/libnoodle_detect.rlib   # → "current ar archive"
```

---

## 10. Perf bench — legacy vs layered (A.7)

```bash
make bench 2>&1 | tee /tmp/noodle-demo/bench.log \
    | grep -E "^anthropic_response_body|time:|thrpt:"
```

Two paths reported with verbatim criterion output: `legacy` and `layered`. Compare against [`docs/guides/codec-perf-bench.md`](../docs/guides/codec-perf-bench.md) — your machine's numbers will differ; the layered/legacy ratio shouldn't shift by orders of magnitude.

---

## 11. Deprecation visibility (A.8.a)

Write a tiny crate that uses one of the deprecated types and see the warning:

```bash
mkdir -p /tmp/noodle-demo/deprecation-check/src
cat > /tmp/noodle-demo/deprecation-check/Cargo.toml <<EOF
[package]
name = "deprecation_check"
version = "0.0.0"
edition = "2024"

[dependencies]
noodle-adapters = { path = "$PWD/crates/noodle-adapters" }
noodle-core     = { path = "$PWD/crates/noodle-core" }
EOF

cat > /tmp/noodle-demo/deprecation-check/src/main.rs <<'EOF'
use noodle_adapters::provider::anthropic::AnthropicCodec;
fn main() {
    let _ = AnthropicCodec::new();
}
EOF

cargo build --manifest-path /tmp/noodle-demo/deprecation-check/Cargo.toml 2>&1 \
    | grep -E "warning: use of deprecated|LayeredAnthropicCodec|A.8.b" | head -5
```

The warning fires; it points at `LayeredAnthropicCodec` as the replacement and at `docs/adrs/040-post-parity-cadence.md` for the eventual removal slice.

---

## 12. Cleanup

```bash
make stop                          # SIGINT the proxy (Makefile target)

# Stop the demo's own background processes
[ -f /tmp/noodle-demo/shipper.pid ] && kill "$(cat /tmp/noodle-demo/shipper.pid)" 2>/dev/null
[ -f /tmp/noodle-demo/sink.pid ]    && kill "$(cat /tmp/noodle-demo/sink.pid)"    2>/dev/null
sleep 1

# Unset the proxy env vars from this shell
unset HTTPS_PROXY HTTP_PROXY NODE_EXTRA_CA_CERTS REQUESTS_CA_BUNDLE SSL_CERT_FILE

# (Optional) wipe the demo scratch dir
# rm -rf /tmp/noodle-demo
```

---

## What this demo does NOT show

Pinning these here so you know what you're not seeing — not bugs, scope:

| Out of demo | Where it lives |
|---|---|
| Macroless Network Extension / DNS-H3-ECH / System Keychain trust install | Track C (macOS endpoint product); needs a Mac dev cert + Swift sysext |
| External cert-mint via Vault | `noodle-cert-external` is built and tested but exercising it needs a Vault instance with PKI mounted (Track D) |
| Embellishment-plane processors (identity resolution, cost-rate-card, redaction, sampling) | OTel collector in its sister repo (story 044) |
| Configurable marker grammar / injection templates | A.9 — not yet shipped |
| Reference LiteLLM plugin against `noodle-detect.wasm` | Sister repo per ADR 039 §8 signal #3 |
| C-2 per-codec proptests / C-3 engine empty-vs-Errored divergence accounting | A.3.b / A.3.c — not yet shipped |

If you want any of these in the next iteration of this demo, file the slice and we'll wire it in.
