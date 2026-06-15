# 023 — UDP/443 blackhole for target IPs

**Status:** open
**Depends on:** 011 iterations 1–3b (shipped on
`feat/011-transparent-mode-macos`)
**Design refs:**
[`docs/adrs/014-transparent-mode-and-quic-mitm.md`](../adrs/014-transparent-mode-and-quic-mitm.md)
§5.2 (Option B)

---

## 1. Value delivered

After this story, an operator running the noodle macOS extension
no longer loses visibility into target traffic that has cached
`Alt-Svc: h3` or a recently-seen DNS HTTPS record advertising h3.
UDP/443 datagrams to target IPs are dropped at the sysext; the
client's QUIC handshake times out within ~100ms and falls back to
TCP+TLS, which the existing TLS MITM captures. This is the
shortest path to "noodle sees the traffic" before story 024
(DNS-level suppression) lands.

## 2. Acceptance criteria

1. With the extension running and the target-IP set configured to
   include a Cloudflare-fronted h3 origin (e.g. `claude.ai`'s
   current A/AAAA), launching a client that prefers QUIC produces
   plaintext HTTP/2 traffic in noodle's wire log within 200ms of
   the first connection attempt.
2. Non-target UDP traffic (DNS over UDP/53, mDNS, NTP, WebRTC,
   QUIC to non-target IPs) is unaffected — the sysext does not
   claim those flows.
3. With the extension stopped, all UDP/443 traffic flows normally
   to the network. (The blackhole is provided by the extension,
   not by a global firewall rule.)
4. Sysext logs a single structured event per dropped UDP flow,
   keyed by destination IP and source PID, suitable for
   `frames.jsonl`-style audit.

## 3. Abstractions introduced or refined

A new `UdpFlowHandler` trait in `crates/noodle-macos-tproxy`:

```rust
pub trait UdpFlowHandler: Send + Sync {
    fn on_flow(&self, flow: UdpFlowMeta) -> UdpFlowDecision;
}

pub enum UdpFlowDecision {
    Drop,
    PassThrough,
    Forward(/* reserved for story 032 */),
}
```

Implementations in this PR:

- `DropAllHandler` — drops every claimed flow. Used in production.
- `RecordingHandler<H>` — wraps another handler and records each
  decision into a `Vec<UdpFlowEvent>` for tests.

DI seam: the Swift sysext entry point instantiates the Rust
`UdpFlowHandler` via the existing FFI macro
(`apple_ne::transparent_proxy_ffi!`). Tests substitute
`RecordingHandler<DropAllHandler>` against a `Vec<UdpFlowMeta>`
input.

## 4. Patterns applied

- **Strategy** — `UdpFlowHandler` is the strategy; `DropAllHandler`
  is this story's strategy; story 032's forwarder is the next
  strategy on the same surface.
- **Decorator** — `RecordingHandler<H>` wraps any handler without
  changing its decision, purely for observability in tests.

## 5. Test plan

- Unit: feed a sequence of `UdpFlowMeta` into
  `DropAllHandler::on_flow` and assert every result is
  `UdpFlowDecision::Drop`.
- Unit: `RecordingHandler<DropAllHandler>` records each input flow
  exactly once; the inner handler's decision is preserved
  verbatim.
- Property: for arbitrary `UdpFlowMeta` inputs (proptest), the
  handler is total — never panics, always returns a decision.
- Integration (manual on a developer machine until we have
  CI macOS runners): with the extension installed and
  `claude.ai` IPs in the target set, `curl --http3` to
  `https://claude.ai` falls back to HTTP/2 within 200ms and the
  request appears in noodle's wire log.

## 6. PR scope

One PR. ~150 lines Rust + minor Swift edit to call the new entry
point. Estimated 30-minute review.

If the Swift wiring turns out to need a new IPC message
(`UdpFlow{src,dst,pid}`) that doesn't exist yet, split:

- 023.a — Rust `UdpFlowHandler` trait + `DropAllHandler` +
  `RecordingHandler` + tests
- 023.b — Swift sysext wiring + IPC plumbing + integration test

## 7. Out of scope

- DNS-level QUIC suppression — story 024.
- Forwarding UDP flows to noodle for QUIC MITM — story 032.
- Per-origin target configuration via config file — initial
  target set is hardcoded in the Rust handler (same vendor list
  as the existing TCP filter). Config file is a separate story.
- Cached `Alt-Svc` invalidation in the client — clients clear
  their cache on their own schedule; this story does not try to
  force a cache flush.
