# 045 — Shipper OTLP/gRPC transport

**Status:** open
**Depends on:** 043 (noodle-shipper) — shipped (PR #95).
**Design refs:**
[`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md)
(downstream collector contract).

---

## 1. Value delivered

After this story ships, operators whose downstream OTel collector
exposes only the gRPC OTLP receiver can point `noodle-shipper` at it
directly — no `otlphttp` sidecar deployment required. Closes one of
the two operational gaps the 043 runbook flagged.

The current shipper emits OTLP/HTTP only. Most operators we
encounter run a collector with both receivers enabled; the gap is
real only for the subset that runs gRPC-only (often dictated by
existing infrastructure conventions or auth-edge constraints).

## 2. Acceptance criteria

1. The shipper config gains a `transport: "http" | "grpc"` field
   (default `http` for backwards compatibility).
2. With `transport: grpc`, the shipper opens a long-lived gRPC
   connection to the configured collector endpoint and emits OTLP
   Logs via the `ExportLogsServiceRequest` RPC.
3. The cursor-on-flag state machine (043 §3) behaves identically
   across transports — `pending → in_flight → delivered | retry →
   poison` semantics unchanged.
4. Exponential backoff (043) applies symmetrically to gRPC
   transient errors (UNAVAILABLE, DEADLINE_EXCEEDED).
5. Tests: integration against an in-process gRPC mock collector
   (mirror of the wiremock-style HTTP harness from E4).
6. Runbook updated: `docs/guides/shipper-runbook.md` drops the
   "HTTP/JSON only" caveat.

## 3. Abstractions introduced or refined

- **`OtlpTransport`** (new): enum `{ Http, Grpc }`. Today the
  shipper hand-rolls OTLP/HTTP JSON; this story either:
  - (a) adopts `opentelemetry-otlp` as a dep and routes both
    transports through its client, or
  - (b) keeps the hand-rolled HTTP path and adds a parallel
    `tonic`-based gRPC path.

  (a) is smaller code-wise but pulls a non-trivial dep tree;
  (b) is more code but keeps deps lean. Pick at implementation.

## 4. Patterns applied

- **Strategy** — the transport is a runtime-selected strategy
  behind a uniform `ExporterClient` trait so the cursor/retry
  state machine doesn't branch on transport.

## 5. Test plan

- Unit: cursor state transitions per-transport, including the
  retry/poison thresholds.
- Integration: in-process gRPC mock collector + the existing HTTP
  wiremock receiver from E4. Both exercises feed the same shipper
  binary configured with the relevant `transport`.
- Backward-compat: existing HTTP integration tests still pass
  unchanged when `transport: http` (the default).

## 6. Out of scope

- Auth headers — [`046`](046-shipper-otlp-auth-headers.md).
- Compression — separate follow-up; not blocking any known
  operator.

## 7. Recommended starting slice

Pick the dep strategy (`opentelemetry-otlp` vs hand-rolled `tonic`)
first — that decision drives most of the shape. After the strategy,
slice as: transport trait + HTTP-path move → gRPC client → tests →
runbook.
