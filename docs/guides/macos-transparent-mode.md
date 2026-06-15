# macOS transparent mode — install + drive

How to build, install, and run noodle's macOS Network Extension on
a developer machine. Iteration 2 of [Story
011](../features/011-transparent-mode.md).

> **What this does today (iteration 2).** The system extension loads,
> gets approved by macOS, and runs — but the handler is
> **passthrough-only**. No traffic is intercepted yet; every flow
> goes through untouched. The point of this iteration is to prove
> the build + install pipeline. Real inspection lands in iteration 3.

## TL;DR

```sh
make macos-tooling      # once: brew install xcodegen + just
make macos-doctor       # confirm prerequisites
make macos-install      # build + sign + install + launch
# Approve in: System Settings → General → Login Items & Extensions → Network Extensions
make macos-list         # expect: [activated enabled] com.noodleproxy.macos.dev.provider
```

The team ID defaults to Joe's (`KRU5V3NCWA`). Override at the command
line if you're someone else: `make macos-install NOODLE_TPROXY_DEVELOPMENT_TEAM=YOURID`.

## Prerequisites

- macOS 12+ (deployment target in `apps/noodle-macos/Project.yml`).
- Xcode 15+ installed, signed in under `Xcode → Settings → Accounts`
  with an Apple ID that's a member of the team you're building for.
  Any Apple ID — free or paid — works for dev mode.
- `make macos-tooling` runs `brew install xcodegen just`.
- `rama` checked out as a sibling of `noodle`. Path resolution in
  `apps/noodle-macos/Project.yml` assumes `../../../rama`.

The first build triggers Xcode's automatic-signing flow with
`-allowProvisioningUpdates`. Xcode registers the App IDs
(`com.noodleproxy.macos.dev`, `com.noodleproxy.macos.dev.provider`)
and the App Group (`KRU5V3NCWA.com.noodleproxy.macos.dev`) in the
Apple Developer portal **automatically** — you do not need to create
anything manually.

## Make targets

| Target | What it does |
|---|---|
| `make macos-tooling` | `brew install xcodegen just` (one-time). |
| `make macos-doctor` | Lists tooling + codesigning identities; verify before install. |
| `make macos-staticlib` | Build the Rust staticlib only (`cargo build --release -p noodle-macos-tproxy`). |
| `make macos-build` | Build the full Xcode app + sysext (dev mode). |
| `make macos-open-xcode` | Generate + open the project in Xcode — use when CLI signing fails and you need to drive auto-provisioning interactively. |
| `make macos-install` | Build, install to `/Applications/`, launch. First-time install prompts for system-extension approval. |
| `make macos-install-reset` | Same as install, but also recreates the saved `NETransparentProxyManager` profile (needed after entitlement / `Info.plist` / `NEMachServiceName` changes). |
| `make macos-list` | Show live system-extension state. |
| `make macos-logs` | Stream logs from the extension + macOS NE daemons. |
| `make macos-logs-recent` | Last 5 minutes of logs. |
| `make macos-uninstall` | Prints the in-app uninstall instructions. Uninstall is **in-app only** — the container app is the only entity allowed to deactivate the sysext under SIP. See **Uninstall** below. |
| `make macos-clean` | Wipe `.xcode-derived/` + the generated `.xcodeproj`. Use when signing gets stuck on a stale cache. |

## First-time install flow

```sh
make macos-install
```

macOS will prompt:

> System Extension Blocked. Open System Settings to approve.

Open `System Settings → General → Login Items & Extensions → Network
Extensions`, toggle noodle on. Confirm:

```sh
make macos-list
# [activated enabled]   com.noodleproxy.macos.dev.provider (0.1/...)
```

## Trust env vars for HTTPS clients (iteration 3b)

Once the sysext is running, the proxy MITMs `api.anthropic.com` and
other AI provider hostnames (see `AI_PROVIDER_HOSTNAMES` in
`crates/noodle-macos-tproxy/src/hostname_filter.rs`). The leaf certs
it mints are signed by a self-signed root CA generated at sysext
startup. HTTPS clients that use the macOS Keychain trust store
inherit nothing from us today (iteration 5 lands Keychain install).
Clients with their own trust store — Node.js, Electron apps, Python
`requests`, Go binaries, curl with its own bundle — need to be
pointed at the CA explicitly via env vars.

### Where the CA lives

```sh
make macos-ca-path
# /Library/Application Support/noodle/macos-tproxy-ca.pem
```

System-wide path with mode `0644` (world-readable). Hardcoded in
`crates/noodle-macos-tproxy/src/tls.rs::CA_PEM_PATH` because the
storage-dir the rama Swift bindings hand the sysext at init lives
under `/var/root/` (mode `0700`) — `NETransparentProxyProvider`
extensions run as root, and `userDomainMask` from inside their
sandbox resolves to root's home, which is unreadable by any other
user. The sysext (as root) creates this dir and chmods both the
dir (`0755`) and the file (`0644`) on every startup.

### Confirm the CA is on disk and valid

```sh
ls -la "$(make macos-ca-path)"
openssl x509 -in "$(make macos-ca-path)" -noout -subject -issuer
# expect:
#   subject= /CN=ca.noodleproxy.macos/O=noodle MITM root CA
#   issuer=  /CN=ca.noodleproxy.macos/O=noodle MITM root CA   (self-signed)
```

### Set the env vars — two ways

**From the menu bar.** Click 🍝 → **Set Trust Env Vars
(launchctl setenv)**. Shows an NSAlert confirming what was set.
Internally runs `launchctl setenv NAME <path>` for each of:

- `NODE_EXTRA_CA_CERTS` — Node.js, Electron, npm, Claude Code itself
- `REQUESTS_CA_BUNDLE` — Python `requests`
- `SSL_CERT_FILE` — OpenSSL-linked tools, Go
- `CURL_CA_BUNDLE` — curl
- `AWS_CA_BUNDLE` — aws-cli

**From the shell** (equivalent):

```sh
make macos-env-export
```

### Caveats

- `launchctl setenv` only affects processes that **start after**
  the call. Already-running processes do **not** inherit. Relaunch
  apps you want to use the trust.
- The setenv values last until you log out. Iteration 5 will write
  a LaunchAgent plist for persistence + register the CA with the
  System Keychain.
- `launchctl setenv` per-user; doesn't affect root daemons.

### Iteration 3b smoke test

After install + setenv:

```sh
# Confirm the sysext is claiming AI provider flows:
make macos-list
# expect: [activated enabled] com.noodleproxy.macos.dev.provider

# Confirm CA on disk:
openssl x509 -in "$(make macos-ca-path)" -noout -subject

# 1) curl --insecure should always work — MITM is invisible to curl
#    when verification is disabled:
curl -sS --insecure -w 'HTTP %{http_code}\n' https://api.anthropic.com/
# expect: HTTP 404

# 2) curl WITH verification (no --insecure) after env vars are set:
#    new terminal session inherits CURL_CA_BUNDLE from the launchctl
#    setenv. Should succeed.
curl -sS -w 'HTTP %{http_code}\n' https://api.anthropic.com/
# expect: HTTP 404 (real validation succeeds via noodle's CA)

# 3) Non-AI HTTPS should pass through unchanged — REAL upstream cert:
curl -sS -w 'HTTP %{http_code}\n' https://api.github.com/
# expect: HTTP 200, no cert errors even without --insecure / env vars

# 4) Logs should show the SNI-based decision:
make macos-logs-recent | grep -E 'allowlist hit|allowlist miss|MITM'
# expect:
#   "MITM allowlist hit — terminating TLS"  sni=api.anthropic.com
#   "MITM allowlist miss — transparent tunnel"  sni=api.github.com
```

### ⚠️ Risk: Claude Code itself is a Node app

This Claude Code CLI session dials `api.anthropic.com` to talk to
Anthropic. The sysext MITMs that. If you `make macos-env-export`
and **relaunch Claude Code**, the new session will inherit
`NODE_EXTRA_CA_CERTS` and route through the MITM. If anything in
the chain misbehaves (cert chain off, handshake fails) the new
session can't reach Anthropic.

Recommended order when verifying for the first time:

1. **Don't relaunch the current Claude Code session yet.**
2. Test with curl (steps 1–4 above) in a different terminal.
3. If curl with real validation succeeds end-to-end, the MITM is
   sound and relaunching Node apps becomes safe.
4. Save the Claude Code relaunch for last (or test from a separate
   Claude Code session you can abandon if it breaks).

Recovery if a Node app stops reaching Anthropic:

```sh
# Either unset the env vars and relaunch:
launchctl unsetenv NODE_EXTRA_CA_CERTS
launchctl unsetenv REQUESTS_CA_BUNDLE
launchctl unsetenv SSL_CERT_FILE
launchctl unsetenv CURL_CA_BUNDLE
launchctl unsetenv AWS_CA_BUNDLE

# Or fully uninstall noodle from the menu bar:
# 🍝 → Uninstall Noodle…
```

## Uninstall

Uninstall is **in-app only**. The container app that activated the
system extension is the only entity allowed to deactivate it under
SIP — there is no supported CLI path.

From the menu bar (`🦙 tproxy demo`) → **Uninstall Noodle…**.
Steps the app performs, in order:

1. Stop the proxy.
2. Load and remove every saved `NETransparentProxyManager` profile
   (clears the row in System Settings → Network → VPN & Filters).
3. Clear the MITM root CA from the System Keychain.
4. Clear the app's `UserDefaults` (standard + app-group suite).
5. Submit `OSSystemExtensionRequest.deactivationRequest` — only the
   container app that activated the sysext can deactivate it.
6. Move `/Applications/Noodle.app` to the Trash via
   `NSWorkspace.recycle`, then quit.

After completion: `systemextensionsctl list` shows no noodle
extension, the app icon is gone from the menu bar, the bundle is in
the Trash, and System Settings → Network → VPN & Filters no longer
lists "Noodle Proxy."

**If the app won't launch at all** and you need a hard-reset, the
only supported path is to reboot and then `rm -rf /Applications/Noodle.app`.
That leaves the orphan `NETransparentProxyManager` profile in the UI;
remove that row with the `−` button under VPN & Filters.

## Troubleshooting

### Build fails with provisioning profile UUID mismatch

```
error: unable to read input file '/Users/.../Provisioning Profiles/<uuid>.provisionprofile': No such file or directory
```

This happens the first time Xcode auto-creates an App ID — the build
cache references an expected profile UUID that doesn't yet exist.
Fix:

```sh
make macos-clean
make macos-install
```

If it still fails, drive the signing interactively once via Xcode:

```sh
make macos-open-xcode
```

In Xcode, select each target (Container + Extension), go to
**Signing & Capabilities**, confirm Team is selected, hit ⌘B. Once
both build green from inside Xcode, drop back to `make macos-install`.

### `NEVPNConnectionErrorDomainPlugin code=6` on launch

Usually means the sysext registration is stale or the provider
crashed last run. Try in order:

```sh
make macos-install              # re-register
make macos-install-reset        # also recreates the saved NE profile
ls -lt /Library/Logs/DiagnosticReports/ | grep com.noodleproxy.macos.dev.provider | head -5
```

Rama's example (`rama/ffi/apple/examples/transparent_proxy/README.md`)
documents a more comprehensive decision tree — most of it applies
verbatim since we share the same Apple plumbing.

### Codesigning identity confusion

`security find-identity -p codesigning -v` may list multiple Apple
Development certs for different teams. The team ID Xcode actually
uses for the build comes from `NOODLE_TPROXY_DEVELOPMENT_TEAM`, not
from the cert name. As long as the team is in your `Xcode → Settings
→ Accounts`, Xcode will fetch / create the right cert on demand.

## Limits today (will change in later iterations)

- **No inspection.** Handler returns `Passthrough` for every flow.
- **Host-arch staticlib only.** Universal arm64+x86_64 build via
  `lipo` is deferred until distribution mode.
- **Dev mode only.** Distribution mode (`Developer ID` signing +
  notarization) is scaffolded but unexercised.
- **Hardcoded `TransparentProxyNetworkRule::any()`.** The sysext
  claims every TCP and UDP flow on the machine; we pass them all
  through. Iteration 3 narrows rules to AI provider hostnames so
  non-AI traffic skips the sysext entirely.

## Bypass: direct just usage

The make targets above call the `justfile` at `apps/noodle-macos/`
under the hood. If you want to invoke `just` directly:

```sh
cd apps/noodle-macos
export NOODLE_TPROXY_DEVELOPMENT_TEAM=KRU5V3NCWA
just install-dev
```

All `make macos-*` targets have `just` equivalents; the make wrapper
just spares you the `cd` + env-var dance.
