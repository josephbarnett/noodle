//! `CertMintService` — leaf-certificate minting port.
//!
//! Per ADR 034 §2.2, the proxy delegates per-host leaf cert
//! generation to an implementation of [`CertMintService`]. The
//! port is framework-neutral on purpose: `noodle-core` takes no
//! `rama` or `BoringSSL` dependency. Adapters (in-process `rcgen`
//! for local mode, external signers for BYOCA / KMS modes) live
//! in `noodle-adapters`. The `rama` `CertIssuer` bridge that
//! adapts between `rama`'s `BoringSSL` trait surface and this
//! port lives in `noodle-proxy`.
//!
//! ## Design
//!
//! - [`LeafRequest`] describes what cert the proxy needs — the
//!   SNI it observed, the upstream certificate's SAN/CN (so the
//!   minted leaf can mirror those, satisfying clients that check
//!   subject identity), and the requested ALPN protocols.
//! - [`LeafCert`] returns the signed certificate chain plus the
//!   matching private key — both encoded as DER bytes so this
//!   crate stays free of cryptographic dependencies.
//! - [`MintError`] enumerates the failure modes adapters need to
//!   surface upward. Health-degradation logic (ADR 034 §3.2)
//!   reads these to decide when to trip the rip-cord.
//!
//! ## Single signature, multiple strategies
//!
//! The trait is the **Strategy** abstraction:
//!
//! | Mode | Impl | Latency |
//! |---|---|---|
//! | Local (this slice) | `LocalCertMintService` (rcgen, in-process) | < 1 ms |
//! | BYOCA-static (S18) | `LocalCertMintService` loaded from operator's CA | < 1 ms |
//! | External (S19) | `ExternalCertMintService` (CSR → Vault/KMS/PKI) | 10–500 ms |

use std::future::Future;

use thiserror::Error;

/// Request for a freshly-minted leaf certificate.
///
/// The proxy fills this from what it observed: the SNI on the
/// client's TLS `ClientHello`, the upstream server's certificate
/// (its SAN list + CN, so noodle's mirrored leaf is acceptable to
/// clients that pin on subject identity), and the negotiated
/// ALPN protocol list.
#[derive(Debug, Clone)]
pub struct LeafRequest {
    /// The DNS name the client requested via SNI (or implied by
    /// the CONNECT line when SNI is absent). Empty when no name
    /// is available — adapters should reject in that case.
    pub server_name: String,
    /// DNS-name SANs lifted from the upstream certificate. The
    /// minted leaf must include every name in this list so any
    /// client checking subject identity sees the upstream's view.
    pub upstream_san: Vec<String>,
    /// CN lifted from the upstream certificate, if present.
    /// Modern clients ignore the CN in favour of SANs; populated
    /// here for completeness so legacy clients are not surprised.
    pub upstream_cn: Option<String>,
    /// ALPN protocol identifiers the leaf must support — opaque
    /// bytes (e.g. `b"h2"`, `b"http/1.1"`) per RFC 7301. The cert
    /// itself does not carry ALPN; the field is plumbed here so
    /// future signers (S19) can include it in audit metadata.
    pub alpn: Vec<Vec<u8>>,
}

impl LeafRequest {
    /// Construct a new [`LeafRequest`]. The convenience accepts
    /// any iterable for SANs / ALPN so adapters can pass typed
    /// inputs directly.
    #[must_use]
    pub fn new(
        server_name: impl Into<String>,
        upstream_san: Vec<String>,
        upstream_cn: Option<String>,
        alpn: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            upstream_san,
            upstream_cn,
            alpn,
        }
    }
}

/// Result of a successful `mint_leaf` call.
///
/// The certificate chain is ordered leaf-first (leaf, intermediate(s),
/// optional root). Each entry is DER-encoded X.509. The private key
/// is DER-encoded PKCS#8 — adapters that produce PEM should convert
/// before returning.
#[derive(Debug, Clone)]
pub struct LeafCert {
    /// Leaf-first DER chain. Index 0 is the leaf the proxy will
    /// present to the client; subsequent entries are intermediates
    /// (and optionally the root for self-contained verification).
    pub cert_chain: Vec<Vec<u8>>,
    /// PKCS#8 DER-encoded private key matching the leaf at
    /// `cert_chain[0]`.
    pub private_key_der: Vec<u8>,
}

impl LeafCert {
    /// Construct a [`LeafCert`] from a leaf DER + private key DER.
    /// No intermediates — appropriate for the in-process signer.
    #[must_use]
    pub fn from_leaf(leaf_der: Vec<u8>, private_key_der: Vec<u8>) -> Self {
        Self {
            cert_chain: vec![leaf_der],
            private_key_der,
        }
    }
}

/// Failure modes a [`CertMintService`] surfaces.
///
/// The variants match ADR 034 §2.2; health-degradation logic
/// (S20) keys off them to decide when to fail open.
#[derive(Debug, Error)]
pub enum MintError {
    /// Signer is reachable but rejects the request. Includes the
    /// reason as reported by the signer (or a brief synthesized
    /// message for adapters that don't carry one). Does not
    /// degrade health on its own — a denied request is a client-
    /// or policy-level issue, not a signer outage.
    #[error("signer denied request: {0}")]
    SignerDenied(String),
    /// Signer is unreachable or unresponsive. This is the
    /// rip-cord signal: enough consecutive `SignerUnavailable`
    /// failures flip the health probe and engage fail-open per
    /// ADR 034 §3.2.
    #[error("signer unavailable: {0}")]
    SignerUnavailable(String),
    /// Operation exceeded the configured `signer_timeout`.
    /// Treated as `SignerUnavailable` for health purposes.
    #[error("signer timeout")]
    Timeout,
    /// Request was malformed — e.g. missing `server_name`, invalid
    /// SAN. Not retryable; not a signer health signal.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

/// Port the proxy uses to obtain a signed leaf certificate.
///
/// Implementations:
/// - `LocalCertMintService` (`noodle-adapters::cert`) — in-process
///   `rcgen` signer.
/// - `ExternalCertMintService` (S19) — CSR-over-HTTP to Vault /
///   KMS / enterprise PKI.
///
/// Bridges (in `noodle-proxy::cert_bridge`) adapt this trait to
/// `rama`'s `BoringMitmCertIssuer` surface; the `rama` cache
/// layer (`CachedBoringMitmCertIssuer`) sits **above** the bridge
/// so single-flight dedup of concurrent mint requests for the
/// same host is preserved.
pub trait CertMintService: Send + Sync {
    /// Mint a leaf certificate for the requested host.
    ///
    /// Implementations should:
    /// 1. Generate the leaf keypair locally (CSR-only model — the
    ///    private key never leaves the noodle host, ADR 034 §5.2).
    /// 2. Build a leaf cert (or CSR for external signers) bearing
    ///    every SAN in `request.upstream_san` plus `request.server_name`.
    /// 3. Sign the leaf with the configured CA (local) or remote
    ///    signer (external).
    /// 4. Return the chain + key as DER bytes.
    fn mint_leaf(
        &self,
        request: LeafRequest,
    ) -> impl Future<Output = Result<LeafCert, MintError>> + Send;
}

/// Object-safe sibling of [`CertMintService`] usable as
/// `Arc<dyn DynCertMintService>`.
///
/// Why: [`CertMintService`] uses RPITIT (return-position
/// `impl Future`), which is not `dyn`-safe. Callers that need to
/// substitute a `LocalCertMintService` or an
/// `ExternalCertMintService<VaultPkiSigner>` at the same trait
/// object slot (e.g. the proxy entry point dispatching on
/// `ca.mode`) reach for this trait. A blanket impl below makes
/// every `CertMintService` automatically a `DynCertMintService`.
pub trait DynCertMintService: Send + Sync {
    /// Mint a leaf certificate. Returns a boxed future so the
    /// trait is `dyn`-safe. Implementers should not implement
    /// this directly — implement [`CertMintService`] and use the
    /// blanket impl.
    fn mint_leaf_boxed<'a>(
        &'a self,
        request: LeafRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<LeafCert, MintError>> + Send + 'a>>;
}

impl<T> DynCertMintService for T
where
    T: CertMintService,
{
    fn mint_leaf_boxed<'a>(
        &'a self,
        request: LeafRequest,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<LeafCert, MintError>> + Send + 'a>> {
        Box::pin(self.mint_leaf(request))
    }
}

/// Helper wrapper that adapts an `Arc<dyn DynCertMintService>`
/// back into a `CertMintService` so the rama bridge
/// (`NoodleCertMintIssuer<S>`) can be parameterised on it.
///
/// Lets the proxy hold a single `Arc<dyn DynCertMintService>` at
/// the config layer while still feeding the generic bridge a
/// concrete type. Cheap to clone.
pub struct DynCertMintAdapter {
    inner: std::sync::Arc<dyn DynCertMintService>,
}

impl DynCertMintAdapter {
    /// Wrap a shared, type-erased mint service.
    #[must_use]
    pub fn new(inner: std::sync::Arc<dyn DynCertMintService>) -> Self {
        Self { inner }
    }
}

impl Clone for DynCertMintAdapter {
    fn clone(&self) -> Self {
        Self {
            inner: std::sync::Arc::clone(&self.inner),
        }
    }
}

impl CertMintService for DynCertMintAdapter {
    async fn mint_leaf(&self, request: LeafRequest) -> Result<LeafCert, MintError> {
        self.inner.mint_leaf_boxed(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_request_new_preserves_fields() {
        let req = LeafRequest::new(
            "api.anthropic.com",
            vec!["api.anthropic.com".into(), "anthropic.com".into()],
            Some("anthropic.com".into()),
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        );
        assert_eq!(req.server_name, "api.anthropic.com");
        assert_eq!(req.upstream_san.len(), 2);
        assert_eq!(req.upstream_cn.as_deref(), Some("anthropic.com"));
        assert_eq!(req.alpn.len(), 2);
        assert_eq!(req.alpn[0], b"h2");
    }

    #[test]
    fn leaf_cert_from_leaf_yields_single_entry_chain() {
        let leaf = LeafCert::from_leaf(vec![1, 2, 3], vec![4, 5, 6]);
        assert_eq!(leaf.cert_chain.len(), 1);
        assert_eq!(leaf.cert_chain[0], vec![1, 2, 3]);
        assert_eq!(leaf.private_key_der, vec![4, 5, 6]);
    }

    #[test]
    fn mint_error_display_is_informative() {
        assert_eq!(
            MintError::SignerDenied("policy".into()).to_string(),
            "signer denied request: policy"
        );
        assert_eq!(
            MintError::SignerUnavailable("connection refused".into()).to_string(),
            "signer unavailable: connection refused"
        );
        assert_eq!(MintError::Timeout.to_string(), "signer timeout");
        assert_eq!(
            MintError::InvalidRequest("no server name".into()).to_string(),
            "invalid request: no server name"
        );
    }

    /// Verify the trait is dyn-compatible enough to use behind
    /// `Arc<dyn CertMintService>` from the adapter bridge.
    /// (The trait itself uses RPITIT, which is not dyn-safe; we
    /// take `Arc<impl CertMintService>` in the bridge. This test
    /// proves the basic shape: a struct can implement the trait.)
    #[test]
    fn trait_can_be_implemented_by_a_simple_struct() {
        struct Static {
            response: LeafCert,
        }
        impl CertMintService for Static {
            async fn mint_leaf(&self, _: LeafRequest) -> Result<LeafCert, MintError> {
                Ok(self.response.clone())
            }
        }

        let svc = Static {
            response: LeafCert::from_leaf(vec![0u8; 10], vec![1u8; 16]),
        };
        let fut = svc.mint_leaf(LeafRequest::new("h", vec![], None, vec![]));
        // Just confirm the future is constructible; we don't need
        // a runtime for the shape test.
        std::mem::drop(fut);
    }
}
