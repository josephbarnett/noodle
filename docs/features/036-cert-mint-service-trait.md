# 036 — `CertMintService` trait + `LocalCertMintService` extraction

**Status:** open
**Depends on:** (none)
**Design refs:**
[`docs/adrs/034-enterprise-ca-and-external-signing.md`](../adrs/034-enterprise-ca-and-external-signing.md) §2.2,
[`docs/adrs/011-tls-mitm-and-noodle-root-ca.md`](../adrs/011-tls-mitm-and-noodle-root-ca.md)

---

## 1. Value delivered

The MITM cert-minting path is decoupled from rama's
`InMemoryBoringMitmCertIssuer` and sits behind a noodle-owned
`CertMintService` trait. Behaviour is unchanged for the existing
local-CA mode — a leaf is generated and signed in-process — but
the seam is in place so subsequent stories (BYOCA-static,
external-signer, rip-cord) can land without further refactoring
of the hot path. Operators see no behaviour change; engineers
can now substitute a fake mint service in tests without spinning
up the full TLS stack.

## 2. Acceptance criteria

1. A `CertMintService` trait is defined in `noodle-core` with
   `async fn mint_leaf(&self, LeafRequest) -> Result<LeafCert,
   MintError>`. Trait surface matches ADR 034 §2.2 exactly.
2. `LocalCertMintService` is implemented in `noodle-adapters` (or
   `noodle-proxy` if rama types are required at the boundary),
   wrapping the existing `rcgen`-based in-process signer.
3. `noodle-proxy` constructs the rama MITM stack using
   `LocalCertMintService` via a `CertIssuer` adapter — the
   existing `CachedBoringMitmCertIssuer` cache layer wraps the
   noodle service, single-flight semantics preserved.
4. Every existing capture-driven TLS-MITM acceptance test passes
   without modification — wire bytes from the client perspective
   are unchanged.
5. A unit test substitutes a fake `CertMintService` that records
   `mint_leaf` calls and asserts the host, SAN, and ALPN reaching
   the service for a representative request.
6. `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo test --workspace` are green at the merge commit.

## 3. Abstractions introduced or refined

- **New trait:** `noodle_core::cert::CertMintService` —
  pure-async, takes a noodle-owned `LeafRequest` (no rama types),
  returns a noodle-owned `LeafCert` (cert chain + private key as
  DER/PEM-neutral bytes).
- **New types:** `LeafRequest`, `LeafCert`, `MintError` in
  `noodle-core`.
- **New adapter:** `LocalCertMintService` — wraps `rcgen` keypair
  generation + leaf signing. Holds the loaded `Ca` material.
- **Refined seam:** the rama `CertIssuer` boundary moves to a
  thin adapter that calls into `CertMintService`. The cache layer
  stays on rama's side (single-flight dedup is rama plumbing).

## 4. Patterns applied

- **Strategy** — `CertMintService` is the strategy interface for
  leaf minting; `LocalCertMintService` is the first concrete
  strategy. Subsequent stories add `ExternalCertMintService`.

## 5. Test plan

- **Unit (`noodle-adapters`):** `LocalCertMintService::mint_leaf`
  produces a leaf whose SAN matches the request, ECDSA P-256,
  signed by the loaded CA. Round-trip parse with `x509-parser`
  validates the chain.
- **Unit (`noodle-proxy`):** Fake `CertMintService` asserts the
  request shape (host, SAN, ALPN) reaching the service for a
  CONNECT to `api.anthropic.com`.
- **Integration / capture-driven:** Re-run the existing TLS-MITM
  capture replay (cell: `api.anthropic.com`). Client-visible
  bytes byte-identical to pre-refactor baseline.

## 6. PR scope

One PR. Roughly: ~150 LOC for trait + types in `noodle-core`,
~200 LOC for `LocalCertMintService` adapter + rama bridge, ~80
LOC test updates. Reviewable in 30 minutes.

## 7. Out of scope

- BYOCA static mode (operator-supplied CA on disk) → story 037.
- External signing (Vault / KMS / webhook) → story 038.
- Health-degradation on mint failure / rip-cord → story 039.
- Leaf cache replacement (rama's `CachedBoringMitmCertIssuer`
  stays as-is — moving the cache into noodle is a separate
  decision, not blocking).
