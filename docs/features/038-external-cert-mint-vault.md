# 038 — External `CertMintService` + Vault PKI backend + procurement

**Status:** open
**Depends on:** 036, 037
**Design refs:**
[`docs/adrs/034-enterprise-ca-and-external-signing.md`](../adrs/034-enterprise-ca-and-external-signing.md) §2.2–§2.5, §5

---

## 1. Value delivered

An enterprise can run noodle without ever placing a CA private
key on the noodle host. The proxy generates the leaf keypair
locally, sends only the CSR to an external signer (HashiCorp
Vault PKI in this story), and receives a signed leaf back. At
startup, noodle pre-mints leaves for every host in the dispatch
table so the first client connection hits a warm cache. This
unlocks deployment in regulated environments (HSM-only key
policies, no on-host long-lived secrets).

## 2. Acceptance criteria

1. Config supports `mode = "external"` with a `[ca.external]`
   block (per ADR 034 §7).
2. `ExternalCertMintService` is implemented in
   `noodle-adapters`, exposing the `CertMintService` trait from
   story 036.
3. `VaultPkiSigner` is the first concrete backend — issues a
   `POST` to the Vault PKI endpoint with the CSR, parses the
   signed cert from the response. Auth supports `token`
   (bearer) and `mtls` (client cert).
4. Single-flight dedup at the cache layer is preserved —
   concurrent requests for the same host result in exactly one
   `mint_leaf` call, same as story 036.
5. The leaf private key never crosses process boundaries. Only
   the CSR is sent to the signer. (Asserted by a test that
   inspects the outbound HTTP request body.)
6. Cert procurement runs at startup as a background task,
   iterating dispatch-table hosts and pre-minting each. Failures
   during procurement do not block proxy startup.
7. `signer_timeout` (default 2s) bounds each mint call.
   Per-call failures return `MintError::Timeout`, surfaced to the
   client as a 502 with a clear log line.
8. Successful and failed mint operations emit `AuditEvent`s
   (`leaf_minted`, `mint_failed`) through `SideEffectSink` with
   the schemas in ADR 034 §5.4.
9. Capture-driven test: with a stub Vault server returning
   signed leaves, an MITM request to `api.anthropic.com`
   completes end-to-end and the resulting leaf chains to the
   test enterprise CA.

## 3. Abstractions introduced or refined

- **New trait:** `ExternalSignerBackend` (in
  `noodle-adapters::ca::external`) — `async fn sign_csr(csr:
  CertificationRequest, ctx: SignContext) -> Result<CertChain,
  SignerError>`. Decouples `ExternalCertMintService` from any
  specific PKI protocol.
- **New adapter:** `VaultPkiSigner` implementing
  `ExternalSignerBackend` over `reqwest`.
- **New module:** `noodle-proxy::procurement` — background task
  that reads the dispatch table and calls `mint_leaf` for each
  host, populating the cache.
- **Refined:** `SideEffectSink` consumes two new `AuditKind`
  variants — `LeafMinted` and `MintFailed`.

## 4. Patterns applied

- **Strategy** — `ExternalSignerBackend` is the strategy seam
  inside `ExternalCertMintService`. Future backends (KMS, SCEP,
  webhook) plug in without changing the mint service.
- **Decorator** — the procurement task is a transparent
  pre-warmer on top of the cache + mint service; the request
  path is unchanged.

## 5. Test plan

- **Unit:** `VaultPkiSigner` builds the correct request body
  (PEM CSR, role name), parses the response, propagates errors.
- **Property:** for any well-formed `LeafRequest`, the CSR sent
  to the signer contains exactly the SAN/CN/ALPN-derived
  attributes — no leakage.
- **Integration:** spin up a wiremock Vault stub; full mint
  flow including procurement. Assert audit events emitted.
- **Capture-driven:** TLS-MITM capture against
  `api.anthropic.com` with external-signer config; client-
  visible bytes match baseline; cert chain validates against
  the test enterprise CA.
- **Fault injection:** stub returns 503 → `MintError::Timeout`
  / `SignerDenied` surfaces correctly; cached hosts unaffected.

## 6. PR scope

Likely two PRs:
- **038.a** — `ExternalCertMintService` + `VaultPkiSigner` +
  config + audit events. ~400 LOC + tests.
- **038.b** — procurement task + dispatch-table integration.
  ~200 LOC + tests.

Each independently reviewable.

## 7. Out of scope

- Additional backends (AWS ACM PCA, Azure Key Vault, SCEP/EST,
  custom webhook) → follow-up stories as customer demand
  surfaces. ADR 034 §2.4 names the catalog.
- Health-degradation on sustained mint failure → story 039.
- OCSP stapling on minted leaves → ADR 034 §9 deferred.
- Multi-CA per-cell selection → ADR 034 §9 deferred.
