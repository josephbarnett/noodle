# 011 — Transparent mode (macOS)

**Design doc:** [`docs/adrs/014-transparent-mode-and-quic-mitm.md`](../adrs/014-transparent-mode-and-quic-mitm.md)
**Status:** In progress (iteration 1 landed)

## Value

noodle intercepts traffic without the agent being configured to use
it as a proxy. On macOS, a system-extension `NETransparentProxyProvider`
claims TCP and UDP flows to AI provider hostnames before they leave
the box, hands them to the noodle proxy, and lets everything else
pass through untouched.

This retires the `HTTPS_PROXY` / system-proxy / IPv6-off /
CA-trust gymnastics in `docs/guides/inspecting-claude-desktop.md`.
The inspection model is unchanged — only the L1 transport changes.

## Implementation model

We follow rama's first-party transparent-proxy example at
`rama/ffi/apple/examples/transparent_proxy/` rather than forking
mitmproxy_rs's redirector (014 §4 considered the latter; rama's
example post-dates that analysis and is the cleaner path):

- **`crates/noodle-macos-tproxy/`** — Rust `staticlib` crate
  implementing `rama::net::apple::networkextension::tproxy::TransparentProxyHandler`.
  Linked into the system extension via the `transparent_proxy_ffi!`
  macro from rama. Contains the per-flow inspect/passthrough/block
  decisions.
- **`apps/noodle-macos/`** *(future iteration)* — Xcode project
  generated from `Project.yml` (XcodeGen). Container app + system
  extension target. Modeled on rama's `tproxy_app/`. Two bundle-ID
  pairs: `com.noodleproxy.macos.dev{,.provider}` for developer-mode
  ad-hoc signing; `com.noodleproxy.macos.dist{,.provider}` for
  Developer ID distribution.
- **CA management** — the sysext generates and stores noodle's MITM
  root CA in the macOS System Keychain on first start (the rama
  example does this via boring TLS + `security_framework`). Replaces
  the broken `make ca-trust-macos` flow.

## Iterations

Each iteration ends green on `cargo check --workspace` and (where
applicable) loads + runs on Joe's machine.

### Iteration 1 — Rust staticlib scaffold (✅ landed this branch)

`crates/noodle-macos-tproxy/` exists, depends on `rama` with the
`net-apple-networkextension` + `net-apple-xpc` features, exports
the FFI surface via `apple_ne::transparent_proxy_ffi!`. Handler is
**passthrough-only** — every claimed TCP and UDP flow is returned
to the OS without inspection. Proves the build pipeline + FFI
surface compile against rama's integration. `libnoodle_macos_tproxy.a`
is 82MB in release mode.

### Iteration 2 — Xcode app + system extension scaffold (✅ landed this branch)

`apps/noodle-macos/` mirrors rama's `tproxy_app/`: `Container/`
(Swift sources for the menu-bar container app), `Extension/`
(system-extension entry point + entitlements), `scripts/` (build /
install / notarize bash helpers), `justfile` (driver), `Project.yml`
and `Project.dist.yml` (XcodeGen specs for dev and distribution
modes). Bundle IDs: `com.noodleproxy.macos.dev{,.provider}` and
`com.noodleproxy.macos.dist{,.provider}`. Staticlib path
(`OTHER_LDFLAGS`) resolves to `target/release/libnoodle_macos_tproxy.a`
relative to the workspace root.

Install path documented in
[`docs/guides/macos-transparent-mode.md`](../guides/macos-transparent-mode.md).
Required environment: macOS 12+, Xcode signed in with any Apple ID
(free or paid), `brew install xcodegen just`, and
`NOODLE_TPROXY_DEVELOPMENT_TEAM` set to the operator's team ID.

### Iteration 3 — Wire noodle's TCP MITM into match_tcp_flow

Extract noodle-proxy's MITM service builder into a shared library
surface so both the standalone binary (HTTPS_PROXY mode) and the
sysext can construct it. `match_tcp_flow` for AI provider hostnames
→ `FlowAction::Intercept` with that service. Everything else →
`Passthrough`. End-to-end: stop `HTTPS_PROXY`, launch Claude
Desktop, traffic appears in `~/.noodle/tap.jsonl`.

Split into three sub-iterations:

- **3a — port-based filter + observability (✅ landed).**
  `crates/noodle-macos-tproxy/src/hostname_filter.rs` defines
  `should_intercept_tcp` and an `AI_PROVIDER_HOSTNAMES` constant
  for the 3b SNI matcher.
  **Important architectural finding:** in transparent mode, rama's
  `TransparentProxyFlowMeta.remote_endpoint` only exposes the
  *resolved IP* of the destination, not the hostname the app
  dialed. The macOS NE framework log shows `name = api.anthropic.com`
  separately, but the rama Swift bindings drop it.
  3a therefore filters at **L4**: claim every TCP/443 flow whose
  remote address is public (not loopback / RFC1918 / link-local).
  Iteration 3b refines via **SNI peek** inside the intercept service.
  `tracing-oslog` is wired in `init`, so `make macos-logs` shows
  every Rust `tracing::info!` event from the sysext, including
  `intercepting TLS flow` per flow.
  13 unit tests cover the predicate (IPv4/IPv6 public, non-443
  ports, loopback, RFC1918, link-local, unique-local, hostname
  paths kept for forward-proxy mode + allowlist invariants).
- **3b — TLS MITM, no inspection.** Replace `IoForwardService` with
  `TlsMitmRelay` (boring-based), mint leaf certs from a per-machine
  CA stored in the System Keychain.
- **3c — wire noodle-core inspection.** Plumb the plaintext from
  3b through the existing `InspectionEngine`, so the same
  `~/.noodle/tap.jsonl` writes happen in transparent mode as in
  forward-proxy mode.

### Iteration 4 — UDP/443 drop for AI providers (QUIC blackhole)

`match_udp_flow` for AI provider IPs/hostnames → `FlowAction::Blocked`.
Forces HTTP/3-capable clients to fall back to HTTP/2 over TCP,
which iteration 3 already inspects. Implements Option A from
014 §5.1. (Option B — real QUIC MITM — remains deferred behind a
trigger.)

### Iteration 5 — CA management via System Keychain

Sysext generates noodle's MITM root CA into the System Keychain
on first start. Retire `make ca-trust-macos`. Container app's
`Rotate CA` menu command deletes the keychain entry and restarts
the proxy (mirrors the rama example's pattern).

## Dependencies

- All MVP stories (001–006). The inspection stack is reused
  unchanged; this story replaces only the L1 transport.
- Iteration 3 depends on a noodle-proxy library refactor to expose
  the MITM service builder without the binary's runtime/CLI shell.

## Out of scope

- **Linux TPROXY.** The original 011 framing covered Linux too; we
  split it out as a future story. The decoupling is the right read
  of "L1 swap is independent of inspection" — but doing two
  platforms in one story muddies the delivery.
- **Apple Developer Account and notarization.** Iteration 2 uses
  developer-mode ad-hoc signing only. Distribution mode is a
  separate story when (a) we want to share the build with anyone
  else and (b) we own a developer account decision (Joe's identity
  vs. a noodle-specific identity).
- **Real QUIC MITM.** 014 §5 deferred this. Iteration 4 blackholes
  QUIC; full termination is a future story when an LLM vendor
  forces it.

## Notes

- The rama example uses XPC for container ↔ sysext communication.
  We'll do the same in iteration 2 — settings updates and the
  `Rotate CA` command flow over XPC.
- Filter rules in iteration 1 claim all TCP/UDP (rule:
  `TransparentProxyNetworkRule::any()`). Iteration 3 narrows to AI
  provider hostnames so non-AI traffic is never even handed to the
  sysext. Concurrency limiter from rama's example is copied verbatim
  in iteration 3.
