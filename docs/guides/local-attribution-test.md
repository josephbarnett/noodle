# Local attribution loop — live test runbook (Path B)

**Last updated:** 2026-05-17

A 5-minute path to running the **full attribution loop** end-to-end
against a real LLM, with no sysext, no system-wide trust install,
no reboot. This is **Path B** — HTTPS_PROXY forward proxy. The
machinery the recipe relies on: `NOODLE_LAYERED_CORE` (the engine
opt-in), `MarkerStripTransform` + `AttributionInjector` (ADR 018),
the side-effect sink + Resolver wiring (ADR 020), and the
`SideEffectsJsonlSink` that writes one JSONL line per emission.

For the full pipeline driven by a real `claude` CLI (proxy →
embellish → SQLite → shipper → OTLP), see
[`demos/end-to-end-demo.md`](../../demos/end-to-end-demo.md); for the
viewer, wire-log inspection, ops endpoints, and troubleshooting, see
[`demo.md`](demo.md). This file is the focused 5-minute path for
seeing the **attribution product** loop close end to end on real
traffic.

---

## What you will see after this works

Three terminals open. The third is tailing `side_effects.jsonl`.
You fire one Claude Code prompt. Lines like these stream past:

```json
{"kind":"audit","kind_inner":"injected","transform":"attribution-inject", ...}
{"kind":"hint","category":"tool","value":"Claude Code","source":"user_agent","confidence":0.95}
{"kind":"resolved","resolved":{"tool":"Claude Code"}, ...}        ← request flow
{"kind":"artifact","name":"work_type","value":"refactor", ...}    ← model self-tagged
{"kind":"hint","category":"work_type","value":"refactor","source":"marker","confidence":0.99}
{"kind":"resolved","resolved":{"tool":"Claude Code","work_type":"refactor"}, ...} ← response flow
```

The first three prove the **request** side of the loop. The last
three prove the **response** side. Both ResolvedRecords landing in
the JSONL is what "loop closed" means.

If you do **not** see those lines, the troubleshooting section at
the bottom maps each symptom to a cause.

---

## Prerequisites

- macOS or Linux. (Path B works on Linux too — only Path A is
  macOS-specific.)
- `cargo`, `jq` installed.
- The Claude Code CLI installed and configured with your Anthropic
  account. (`which claude` should return a path.)
- A working terminal multiplexer, or three tabs.

You do **not** need:

- The macOS Noodle.app or sysext (Path A). On this dev checkout it
  is the forbidden path — building it activates the sysext and
  corrupts the captures corpus.
- System-wide CA trust install. Per-tool env vars are enough.
- Root / sudo.

---

## One-shot runbook

### Terminal A — start noodle

```sh
make run-release-layered
```

What this does: builds the release binary, runs `noodle` with
`NOODLE_LAYERED_CORE=1`. The engine opens with:

- `SseFrameCodec` at L4 + `LayeredAnthropicCodec` at L5
- `MarkerStripTransform` on the response chain (strips
  `<noodle:NAME>VALUE</noodle:NAME>` markers; emits `Artifact` and
  `Hint` for each)
- `AnthropicMessagesRequestCodec` + `ClaudeAiChatRequestCodec` on
  the request chain
- `AttributionInjector` on the request chain (appends the
  directive to the system slot)
- `MultiSideEffectSink { TracingSink, SideEffectsJsonlSink }` for
  the engine's side-effect drain

On first run noodle generates the MITM root CA at
`~/.config/noodle/ca/ca.pem` (`Ca::generate_or_load`,
`crates/noodle-adapters/src/tls/ca.rs:118`). Subsequent runs reuse
it — same fingerprint, same trust relationship.

You should see in stderr:

```
INFO noodle_proxy: noodle MITM relay armed; clients must trust the CA at NODE_EXTRA_CA_CERTS
INFO noodle_proxy: NOODLE_LAYERED_CORE set — SSE responses decode via the layered codec stack ...
INFO noodle_proxy: noodle proxy listening addr=127.0.0.1:62100
```

### Terminal B — point Claude Code at noodle

```sh
eval "$(make demo-attribution-env)"
claude
# then ask it anything: "refactor this snippet:\nlet x = 1;"
```

`make demo-attribution-env` prints the four export lines you need:
`HTTPS_PROXY`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`,
`SSL_CERT_FILE`, `CURL_CA_BUNDLE`. `eval $(...)` applies them to
the current shell. Now any Node-based tool (Claude Code, the
Anthropic SDK, etc.) trusts noodle's CA **in addition to** the
system roots — no security downgrade.

The Claude Code CLI's `User-Agent` header (`Claude-Code/…`) is
what triggers the `user_agent_hint` UA-derived `Hint`
(`crates/noodle-proxy/src/wirelog.rs::user_agent_hint`).

### Terminal C — watch the loop close

```sh
make side-effects-tail
```

This live-tails `~/.noodle/side_effects.jsonl`. As Claude Code's
request and response stream through noodle, you should see entries
in roughly this order per turn:

1. **Audit** `kind_inner:injected` — `AttributionInjector` wrote
   the directive into the request system slot.
2. **Hint** `source:user_agent, category:tool, value:"Claude Code"`
   — UA-derived hint, fired by the wirelog at request flow end.
3. **Resolved** `{tool: "Claude Code"}` — request flow drain.
4. (Response streams; per-chunk side-effects accumulate.)
5. **Artifact** `name:work_type, value:<whatever the model emitted>`
   — `MarkerStripTransform` stripped a `<noodle:work_type>...</noodle:work_type>`
   marker from the model's text.
6. **Hint** `source:marker, category:work_type, value:<same>` —
   the strip-derived hint, fed to the Resolver.
7. **Audit** `kind_inner:redacted` — bookkeeping for the strip.
8. **Resolved** `{tool: "Claude Code", work_type: "..."}` —
   response flow drain. **This is the loop closed.**

---

## What "the loop closed" means in code

| Step | File:line | Type emitted |
|---|---|---|
| Inject directive into system slot | `crates/noodle-adapters/src/transform/attribution_inject.rs:93` | `SideEffect::Audit{kind:Injected}` |
| UA → tool hint | `crates/noodle-proxy/src/wirelog.rs::user_agent_hint` | `SideEffect::Hint{source:"user_agent"}` |
| Request-flow drain | `crates/noodle-proxy/src/wirelog.rs::request_outbound` | `SideEffect::Resolved{...}` via `engine.drain_to_sink` |
| Strip marker, emit value | `crates/noodle-adapters/src/transform/marker_strip.rs::drain` | `Artifact` + `Hint{source:"marker"}` + `Audit{kind:Redacted}` |
| Re-encode without marker | `crates/noodle-core/src/layered/engine.rs::ResponseFlow::encode_events` | client receives clean bytes |
| Response-flow drain | `crates/noodle-proxy/src/wirelog.rs::EngineState::finish` | `SideEffect::Resolved{...}` via `engine.drain_to_sink` |
| File the records | `crates/noodle-adapters/src/sink.rs::SideEffectsJsonlSink` | one JSONL line per emission |

---

## Inspecting individual signals

While `side-effects-tail` is running, the other JSONL streams stay
useful:

```sh
make tap-tail            # request/response bodies (the wire view)
make frames-tail         # per-SSE-frame timing
make events-tail         # decoded NormalizedEvents
make side-effects-tail   # attribution loop output (this story's stream)
```

All four can run in parallel; they're independent files.

## Confirming the ops listener while the loop runs

The proxy serves a separate **ops HTTP API** on `NOODLE_OPS_LISTEN`
(default `127.0.0.1:9091`) — the same surface a Kubernetes
deployment exposes for probes and Prometheus scrape (ADR 043 §2.7).
You can hit it any time while the attribution loop is running; the
calls do not touch the proxy data path.

```sh
# Liveness — process is up.
curl -sS http://127.0.0.1:9091/healthz   # → ok

# Readiness — engine wired and accepting traffic.
curl -sS http://127.0.0.1:9091/readyz    # → ready (503 + "not ready" before bind)

# Prometheus scrape — uptime + build_info + tap_enabled.
curl -sS http://127.0.0.1:9091/metrics
```

Toggle the tap gauge to prove it's live without touching the wire
(the tap defaults to **enabled**):

```sh
curl -sS -X POST http://127.0.0.1:9091/debug/tap/disable
curl -sS         http://127.0.0.1:9091/metrics | grep noodle_proxy_tap_enabled
# → noodle_proxy_tap_enabled 0

curl -sS -X POST http://127.0.0.1:9091/debug/tap/enable
curl -sS         http://127.0.0.1:9091/metrics | grep noodle_proxy_tap_enabled
# → noodle_proxy_tap_enabled 1
```

When the tap is disabled, `~/.noodle/tap.jsonl` stops growing but
the attribution loop keeps running — `side_effects.jsonl` continues
to accumulate `Resolved` records because the engine drain is
separate from the tap writer. That separation is what makes the K8s
deployment honest: probes and scrape reflect process health, not
data-path noise.

## Filtering side_effects.jsonl with jq

```sh
# Just the Resolved records — the per-flow attribution conclusions.
jq -c 'select(.kind == "resolved")' ~/.noodle/side_effects.jsonl

# Just markers stripped this session.
jq -c 'select(.kind == "artifact")' ~/.noodle/side_effects.jsonl

# Just the source=marker Hints (the model's self-tags).
jq -c 'select(.kind == "hint" and .source == "marker")' ~/.noodle/side_effects.jsonl

# Count Resolved records by tool.
jq -c 'select(.kind == "resolved") | .resolved.tool' ~/.noodle/side_effects.jsonl \
    | sort | uniq -c | sort -rn
```

---

## Iterating on the directive prompt

The injected directive lives at
`crates/noodle-adapters/src/transform/attribution_inject.rs:34`
(`DEFAULT_DIRECTIVE`). To try a different prompt:

1. Edit the constant.
2. Re-run `make run-release-layered`.
3. Fire a Claude Code prompt.
4. Watch `side-effects-tail` — confirm the model still emits the
   marker (or doesn't, with the new prompt).

This iteration loop is the motivation for [story
034](../features/034-configurable-marker-and-injection-prompts.md)
which makes the directive configurable from a file rather than
requiring a recompile.

---

## Cleanup

When you're done:

```sh
make stop      # SIGINT to noodle
```

The four JSONL files persist at `~/.noodle/`; they truncate at the
start of each `make run-release-layered`. To wipe them between
runs:

```sh
rm -rf ~/.noodle
```

The CA at `~/.config/noodle/ca/` is also reused across runs. To
regenerate (different fingerprint, requires re-pointing every
client to the new `ca.pem`):

```sh
rm -rf ~/.config/noodle/ca
```

---

## Troubleshooting

**`side_effects.jsonl` doesn't exist.**
You forgot `NOODLE_LAYERED_CORE=1`. Either use `make
run-release-layered` (which sets it) or set it manually:
`NOODLE_LAYERED_CORE=1 ./target/release/noodle`.

**`SSL_ERROR: self-signed certificate` from Claude Code.**
The CLI doesn't trust noodle's CA. Confirm
`NODE_EXTRA_CA_CERTS=~/.config/noodle/ca/ca.pem` is exported in
the shell that runs `claude` (not just in the shell where you
started noodle). Re-run `eval "$(make demo-attribution-env)"`.

**Claude Code launches and works, but `side_effects.jsonl` shows
nothing.**
The request didn't route through noodle. Check `HTTPS_PROXY` is
set in the same shell. `echo $HTTPS_PROXY` should print
`http://127.0.0.1:62100`. Claude Code respects this env var; if
it's missing, the CLI goes direct.

**`Resolved` lines show `{"tool":"Claude Code"}` but no
`work_type`.**
The model didn't emit a `<noodle:work_type>` marker on that turn.
A few causes:
1. The directive isn't reaching the model. Check for the `audit`
   line with `kind_inner:injected` in `side_effects.jsonl`. Absent
   means the request-side injection didn't fire.
2. The model emitted the marker but it's malformed. The default
   directive (`DEFAULT_DIRECTIVE`) asks for an exact form; some
   models paraphrase. Check `events.jsonl` for the raw token
   stream — search for `noodle:`.
3. The marker name isn't in `MarkerStripTransform`'s allow-list.
   The current allow-list is hardcoded in
   `crates/noodle-proxy/src/tap_setup/mod.rs` (`["work_type",
   "tool"]`). A name outside that set will not be stripped.

**The marker shows in the response text the client sees.**
`MarkerStripTransform` didn't strip it. Verify the marker name
matches the allow-list in `tap_setup/mod.rs` exactly (case-
sensitive, no whitespace).

**Port 62100 already in use.**
`make stop` to kill any prior noodle, or check `lsof -i :62100`.

**`HTTPS_PROXY` is set but `curl` won't honor it.**
Some shells reject `HTTPS_PROXY`. Use lowercase `https_proxy` or
`-x http://127.0.0.1:62100` on the command line.
