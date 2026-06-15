//! Adapter bridge: rama's [`BoringMitmCertIssuer`] ↔ noodle's
//! [`CertMintService`].
//!
//! Per ADR 034 §2.2 and refactor S17, leaf-cert minting is owned
//! by the noodle [`CertMintService`] trait (`noodle-core::cert`).
//! Rama's existing TLS MITM pipeline expects a
//! [`BoringMitmCertIssuer`] — its in-process `InMemoryBoringMitmCertIssuer`
//! used to be wired directly into the relay. This bridge replaces
//! that direct wiring with a thin adapter:
//!
//! ```text
//!  rama::TlsMitmRelay
//!    └── CachedBoringMitmCertIssuer            // single-flight dedup
//!           └── NoodleCertMintIssuer<S>        // this module
//!                  └── S: CertMintService      // LocalCertMintService today
//! ```
//!
//! The rama cache layer stays in place — it is generic over
//! `BoringMitmCertIssuer` so it wraps the noodle adapter without
//! changes. Single-flight semantics for concurrent connections
//! to the same host are preserved.
//!
//! ## On the boundary
//!
//! The bridge converts:
//!
//! - **Upstream X509 → [`LeafRequest`]** by extracting the
//!   subject CN and the SAN DNS-name list.
//! - **[`LeafCert`] → `(NonEmptyVec<X509>, PKey<Private>)`** by
//!   parsing the leaf DER and private-key PKCS#8 DER through
//!   `BoringSSL`.
//!
//! ALPN is plumbed through `LeafRequest::alpn`, but the leaf cert
//! itself does not encode ALPN — that is part of TLS negotiation,
//! not the certificate. The field is populated for future signers
//! (S19) that include ALPN in audit metadata.

use std::sync::Arc;

use noodle_core::{CertMintService, LeafRequest, MintError};
use noodle_tls::LocalCertMintService;
use noodle_tls::ca::Ca;
use rama::tls::boring::{
    core::{
        error::ErrorStack,
        pkey::PKey,
        x509::X509,
    },
    proxy::cert_issuer::{BoringMitmCertIssuer, MitmIssuedCert},
};
use rama::utils::collections::NonEmptyVec;
use thiserror::Error;

/// Bridge that implements rama's [`BoringMitmCertIssuer`] in
/// terms of noodle's [`CertMintService`].
///
/// Clone is cheap (one `Arc`); intended to be wrapped by
/// `CachedBoringMitmCertIssuer` upstream.
pub struct NoodleCertMintIssuer<S> {
    service: Arc<S>,
}

impl<S> NoodleCertMintIssuer<S> {
    /// Construct a bridge over the given `CertMintService`. The
    /// service is held by `Arc` so the rama cache layer (which
    /// clones the issuer per connection task) shares one
    /// underlying service.
    #[must_use]
    pub fn new(service: Arc<S>) -> Self {
        Self { service }
    }
}

impl<S> Clone for NoodleCertMintIssuer<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
        }
    }
}

/// Errors returned by the bridge.
///
/// The variants cover the three failure modes at the boundary:
/// (a) the noodle mint service returned an error, (b) the
/// upstream cert could not be parsed for SAN/CN extraction,
/// (c) the noodle DER could not be parsed back through
/// `BoringSSL`. Each carries enough context for the operator log
/// to diagnose.
#[derive(Debug, Error)]
pub enum CertBridgeError {
    /// The wrapped [`CertMintService`] returned an error.
    #[error("mint service failed: {0}")]
    Mint(#[from] MintError),
    /// The upstream certificate handed by rama could not be
    /// parsed — its SAN/CN are unreadable. Effectively the
    /// upstream presented something `LocalCertMintService` cannot
    /// mirror; surfaces to rama as a per-flow error.
    #[error("upstream cert parse failed: {0}")]
    UpstreamParse(ErrorStack),
    /// The minted leaf DER produced by the noodle service could
    /// not be parsed back through `BoringSSL`. Indicates a bug in
    /// the signer (DER must be valid X.509) — should not happen
    /// for `LocalCertMintService`.
    #[error("minted leaf parse failed: {0}")]
    LeafParse(ErrorStack),
    /// The minted private-key DER could not be parsed through
    /// `BoringSSL`. Same diagnosis: bug in the signer.
    #[error("minted leaf key parse failed: {0}")]
    LeafKeyParse(ErrorStack),
    /// The mint service returned no cert in the chain. Should
    /// never happen for a well-behaved signer.
    #[error("mint service returned empty chain")]
    EmptyChain,
}

impl<S> BoringMitmCertIssuer for NoodleCertMintIssuer<S>
where
    S: CertMintService + 'static,
{
    type Error = CertBridgeError;

    async fn issue_mitm_x509_cert(
        &self,
        server_cert: X509,
    ) -> Result<MitmIssuedCert, Self::Error> {
        let request = leaf_request_from_upstream(&server_cert);
        let minted = self.service.mint_leaf(request).await?;

        if minted.cert_chain.is_empty() {
            return Err(CertBridgeError::EmptyChain);
        }
        let mut x509_chain: Vec<X509> = Vec::with_capacity(minted.cert_chain.len());
        for der in &minted.cert_chain {
            let cert = X509::from_der(der).map_err(CertBridgeError::LeafParse)?;
            x509_chain.push(cert);
        }
        let chain = NonEmptyVec::try_from(x509_chain).map_err(|_| CertBridgeError::EmptyChain)?;

        let key = PKey::private_key_from_pkcs8(&minted.private_key_der)
            .map_err(CertBridgeError::LeafKeyParse)?;

        // OCSP stapling is best-effort and not derived from the noodle
        // mint path today — leave it unstapled (`None`). rama treats this
        // as "no staple available", same as the prior tuple return.
        Ok(MitmIssuedCert {
            crt_chain: chain,
            key,
            ocsp_staple: None,
        })
    }
}

/// Convert an upstream `BoringSSL` X509 into a [`LeafRequest`].
///
/// Pulls:
/// - **SANs**: every DNS name in the upstream cert's
///   subjectAltName extension.
/// - **CN**: the first commonName in the subject DN (if any).
/// - **`server_name`**: defaults to the first SAN; if there are
///   none, defaults to the CN. Empty when neither is present —
///   the mint service then rejects (no name to mint against).
///
/// ALPN is not derivable from the upstream cert; left empty
/// here. A future revision can plumb ALPN via a side channel
/// from the TLS `ClientHello` (S19 territory).
fn leaf_request_from_upstream(cert: &X509) -> LeafRequest {
    let san_list: Vec<String> = cert
        .subject_alt_names()
        .map(|stack| {
            stack
                .iter()
                .filter_map(|gn| gn.dnsname().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // Pull CN out of the subject DN. The `BoringSSL` API exposes a
    // text-name iterator; first commonName wins.
    let cn = cert
        .subject_name()
        .entries_by_nid(rama::tls::boring::core::nid::Nid::COMMONNAME)
        .next()
        .and_then(|entry| entry.data().as_utf8().ok())
        .map(|s| s.to_string());

    let server_name = san_list
        .first()
        .cloned()
        .or_else(|| cn.clone())
        .unwrap_or_default();

    LeafRequest::new(server_name, san_list, cn, Vec::new())
}

/// Wire the default `LocalCertMintService(ca)` into a
/// `NoodleCertMintIssuer`. The proxy's `mitm::build_mitm_service`
/// passes the result to
/// `TlsMitmRelay::new_with_cached_issuer` so the rama cache layer
/// is still in place.
#[must_use]
pub fn default_local_issuer(ca: Arc<Ca>) -> NoodleCertMintIssuer<LocalCertMintService> {
    let svc = Arc::new(LocalCertMintService::new(ca));
    NoodleCertMintIssuer::new(svc)
}

#[cfg(test)]
mod tests {
    //! Bridge unit tests live alongside the production code so
    //! the test substitutes a fake `CertMintService` and asserts
    //! the request shape reaching the service for a CONNECT to a
    //! representative upstream — the acceptance criterion in
    //! `docs/features/036-cert-mint-service-trait.md` §5.

    use std::sync::Mutex;

    use noodle_core::{LeafCert, LeafRequest};
    use rama::tls::boring::core::{
        asn1::Asn1Time,
        bn::{BigNum, MsbOption},
        hash::MessageDigest,
        nid::Nid,
        pkey::PKey as BoringPKey,
        rsa::Rsa,
        x509::{X509, X509Builder, X509NameBuilder, extension::SubjectAlternativeName},
    };

    use super::*;

    /// Capturing fake — records every `mint_leaf` call so the
    /// test can assert on the request shape (host, SAN, ALPN).
    struct CapturingMint {
        calls: Mutex<Vec<LeafRequest>>,
        response: LeafCert,
    }

    impl CapturingMint {
        fn new(response: LeafCert) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response,
            }
        }

        fn calls(&self) -> Vec<LeafRequest> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CertMintService for CapturingMint {
        async fn mint_leaf(&self, request: LeafRequest) -> Result<LeafCert, MintError> {
            self.calls.lock().unwrap().push(request);
            Ok(self.response.clone())
        }
    }

    /// Build a minimal X509 with the supplied CN + SAN DNS names.
    /// Used to feed `leaf_request_from_upstream`.
    fn fake_upstream_cert(cn: &str, sans: &[&str]) -> X509 {
        let rsa = Rsa::generate(2048).expect("rsa");
        let pkey = BoringPKey::from_rsa(rsa).expect("pkey");
        let mut builder = X509Builder::new().expect("x509 builder");
        builder.set_version(2).expect("set version");
        let mut serial = BigNum::new().expect("bn");
        serial
            .rand(128, MsbOption::MAYBE_ZERO, false)
            .expect("rand serial");
        builder
            .set_serial_number(&serial.to_asn1_integer().expect("serial asn1"))
            .expect("set serial");

        let mut name_builder = X509NameBuilder::new().expect("name builder");
        name_builder
            .append_entry_by_nid(Nid::COMMONNAME, cn)
            .expect("append cn");
        let name = name_builder.build();
        builder.set_subject_name(&name).expect("set subject");
        builder.set_issuer_name(&name).expect("set issuer");
        builder
            .set_not_before(&Asn1Time::days_from_now(0).expect("nbf"))
            .expect("nbf");
        builder
            .set_not_after(&Asn1Time::days_from_now(30).expect("naf"))
            .expect("naf");
        builder.set_pubkey(&pkey).expect("pubkey");

        if !sans.is_empty() {
            let mut san = SubjectAlternativeName::new();
            for n in sans {
                san.dns(n);
            }
            let ext = san
                .build(&builder.x509v3_context(None, None))
                .expect("build san ext");
            builder.append_extension(&ext).expect("append ext");
        }

        builder.sign(&pkey, MessageDigest::sha256()).expect("sign");
        builder.build()
    }

    #[test]
    fn leaf_request_extracts_san_and_cn_from_upstream() {
        let cert = fake_upstream_cert("api.anthropic.com", &["api.anthropic.com", "anthropic.com"]);
        let req = leaf_request_from_upstream(&cert);
        assert_eq!(req.upstream_cn.as_deref(), Some("api.anthropic.com"));
        assert!(req.upstream_san.contains(&"api.anthropic.com".to_string()));
        assert!(req.upstream_san.contains(&"anthropic.com".to_string()));
        // server_name defaults to first SAN.
        assert_eq!(req.server_name, "api.anthropic.com");
    }

    #[test]
    fn leaf_request_falls_back_to_cn_when_no_san() {
        let cert = fake_upstream_cert("only-cn.example.com", &[]);
        let req = leaf_request_from_upstream(&cert);
        assert_eq!(req.upstream_cn.as_deref(), Some("only-cn.example.com"));
        assert!(req.upstream_san.is_empty());
        assert_eq!(req.server_name, "only-cn.example.com");
    }

    #[tokio::test]
    async fn bridge_forwards_request_to_mint_service_with_expected_shape() {
        // Mint a real leaf via LocalCertMintService so the
        // returned DER round-trips through `BoringSSL`. Wrap that
        // signer in CapturingMint-as-decorator? Simpler: capture
        // the request shape via a fake, but build a real leaf
        // DER once to satisfy the bridge's parse path.

        let ca = Arc::new(Ca::generate().expect("ca"));
        let local = LocalCertMintService::new(Arc::clone(&ca));
        let bootstrap = local
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                Some("api.anthropic.com".into()),
                vec![],
            ))
            .await
            .expect("bootstrap leaf");
        let fake = Arc::new(CapturingMint::new(bootstrap));

        let bridge = NoodleCertMintIssuer::new(Arc::clone(&fake));

        // Synthesize a representative upstream cert (what rama
        // would hand us after the TlsMitmRelay completed the
        // upstream handshake).
        let upstream = fake_upstream_cert(
            "api.anthropic.com",
            &["api.anthropic.com", "*.anthropic.com"],
        );

        let issued = bridge.issue_mitm_x509_cert(upstream).await.expect("mint");
        assert!(!issued.crt_chain.is_empty(), "non-empty chain");

        let calls = fake.calls();
        assert_eq!(calls.len(), 1, "service called once");
        let req = &calls[0];

        // Acceptance §5: the mint service sees the right host,
        // SAN list, and CN. ALPN is empty here (bridge does not
        // surface it yet; S19 territory).
        assert_eq!(req.server_name, "api.anthropic.com");
        assert!(req.upstream_san.contains(&"api.anthropic.com".to_string()));
        assert!(req.upstream_san.contains(&"*.anthropic.com".to_string()));
        assert_eq!(req.upstream_cn.as_deref(), Some("api.anthropic.com"));
        assert!(req.alpn.is_empty());
    }

    #[tokio::test]
    async fn bridge_returns_minted_chain_parseable_as_x509() {
        let ca = Arc::new(Ca::generate().expect("ca"));
        let svc = Arc::new(LocalCertMintService::new(Arc::clone(&ca)));
        let bridge = NoodleCertMintIssuer::new(svc);
        let upstream =
            fake_upstream_cert("host.example.com", &["host.example.com", "alt.example.com"]);
        let issued = bridge.issue_mitm_x509_cert(upstream).await.expect("mint");

        assert!(!issued.crt_chain.is_empty(), "chain non-empty");
        // Pub key is ECDSA (id-ecPublicKey OID 1.2.840.10045.2.1).
        let leaf = &issued.crt_chain[0];
        let leaf_pem = leaf.to_pem().expect("pem");
        assert!(leaf_pem.starts_with(b"-----BEGIN CERTIFICATE-----"));
        // The matching private key round-trips back to DER.
        let key_der = issued.key.private_key_to_der().expect("key der");
        assert!(!key_der.is_empty());
    }

    #[tokio::test]
    async fn bridge_propagates_mint_service_errors() {
        struct AlwaysFails;
        impl CertMintService for AlwaysFails {
            async fn mint_leaf(&self, _: LeafRequest) -> Result<LeafCert, MintError> {
                Err(MintError::SignerUnavailable("test-induced".into()))
            }
        }

        let bridge = NoodleCertMintIssuer::new(Arc::new(AlwaysFails));
        let upstream = fake_upstream_cert("x", &["x"]);
        let err = bridge
            .issue_mitm_x509_cert(upstream)
            .await
            .expect_err("err");
        assert!(matches!(
            err,
            CertBridgeError::Mint(MintError::SignerUnavailable(_))
        ));
    }
}
