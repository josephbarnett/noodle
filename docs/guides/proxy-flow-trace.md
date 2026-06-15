# Diagnosing client hangs with per-flow trace

A `FlowTrace` decorator wraps each per-connection service on both proxy
paths. Every flow gets a unique `flow.id`; all log lines for that
connection — including rama's relay/bridge logs — share it.

The diagnostic signal for a hang: a wedged connection logs `flow.start`
(and whatever inner checkpoints fire) but **never `flow.end`**. Find the
hung `flow.id`, and the **last line under it is where the flow stalled**.

## What you'll see

```
DEBUG flow{id=42 kind=forward}: flow.start
DEBUG flow{id=42 kind=forward}: ... rama relay / handshake / copy lines ...
DEBUG flow{id=42 kind=forward}: flow.end elapsed_ms=1183 outcome=ok
```

A hang looks like a `flow.id` with `flow.start` but no `flow.end`. Common
last-line locations and what they mean:

| Last line under the hung `flow.id` | Where it wedged |
|---|---|
| `flow.start`, nothing after | SNI peek — client opened the socket but the ClientHello never completed |
| MITM/handshake debug lines, then silence | upstream TLS handshake or cert mint stalled |
| copy/bridge lines, then silence | byte bridge — likely a half-close (FIN) not propagated across the relay |

rama's bridge emits a `BridgeCloseReason` on close
(`PeerEofLeft/Right`, `IdleTimeout`, `ReadError/WriteError`,
`Shutdown`). Seeing that line means the flow *did* close — note which
reason. Not seeing it means it's still wedged.

## Forward proxy (`make run-release`)

Logs trace to stdout/stderr in your terminal. Enable:

```sh
RUST_LOG=noodle_proxy=debug,rama=debug make run-release
# add rama=trace for per-copy byte progress:
RUST_LOG=noodle_proxy=debug,rama=trace make run-release
```

Drive traffic through it (`HTTPS_PROXY=http://127.0.0.1:<port>`), and when
a client hangs, note the time and grep the last `flow.id` still missing a
`flow.end`.

## macOS transparent proxy (system extension)

The sysext routes `tracing` to macOS unified logging via `tracing-oslog`
(subsystem `com.noodleproxy.macos`, category `noodle-macos-tproxy`), not
a terminal. The subscriber in `init_tracing` installs **no `EnvFilter`**,
so `RUST_LOG` has no effect and every level is emitted — you just have to
ask `log` for `debug`:

```sh
log stream --predicate 'subsystem == "com.noodleproxy.macos"' --level debug
# or the repo target, which filters on the provider process name:
make macos-logs
```

## If `flow.start`/`flow.end` isn't enough

A wedged flow currently has only the 15-minute engine `tcp_idle_timeout`
(transparent path) / no per-bridge idle timeout (forward fallback) as a
backstop — so it just sits there until you abort. To force a fast,
*logged* close (`BridgeCloseReason::IdleTimeout`) we can add a short,
env-gated idle timeout to `IoForwardService` (off by default). Ask for it
once you've confirmed trace alone doesn't localize the stall.
