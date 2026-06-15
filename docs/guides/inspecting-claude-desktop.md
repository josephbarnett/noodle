# Inspecting Claude Desktop (and other Electron apps) with noodle

**Last updated:** 2026-05-12
**Applies to:** macOS 14+, Claude Desktop, VS Code, Cursor, Rancher Desktop,
and any other Electron/Chromium app that talks to LLM endpoints.

This is a runbook. Follow the steps in order, run the verification
after each, and don't skip the cleanup at the end. The "real fix"
is Story 011 — a `NETransparentProxyProvider` system extension —
which makes most of this unnecessary. Until that ships, this is
how you get a clean inspection picture.

---

## What you're up against

Three macOS-level realities work against a vanilla
`HTTPS_PROXY=http://127.0.0.1:62100`:

1. **Electron apps have at least three HTTP clients.** The main
   Node process honors `HTTPS_PROXY`. The Chromium renderer
   honors `HTTPS_PROXY` for HTTP/1.1+HTTP/2 but **switches to
   HTTP/3 (UDP) when the server advertises it via `Alt-Svc`** —
   and an HTTP `CONNECT` proxy can't carry UDP. The crashpad
   handler and auto-updater (Squirrel) use macOS-native
   networking (`NSURLSession`) which honors only the
   **system** proxy, not env vars.

2. **macOS IPv6 happy-eyeballs.** When `claude.ai` resolves to
   both A and AAAA records, the OS races them; whichever
   responds first wins. If the IPv6 route isn't proxy-aware,
   you lose that connection.

3. **Chromium's trust store on macOS is opinionated.** Since
   Chrome ~105 on Mac, Chromium uses the **Chrome Root Store**
   for built-in roots and consults the macOS keychain for
   *additional* user roots — but only when the cert has the
   right trust *policies* set. The default `security
   add-trusted-cert` invocation doesn't set them.

The solution is a layered one: fix the proxy, kill QUIC, set the
trust correctly. Each layer adds visibility; you can't skip any.

---

## Setup, end-to-end

### Prerequisites

- noodle proxy running. Either `make run` (debug) or
  `make run-release` (production-fast).
- `$HOME/.config/noodle/ca/ca.pem` exists (generated on first
  proxy boot).

### Step 1 — Install the CA into the macOS user keychain

```sh
make ca-trust-macos
```

This now uses the right flags (`-r trustRoot -p ssl -p basic`)
that Chromium/Electron requires. You'll be prompted for your
login password.

**Verify:**

```sh
security dump-trust-settings | grep -A 3 noodle
```

Expected:
```
Cert 0: noodle MITM root CA
   Result Type   : kSecTrustSettingsResultTrustRoot
   Policy OID    : SSL
   Policy OID    : Basic
```

If that's there, proceed. If not, see [Troubleshooting → CA install
failed silently](#ca-install-failed-silently).

### Step 2 — Set the system proxy (catches CFNetwork clients)

```sh
sudo networksetup -setwebproxy        Wi-Fi 127.0.0.1 62100
sudo networksetup -setsecurewebproxy  Wi-Fi 127.0.0.1 62100
sudo networksetup -setproxybypassdomains Wi-Fi 127.0.0.1 localhost
```

(Replace `Wi-Fi` with `Ethernet` if you're wired.)

This catches the macOS-native clients that Claude Desktop runs
alongside Chromium — the auto-updater, the trust daemon
(`com.apple.trustd`), and any helper binaries that use
`NSURLSession`.

**Verify:**

```sh
scutil --proxy | grep -iE 'HTTPS|HTTP[^S]|Proxy'
```

You should see both `HTTPProxy` and `HTTPSProxy` set to
`127.0.0.1:62100`.

### Step 3 — Disable IPv6 on your active interface (optional, but ends the IPv6 mystery)

```sh
networksetup -setv6off Wi-Fi
```

This forces every client to do IPv4 only — where your HTTP-proxy
expectation actually applies. Without it, "happy-eyeballs" picks
the first responder of IPv4-vs-IPv6, and you'll see TCP traffic
to `[2607:6bc0::10]:443` mysteriously bypassing noodle.

### Step 4 — Launch Claude Desktop with the right flags

**Fully quit** any running Claude Desktop instance first — Chromium
reads keychain trust at process start, so a hot reload won't pick
up Step 1.

```sh
pkill -9 -f Claude
sleep 1

HTTPS_PROXY=http://127.0.0.1:62100 \
NODE_EXTRA_CA_CERTS=$HOME/.config/noodle/ca/ca.pem \
/Applications/Claude.app/Contents/MacOS/Claude \
  --disable-quic \
  --disable-http3 \
  &
```

Three things here:

- **Direct binary launch** (not `open -W /Applications/Claude.app`).
  `open` routes through LaunchServices and `launchd`, which may
  not propagate your shell's env vars. Direct exec inherits them
  guaranteed.
- **`--disable-quic --disable-http3`** kills HTTP/3, forcing
  Chromium to fall back to HTTP/2 over TCP — which a `CONNECT`
  proxy can carry.
- **`NODE_EXTRA_CA_CERTS`** is for the Node-side main process. The
  Chromium-side renderer reads the keychain, not this env var.
  Set both anyway.

### Step 5 — Verify traffic is flowing through noodle

```sh
# Terminal A — tail decoded events live
make events-tail

# Terminal B — tail every request/response that hits the proxy
make tap-tail
```

In Claude Desktop, send a chat message. Within ~1 second you
should see:

- `tap.jsonl` lines with `host: "*.anthropic.com"` and
  `method: "POST"`.
- `events.jsonl` lines with `event: "token"` carrying the
  streamed response text.

If you see `host: "claude.ai"` and the response is a 200 with
HTML — that's just the initial app load, not the chat traffic.
Keep sending chat messages until you see the API host.

---

## What's normal noise vs what's missing

The first few minutes of capture include a lot of traffic that
**isn't** chat:

| user-agent / host | What it is | Action |
|---|---|---|
| `com.apple.trustd/3.0` to `certs.apple.com`, `cacerts.digicert.com`, `crt.sectigo.com`, `i.pki.goog`, `*.lencr.org`, `caissuers.microsoft.com` | macOS PKI trust daemon doing AIA (Authority Information Access) chain fetches for every TLS session on the box. Plain HTTP, GET, returns `application/pkix-cert` bodies. | Ignore. Filter by `provider != "unknown"` if it's noisy. |
| Electron apps (Rancher Desktop, VS Code, Cursor) doing the same AIA fetches | Same as above — every Electron app on your machine does this. | Ignore. |
| `oneocsp.microsoft.com`, `*.lencr.org` OCSP | OCSP revocation checks. Tiny POSTs. | Ignore unless investigating revocation. |
| Claude Desktop telemetry endpoints (statsig, sentry) | Crash reports + feature flag fetches. | Inspect if curious; not security-relevant unless you're auditing telemetry. |

What you want to see for chat:

- Method: `POST`
- Host: `api.anthropic.com` (or `mcp-proxy.anthropic.com` for
  MCP traffic, or `*.claude.ai` for the web-app's API)
- Response content-type: `text/event-stream`
- A matching line in `events.jsonl` with
  `event: "turn_start"` shortly after.

---

## Troubleshooting

### `ERR_CERT_AUTHORITY_INVALID` in Claude Desktop

Chromium's renderer doesn't trust the CA.

1. Confirm install:
   ```sh
   security dump-trust-settings | grep -A 3 noodle
   ```
   If empty → re-run `make ca-trust-macos` (it'll prompt for
   your password again).

2. If install succeeded but Chromium still complains, install
   into the **system** keychain instead. The Chrome Root Store
   on Mac respects system-wide admin trust:
   ```sh
   make ca-trust-macos-system
   ```
   Roll back with `make ca-untrust-macos-system`.

3. **Fully quit Claude** (Cmd-Q, then `pgrep -f Claude` returns
   nothing) and relaunch. A page refresh won't re-read trust.

### `handshake failed; SSL error code 1, net_error -202` in stderr

Same root cause as `ERR_CERT_AUTHORITY_INVALID` — Chromium
rejecting the leaf at the TLS layer. Same fix as above.

### CA install failed silently

`security add-trusted-cert` is notoriously polite about
failures. If `dump-trust-settings` doesn't show the cert after
running `make ca-trust-macos`:

1. Check the cert is parseable:
   ```sh
   openssl x509 -in ~/.config/noodle/ca/ca.pem -noout -subject
   # → subject= /CN=noodle MITM root CA/O=noodle
   ```

2. Try the install command manually so any error is visible:
   ```sh
   security add-trusted-cert \
     -r trustRoot \
     -p ssl -p basic \
     -k "$HOME/Library/Keychains/login.keychain-db" \
     "$HOME/.config/noodle/ca/ca.pem"
   ```
   A password prompt will appear; cancel it and you'll see the
   failure mode (usually "user cancelled" — which is fine, just
   re-enter your password).

3. As a last resort, double-click `~/.config/noodle/ca/ca.cer`
   in Finder → Keychain Access opens → drag to **login**
   keychain → double-click the cert → **Trust** → "When using
   this certificate" → **Always Trust**.

### Traffic still not visible after a chat message

Run this and see what hosts are being hit:

```sh
sudo tcpdump -i any -n -tt '(tcp or udp) and port 443 and host not 127.0.0.1' | head -50
```

You're looking for **any** line that's not `127.0.0.1:62100`.
If you see traffic to `*.anthropic.com` or `claude.ai` that
isn't going through localhost, it's bypassing the proxy.
Possible causes:

| What you see | Why |
|---|---|
| `quic, initial, dcid …` | HTTP/3 still active. Either `--disable-quic` flag didn't take, or something else (a sibling Electron app, Safari) is doing QUIC. Restart Claude Desktop. |
| `IP6 …:443` (raw TCP, no quic) | IPv6 bypass. Run `networksetup -setv6off Wi-Fi`. |
| `IP …:443` (IPv4 TCP, but not to 127.0.0.1) | A client that doesn't honor env-var OR system proxy. Look at the source IP with `lsof`:
```sh
sudo lsof -i TCP -P -n | grep -E ':443.*ESTABLISHED'
``` |

### Chat works in one Electron app but not another

Each Electron app's main process has its own env. `HTTPS_PROXY`
is inherited only by whatever you launched from your terminal.

If you started Claude from `open -W` or from the macOS Dock,
the env isn't inherited. Always launch from the terminal with
explicit env (Step 4 above).

System proxy + keychain trust apply to all apps on the box, so
once those are set, an `open -W` launch will partially work —
but the env-based `NODE_EXTRA_CA_CERTS` won't propagate, which
breaks the Node side of the app.

### After capture: I want my machine back

```sh
# Restore IPv6:
networksetup -setv6automatic Wi-Fi

# Clear the system proxy:
sudo networksetup -setwebproxystate       Wi-Fi off
sudo networksetup -setsecurewebproxystate Wi-Fi off

# Remove the CA trust (do BOTH if you installed system-wide):
make ca-untrust-macos
make ca-untrust-macos-system   # if applicable

# Confirm system proxy is gone:
scutil --proxy | grep -iE 'HTTP|Proxy'   # should be empty
```

---

## What's not covered here (and where to go)

- **iOS / iPadOS clients** — completely different trust story
  (MDM profile install). Out of scope.
- **MCP servers over stdio / unix-socket** — bypass HTTP
  entirely. Not visible to a proxy; instrument the MCP server
  side directly.
- **WebSocket / `wss://` traffic** — traverses `CONNECT` so it
  appears in `tap.jsonl` as one long-lived row with status 101,
  but the codec layer (story 020) doesn't decode WS framing
  today. Story 009 (WebSocket adapter) is the proper home.
- **Other browsers (Safari, Firefox)** — Safari uses macOS
  system proxy + system trust, so Steps 1+2 cover it. Firefox
  has its own trust store; you'd have to import the CA into
  Firefox's preferences separately.

---

## The real fix (Story 011 — transparent mode)

Every workaround above exists because the proxy lives at L7
(HTTP) when the apps decide whether to use it at L4 (per
connection). A `NETransparentProxyProvider` system extension
intercepts at L3 — before the kernel chooses UDP vs TCP, before
the app decides whether to honor `HTTPS_PROXY`, before
happy-eyeballs picks IPv4 vs IPv6. Everything funnels through.

That's a chunky piece of work — code-signed system extension,
packaged installer, entitlements, App Sandbox interactions. The
guide above is the workaround until that ships. See
[`docs/features/011-transparent-mode.md`](../features/011-transparent-mode.md)
for the future story; see
[`docs/adrs/011-tls-mitm-and-ca.md`](../adrs/011-tls-mitm-and-ca.md)
for the current TLS-MITM mechanics.
