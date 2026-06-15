# 046 — Shipper OTLP auth headers

**Status:** open
**Depends on:** 043 (noodle-shipper) — shipped (PR #95).
**Design refs:**
[`docs/adrs/022-otel-collector-embellishment-plane.md`](../adrs/022-otel-collector-embellishment-plane.md).

---

## 1. Value delivered

After this story ships, operators whose downstream OTel collector
sits behind any auth boundary — Bearer tokens, API keys in custom
headers, mTLS client certs — can point `noodle-shipper` at it
directly without a local nginx/envoy sidecar terminating auth.
Closes the second of the two operational gaps the 043 runbook
flagged.

## 2. Acceptance criteria

1. The shipper config gains an `auth_headers` map (string → string)
   that the shipper attaches to every outbound OTLP request.
2. Header values are read once at config-load. Rotation is
   delivered via a SIGHUP-driven re-read or process restart;
   per-request token minting is **out of scope** here.
3. Sensitive header values never appear in logs, error messages,
   or PR/runbook text. The shipper's existing `tracing` config
   already redacts header values; this story extends that
   guarantee to the new field.
4. Tests: integration against the wiremock receiver from E4 with
   a required-header policy; the shipper succeeds with the header
   present and 401-loops-into-retry-then-poison without it.
5. The runbook drops the "No auth" caveat and gains a short
   "configuring auth headers" section.

## 3. Abstractions introduced or refined

- **`auth_headers: HashMap<String, String>`** on the shipper
  config. No new traits; the existing OTLP client already accepts
  arbitrary headers per request.
- **Secret-handling discipline**: rely on the runtime's existing
  `tracing` field-redaction for `auth_headers.*` — do not add a
  bespoke wrapper type. If the redaction story drifts, file a
  separate observability story.

## 4. Patterns applied

- **Composition** — auth is a header map layered onto the existing
  request shape, not a transport-level concern.

## 5. Test plan

- Unit: header map round-trips through config load → exporter init
  → request build, redacted in tracing output.
- Integration: wiremock receiver requires header; assert
  401-without / 200-with behavior and the retry/poison transitions.

## 6. Out of scope

- **mTLS client certs.** TLS-layer auth is a transport concern; if
  needed it gets its own story. This one is HTTP/gRPC metadata
  headers.
- **Token rotation / minting.** v1 reads on boot. Hot-reload via
  SIGHUP is a small follow-up if operationally needed.

## 7. Recommended starting slice

Land the config field + redaction guarantee in one PR with the
wiremock integration test. Then update the runbook.
