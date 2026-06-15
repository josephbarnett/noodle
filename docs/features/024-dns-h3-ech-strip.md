# 024 — `NEDNSProxyProvider` Swift extension + sysext wiring

**Status:** open
**Depends on:** 011 iterations 1–3b, 023 (blackhole as a
belt-and-braces fallback while this DNS path stabilises),
**done/027 — the Rust core (`DnsWireCodec` + `Transform<DnsMessage>`
implementations of `StripH3` / `StripEch`) shipped via commits
`5ba4d5d`, `ab7dc19`, `ec4876b`; see
[`done/027-dns-wire-codec.md`](done/027-dns-wire-codec.md)**
**Design refs:**
[`docs/adrs/014-transparent-mode-and-quic-mitm.md`](../adrs/014-transparent-mode-and-quic-mitm.md)
§5.1 (Option A) + §10 (DNS interception infrastructure),
[`docs/adrs/015-layered-codec-architecture.md`](../adrs/015-layered-codec-architecture.md)
§11 step 2

> **Scope note (2026-05-14, refreshed 2026-05-17).** The ad-hoc
> `DnsResponseRewriter` Rust trait this story originally proposed
> has been *replaced* by the layered codec architecture per 015
> §11 step 2. The DNS rewriting logic now lives in
> `noodle-adapters` as `DnsWireCodec` +
> `Transform<DnsMessage>` — built in **story 027 (shipped, see
> [`done/027-dns-wire-codec.md`](done/027-dns-wire-codec.md))**,
> the first real exercise of the new `Codec` + `Transform`
> traits. This story (024) is reduced to its platform-integration
> work: the Swift `NEDNSProxyProvider` extension, the sysext
> wiring, and the FFI bridge from the Swift provider to 027's
> shipped Rust core.

---

## 1. Value delivered

After this story, target origins (`claude.ai`,
Cloudflare-fronted AI vendors) are *surgically* prevented from
using QUIC. The DNS HTTPS record returned to the system has its
`alpn=h3` parameter stripped and its `ech=` parameter removed
entirely; A/AAAA, `ipv4hint`, `port`, and other parameters are
preserved verbatim. The client never learns h3 is available, never
attempts QUIC, and never has ECH-protected SNI to hide behind —
so the existing TLS MITM has clean SNI to mint a leaf certificate.
Operator experience: no failed-handshake RTT (unlike story 023),
no error dialogs, no degraded UX.

## 2. Acceptance criteria

1. With the DNS proxy extension installed and approved, querying
   `dig +short TYPE65 claude.ai` returns an HTTPS record whose
   ALPN field contains `h2` but not `h3`, and which carries no
   `ech=` parameter.
2. A/AAAA records for the same query are unmodified (verified by
   diffing against `dig @8.8.8.8 +short A claude.ai`).
3. Non-target origins return HTTPS records identical to the
   upstream response — no rewriting, no parameter loss, no TTL
   change beyond what the local resolver naturally applies.
4. A target origin with no `alpn=h3` in its upstream response
   passes through unchanged (the rewriter is idempotent on
   already-clean responses).
5. With the DNS proxy extension stopped, all DNS resolution
   reverts to the system default.
6. Each rewritten response is recorded in a structured audit
   event (origin, original ALPN, rewritten ALPN, ECH-present-bool)
   suitable for `events.jsonl`.

## 3. Abstractions introduced or refined

> **Sections 3–5 below describe the original ad-hoc Rust-trait
> approach** (`DnsResponseRewriter` in
> `crates/noodle-macos-tproxy`). That trait is **superseded** by
> story 027's `DnsWireCodec` + `Transform<DnsMessage>` impls in
> `noodle-adapters` (**shipped — see
> [`done/027-dns-wire-codec.md`](done/027-dns-wire-codec.md)**).
> This story (024) retains only the Swift `NEDNSProxyProvider`
> extension, sysext wiring, FFI bridge to 027's Rust core, and
> the integration test that exercises the end-to-end path
> through real DNS.
>
> The historical content below is preserved as a record of the
> design intent that drove the shape of the shipped 027 codec.

A `DnsResponseRewriter` trait in `crates/noodle-macos-tproxy`:

```rust
pub trait DnsResponseRewriter: Send + Sync {
    fn rewrite(&self, query: &DnsQuery, response: DnsResponse)
        -> DnsResponse;
}
```

Implementations in this PR:

- `Identity` — returns the response untouched. Default for
  non-target origins.
- `StripH3` — removes `h3` (and `h3-29`, `h3-Q050`, …) from the
  ALPN value list of HTTPS records. Preserves all other fields.
- `StripEch` — removes the `ech` SvcParamKey from HTTPS records.
- `Composite<I: IntoIterator<Item = Box<dyn DnsResponseRewriter>>>`
  — runs multiple rewriters in order. Used in production as
  `Composite::new([StripH3, StripEch])`.
- `RecordingRewriter<R>` — wraps any rewriter and records each
  rewrite event into an audit channel.

Routing: a small `OriginRouter` selects which rewriter applies to
which query — target origins get `Composite::new([StripH3,
StripEch])`; everything else gets `Identity`.

DI seam: the Swift `NEDNSProxyProvider` instantiates the Rust
handler via the FFI macro with the configured rewriter chain.
Tests substitute `RecordingRewriter<…>` and feed hand-crafted DNS
responses through it without any Swift or NEDNSProxyProvider in
the loop.

## 4. Patterns applied

- **Chain of Responsibility** — `Composite` runs rewriters in
  order; each may modify the response and pass it on.
- **Strategy** — `OriginRouter` selects the rewriter strategy
  per-origin.
- **Composite** — `Composite` is itself a `DnsResponseRewriter`,
  so chains of chains compose without special-case logic.
- **Decorator** — `RecordingRewriter<R>` adds audit observation
  to any rewriter without changing its decision.

## 5. Test plan

- Unit: feed hand-crafted DNS response bytes (HTTPS record with
  `alpn=h3,h2 ipv4hint=… ech=…`) through `StripH3` — assert ALPN
  becomes `h2`, all other params preserved byte-for-byte.
- Unit: `StripEch` on a response with `ech=` removes the param;
  on a response without `ech=` the response is byte-identical.
- Unit: `Composite::new([StripH3, StripEch])` produces the
  expected combined output regardless of input ordering.
- Unit: `Identity` on arbitrary input returns the exact bytes
  back.
- Property (proptest): for arbitrary DNS responses,
  `StripH3` followed by another `StripH3` is idempotent;
  `Identity` is a left- and right-identity in the `Composite`.
- Integration: install the DNS proxy extension, run `dig` for a
  target origin and a non-target origin, compare against `dig
  @8.8.8.8` baseline.

## 6. PR scope

Likely two PRs:

- **024.a — Rust rewriter trait + impls + tests.** The
  `DnsResponseRewriter` trait, `Identity` / `StripH3` /
  `StripEch` / `Composite` / `RecordingRewriter`,
  `OriginRouter`, unit + property tests. ~400 lines Rust.
  Mergeable independently because it can be exercised with
  hand-crafted DNS bytes — no extension needed.
- **024.b — Swift `NEDNSProxyProvider` extension + FFI plumbing
  + integration tests.** New sysext target in
  `apps/noodle-macos/`, FFI entry point, manual install/approve
  flow, `dig` integration test. ~300 lines Swift + ~50 lines
  Rust glue.

Splitting this way means 024.a lands behind tests and is
reviewable in 30 minutes; 024.b carries the platform-integration
risk and gets its own review focus.

## 7. Out of scope

- **DoH bypass mitigations** (per 014 §5.1, mitigation 2 — block
  known DoH endpoints at the transparent proxy). Separate story
  024.c, defaulted off until empirical evidence a target client
  uses DoH.
- **DNS-over-TLS / DNS-over-QUIC bypass mitigations.** Same
  rationale; separate story when needed.
- **Config-driven target-origin list.** This PR hardcodes the
  same vendor list used by the TCP filter. A config file
  (`~/.config/noodle/intercept.toml` or similar) is a separate
  story shared with the TCP filter.
- **Forwarding QUIC flows to noodle for MITM** — story 032.
- **Modifying any DNS record type other than HTTPS (TYPE65).**
  A/AAAA/SVCB/MX/TXT all pass through unmodified.
