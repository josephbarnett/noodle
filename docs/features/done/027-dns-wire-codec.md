# 027 — DNS wire codec + h3/ech transforms

**Status:** done (retrospective — work shipped without an
upfront story file; file written 2026-05-17 to close the gap)
**Depends on:** 026 (`Codec` + `Transform` trait surface)
**Design refs:**
[`docs/adrs/014-transparent-mode-and-quic-mitm.md`](../../adrs/014-transparent-mode-and-quic-mitm.md)
§5.1 (Option A — DNS h3/ech strip) + §10 (DNS interception),
[`docs/adrs/015-layered-codec-architecture.md`](../../adrs/015-layered-codec-architecture.md)
§3, §6 (DNS as a sibling L2 branch, not a sidecar)

> **Retrospective note.** The DNS-codec work was tracked in
> commit messages and source comments under "story 027" (sub-
> stages 027.a / 027.b / 027.c) but no story file was ever
> written. This file records what shipped so the references in
> `features/024-dns-h3-ech-strip.md` and in
> `crates/noodle-adapters/src/dns/` source comments point at a
> real, completed story. The originally-numbered-027 story
> (embellishment add-on) moved to `028` to free the slot.

---

## 1. Value delivered

After this story, noodle's layered codec stack has a **first-class
L2 sibling for DNS**: raw UDP/53 datagrams decode to a typed
`DnsMessage`, transforms operate on the structured message, and
the message re-encodes to bytes round-trip-faithfully. The two
transforms `StripH3` and `StripEch` give the proxy the building
blocks to surgically prevent target origins from negotiating
QUIC (strip `alpn=h3` from HTTPS RRs) and from hiding their SNI
(strip `ech=` SvcParam), without touching A/AAAA or any other
record. This is the Rust core that story 024 wires onto a
`NEDNSProxyProvider` Swift extension; it is also the first real
exercise of ADR 015's premise that DNS is a parallel L2 branch
in the codec stack rather than a separate subsystem.

## 2. Acceptance criteria

All met by the shipped code in `crates/noodle-adapters/src/dns/`:

1. `DnsMessage`, `DnsHeader`, `DnsFlags`, `DnsQuestion`,
   `DnsRecord`, `DnsRecordType`, `DnsClass`, `DnsName`,
   `DnsRecordData`, `HttpsRecord`, `SvcParam`, `SvcParamKey`,
   `SvcParamValue` model the wire-level structure of a DNS
   message, including TYPE65 HTTPS records and SvcParam values.
2. `DnsWireCodec: Codec<Input = Bytes, Output = DnsMessage>`
   decodes a single datagram to a `DnsMessage` and re-encodes
   a `DnsMessage` to bytes; the round-trip invariant
   (`encode(decode(bytes)) == bytes` for unmutated input) is
   enforced by tests.
3. `StripH3: Transform<Event = DnsMessage>` removes the `alpn`
   SvcParam value `h3` (and `h3-NN` draft variants) from any
   HTTPS RR in the message; if the resulting alpn list is empty,
   the SvcParam is dropped. A/AAAA and other RRs are unchanged.
4. `StripEch: Transform<Event = DnsMessage>` removes the entire
   `ech` SvcParam from any HTTPS RR in the message. Other
   SvcParams (`alpn`, `port`, `ipv4hint`, `ipv6hint`, …) are
   unchanged.
5. Both transforms are idempotent on messages that already lack
   the targeted parameter.
6. The codec emits `AuditKind::Errored` on the side channel for
   malformed datagrams and returns empty (ADR 015 §16
   empty-on-error contract); never panics.
7. Unit tests cover the wire-decoder edge cases (compressed
   names, truncated input, unknown record types) and the
   transform behaviour (HTTPS-RR presence/absence, multi-RR
   messages, mixed-record messages).

## 3. Abstractions introduced or refined

- **`DnsWireCodec`** — concrete `Codec` impl realising ADR 015's
  "DNS is a sibling L2 branch" claim. Sits next to `HttpH1Codec`
  / `HttpH2Codec` at L2, but emits `DnsMessage` instead of
  `BodyFrameEvent`.
- **`DnsMessage` + supporting types** — the L3-equivalent
  structured value for the DNS branch. Parallel to
  `HttpRequest`/`HttpResponse` for the HTTP branch.
- **`StripH3`, `StripEch`** — `Transform<DnsMessage>` impls. First
  use of the `Transform` trait on a non-`BodyFrameEvent` /
  non-`NormalizedEvent` payload type, proving the trait
  generalises across the codec stack.

## 4. Patterns applied

- **Strategy** — `StripH3` and `StripEch` are independent
  transforms composed in registration order; the dispatcher
  picks them per cell.
- **Composite** — transforms layer naturally (both can run on
  the same message) by virtue of the `TransformRegistry` chain.
- **Round-trip codec** — `Codec::decode` + `Codec::encode` as a
  symmetric pair; same invariant as the HTTP and SSE codecs.

## 5. Test plan (shipped)

- Unit tests in `crates/noodle-adapters/src/dns/wire.rs`,
  `dns/codec.rs`, `dns/transforms.rs` cover decode, encode,
  round-trip, error paths, and transform behaviour. ~68 test
  functions across the four files.

## 6. PR scope

Three sub-PRs landed (commits in `git log`):

- **027.a** (`5ba4d5d`) — `DnsMessage` + wire codec for HTTPS RR
  (`feat(noodle-adapters): add DnsMessage + wire codec for HTTPS
  RR`)
- **027.b** (`ab7dc19`) — `DnsWireCodec` implements `Codec`
  (`feat(noodle-adapters): DnsWireCodec implements Codec trait`)
- **027.c** (`ec4876b`) — `StripH3` + `StripEch` transforms
  (`feat(noodle-adapters): StripH3 + StripEch Transform<DnsMessage>`)

## 7. Out of scope

- The Swift `NEDNSProxyProvider` extension and the FFI bridge
  from the provider to this Rust core — these are story 024.
- DNS dispatch in the engine (selecting `DnsWireCodec` per cell
  in the `(domain address, endpoint, direction)` key space) —
  the DNS path runs at L2; engine dispatch will pick it up when
  the transparent-mode track wires DNS flows in (story 013 / the
  sysext UDP path).
- Other SvcParam manipulation (forcing IPv4 only, rewriting
  `port`) — out of scope; the transform surface is open if a
  future story needs them.
