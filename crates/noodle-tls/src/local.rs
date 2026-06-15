//! Local in-process implementation of [`CertMintService`].
//!
//! [`LocalCertMintService`] is the noodle-side strategy that
//! preserves the existing local-CA behaviour from before slice
//! S17 (ADR 034 §2.2): on every `mint_leaf` call, generate a
//! fresh ECDSA P-256 keypair, sign a leaf with the loaded
//! [`Ca`], return the chain + key as DER bytes.
//!
//! Substituted at the proxy boundary by `noodle-proxy::cert_bridge`,
//! which adapts the rama `BoringMitmCertIssuer` surface to this
//! port. The rama cache layer (`CachedBoringMitmCertIssuer`) sits
//! upstream of the bridge so concurrent requests for the same host
//! still single-flight through one mint operation.
//!
//! ## Behaviour parity with the pre-S17 path
//!
//! Before S17 the proxy used rama's `InMemoryBoringMitmCertIssuer`
//! directly. Both that signer and this one:
//!
//! - Generate a fresh ECDSA P-256 keypair per leaf (no key
//!   reuse — important so cracking one leaf doesn't reveal
//!   others).
//! - Sign the leaf with the loaded CA key.
//! - Mirror the upstream certificate's SANs and CN onto the
//!   minted leaf so clients that pin on subject identity accept
//!   the leaf as if it were the upstream's.
//!
//! Per-leaf bytes differ run-to-run (different serial, different
//! key) but the client-visible TLS handshake behaviour is
//! identical: the same SANs are accepted, the same ALPN protocols
//! are advertised, the chain still verifies via the noodle CA.
//!
//! ## Latency
//!
//! Local mint is dominated by ECDSA P-256 keygen + signing —
//! typically well under 1 ms on modern hardware. The rama cache
//! layer absorbs the cost for repeat hosts (every cell after the
//! first connection is a cache hit, not a mint).

use std::sync::Arc;

use noodle_core::{CertMintService, LeafCert, LeafRequest, MintError};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType,
};
use time::{Duration, OffsetDateTime};

use crate::ca::Ca;

/// Validity window for minted leaves: 90 days forward + 1-hour
/// back-skew. Mirrors mainstream public-CA leaf lifetimes (Let's
/// Encrypt, Google Trust Services issue ~90-day leaves) and the
/// rama in-memory issuer's defaults.
const LEAF_VALIDITY_DAYS: i64 = 90;
const LEAF_SKEW_HOURS: i64 = 1;

/// In-process leaf signer wrapping the noodle [`Ca`].
///
/// Constructed once at proxy startup; clones share the inner `Arc`.
/// Cheap to clone. Held by the rama `BoringMitmCertIssuer` bridge
/// adapter in `noodle-proxy::cert_bridge`.
#[derive(Clone)]
pub struct LocalCertMintService {
    ca: Arc<Ca>,
}

impl LocalCertMintService {
    /// Construct a `LocalCertMintService` over the given CA.
    ///
    /// The `Ca` provides both the issuer cert (signed by itself)
    /// and the issuer private key. Both are required to sign
    /// leaves; both stay in memory for the lifetime of the
    /// service.
    #[must_use]
    pub fn new(ca: Arc<Ca>) -> Self {
        Self { ca }
    }

    /// Access the wrapped CA — exposed so callers that need both
    /// the mint service and the CA cert (e.g. for `NODE_EXTRA_CA_CERTS`)
    /// can re-use the same `Arc` rather than holding two.
    #[must_use]
    pub fn ca(&self) -> &Arc<Ca> {
        &self.ca
    }
}

impl CertMintService for LocalCertMintService {
    async fn mint_leaf(&self, request: LeafRequest) -> Result<LeafCert, MintError> {
        // No I/O — the mint is pure CPU. Wrap in `Result::Ok` so
        // the trait surface stays uniform with the external
        // signer that will land in S19.
        mint_leaf_sync(&self.ca, &request)
    }
}

/// Pure-sync mint implementation. Split out for unit-testability —
/// callers can exercise this without an async runtime.
fn mint_leaf_sync(ca: &Ca, request: &LeafRequest) -> Result<LeafCert, MintError> {
    if request.server_name.is_empty() && request.upstream_san.is_empty() {
        return Err(MintError::InvalidRequest(
            "leaf request carries no server name and no SANs".into(),
        ));
    }

    // Build the SAN list: every upstream SAN, plus the server_name
    // if it isn't already covered. De-dup while preserving order
    // (deterministic, helps cache-hit equality).
    let mut sans: Vec<String> = Vec::with_capacity(request.upstream_san.len() + 1);
    for s in &request.upstream_san {
        if !sans.iter().any(|existing| existing == s) {
            sans.push(s.clone());
        }
    }
    if !request.server_name.is_empty() && !sans.iter().any(|s| s == &request.server_name) {
        sans.push(request.server_name.clone());
    }

    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|e| MintError::SignerDenied(format!("leaf keypair generation failed: {e}")))?;

    // Build leaf cert params. SANs come in via the constructor;
    // CN, validity window, key usages, and EKUs are set
    // explicitly so clients pinning on those fields see a
    // mainstream server-auth leaf.
    let mut params = CertificateParams::new(sans.clone())
        .map_err(|e| MintError::InvalidRequest(format!("invalid SAN list: {e}")))?;

    // CN: prefer the upstream's CN when present; fall back to
    // the server_name (matches rama / mitmproxy convention).
    let cn = request
        .upstream_cn
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if request.server_name.is_empty() {
                sans.first().cloned()
            } else {
                Some(request.server_name.clone())
            }
        })
        .unwrap_or_else(|| "noodle-mitm-leaf".to_string());
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    params.distinguished_name = dn;

    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::hours(LEAF_SKEW_HOURS);
    params.not_after = now + Duration::days(LEAF_VALIDITY_DAYS);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let (issuer_cert, issuer_key) = ca.issuer_handles();
    let leaf = params
        .signed_by(&leaf_key, issuer_cert, issuer_key)
        .map_err(|e| MintError::SignerDenied(format!("leaf signing failed: {e}")))?;

    let leaf_der = leaf.der().to_vec();
    let key_der = leaf_key.serialize_der();

    // S18 / ADR 034 §4: BYOCA-static can supply an intermediate
    // chain via `chain.pem`. When present, append it to the leaf's
    // chain so the client can build a path to a root it already
    // trusts even when only the leaf-signing CA's intermediate is
    // trusted directly. Local-mode CAs have an empty intermediate
    // slice, leaving the chain at `[leaf]` — the pre-S18 shape.
    let intermediates = ca.intermediate_chain_der();
    let mut cert_chain = Vec::with_capacity(1 + intermediates.len());
    cert_chain.push(leaf_der);
    for int_der in intermediates {
        cert_chain.push(int_der.clone());
    }

    Ok(LeafCert {
        cert_chain,
        private_key_der: key_der,
    })
}

// Sanity import check: ensure `SanType::DnsName` and the
// `ExtendedKeyUsagePurpose::ServerAuth` constant are still
// available from rcgen — both load-bearing for `mint_leaf_sync`
// even though they are referenced indirectly (rcgen converts
// the `Vec<String>` SAN list into `SanType::DnsName` internally).
//
// If rcgen ever renames or drops these, this fails to compile
// rather than silently producing a leaf without the right
// extensions.
#[allow(dead_code)]
const _SAN_TYPE_DNS: fn(rcgen::Ia5String) -> SanType = SanType::DnsName;
#[allow(dead_code)]
const _EKU_SERVER_AUTH: ExtendedKeyUsagePurpose = ExtendedKeyUsagePurpose::ServerAuth;

#[cfg(test)]
mod tests {
    use super::*;

    fn ca() -> Arc<Ca> {
        Arc::new(Ca::generate().expect("generate test CA"))
    }

    #[tokio::test]
    async fn mints_leaf_with_requested_server_name_in_san() {
        let svc = LocalCertMintService::new(ca());
        let req = LeafRequest::new(
            "api.anthropic.com",
            vec!["api.anthropic.com".into()],
            Some("api.anthropic.com".into()),
            vec![b"h2".to_vec()],
        );
        let leaf = svc.mint_leaf(req).await.expect("mint succeeds");
        assert_eq!(leaf.cert_chain.len(), 1, "leaf-only chain");
        assert!(!leaf.cert_chain[0].is_empty(), "leaf DER non-empty");
        assert!(!leaf.private_key_der.is_empty(), "key DER non-empty");

        // Parse the leaf via x509-parser (re-exported through rcgen)
        // and validate SAN + signer.
        let (_, parsed) = x509_parser::parse_x509_certificate(&leaf.cert_chain[0])
            .expect("parse minted leaf DER");
        // SAN extension present and includes the server name.
        let mut found_san = false;
        for ext in parsed.extensions() {
            if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
                ext.parsed_extension()
            {
                for gn in &san.general_names {
                    if let x509_parser::extensions::GeneralName::DNSName(name) = gn
                        && *name == "api.anthropic.com"
                    {
                        found_san = true;
                    }
                }
            }
        }
        assert!(found_san, "SAN list must include the requested server_name");

        // Subject CN was set to the upstream CN (== server_name in this case).
        let cn_iter = parsed
            .subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok())
            .collect::<Vec<_>>();
        assert!(
            cn_iter.contains(&"api.anthropic.com"),
            "subject CN should be api.anthropic.com; got {cn_iter:?}"
        );
    }

    #[tokio::test]
    async fn mints_leaf_signed_by_loaded_ca() {
        let ca_arc = ca();
        let svc = LocalCertMintService::new(Arc::clone(&ca_arc));

        let leaf = svc
            .mint_leaf(LeafRequest::new(
                "example.com",
                vec!["example.com".into()],
                None,
                vec![],
            ))
            .await
            .expect("mint succeeds");

        let (_, leaf_parsed) =
            x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse leaf");
        let ca_pem = ca_arc.cert_pem();
        let ca_der = pem::parse(ca_pem).expect("parse CA PEM").into_contents();
        let (_, ca_parsed) = x509_parser::parse_x509_certificate(&ca_der).expect("parse CA DER");

        // The leaf's issuer DN must equal the CA's subject DN.
        let leaf_issuer = leaf_parsed.issuer().to_string();
        let ca_subject = ca_parsed.subject().to_string();
        assert_eq!(
            leaf_issuer, ca_subject,
            "leaf issuer DN must match CA subject DN (signed by the CA)"
        );

        // And the leaf's signature must verify against the CA's
        // public key. x509-parser exposes this via
        // `verify_signature` taking the issuer public key.
        leaf_parsed
            .verify_signature(Some(ca_parsed.public_key()))
            .expect("leaf signature verifies against CA public key");
    }

    #[tokio::test]
    async fn mints_leaf_uses_ecdsa_p256_keypair() {
        let svc = LocalCertMintService::new(ca());
        let leaf = svc
            .mint_leaf(LeafRequest::new("host", vec!["host".into()], None, vec![]))
            .await
            .expect("mint succeeds");

        // Parse the cert and read the SPKI algorithm. P-256 OID is
        // 1.2.840.10045.3.1.7 (prime256v1).
        let (_, parsed) =
            x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse leaf");
        let pk = parsed.public_key();
        let algo_oid = pk.algorithm.algorithm.to_string();
        // id-ecPublicKey OID.
        assert_eq!(
            algo_oid, "1.2.840.10045.2.1",
            "leaf SPKI must be id-ecPublicKey (ECDSA); got {algo_oid}"
        );
        // Curve OID is encoded as the params.
        let curve = pk
            .algorithm
            .parameters
            .as_ref()
            .map(|p| p.as_oid().map(|o| o.to_string()).unwrap_or_default())
            .unwrap_or_default();
        assert_eq!(
            curve, "1.2.840.10045.3.1.7",
            "curve must be P-256 (prime256v1); got {curve}"
        );
    }

    #[tokio::test]
    async fn includes_every_upstream_san_in_the_leaf() {
        let svc = LocalCertMintService::new(ca());
        let req = LeafRequest::new(
            "api.example.com",
            vec![
                "api.example.com".into(),
                "v2.example.com".into(),
                "example.com".into(),
            ],
            None,
            vec![],
        );
        let leaf = svc.mint_leaf(req).await.expect("mint");
        let (_, parsed) = x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse");
        let mut san_names: Vec<String> = Vec::new();
        for ext in parsed.extensions() {
            if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
                ext.parsed_extension()
            {
                for gn in &san.general_names {
                    if let x509_parser::extensions::GeneralName::DNSName(name) = gn {
                        san_names.push((*name).to_string());
                    }
                }
            }
        }
        assert!(san_names.contains(&"api.example.com".to_string()));
        assert!(san_names.contains(&"v2.example.com".to_string()));
        assert!(san_names.contains(&"example.com".to_string()));
    }

    #[tokio::test]
    async fn rejects_request_with_no_name_or_sans() {
        let svc = LocalCertMintService::new(ca());
        let req = LeafRequest::new("", vec![], None, vec![]);
        let err = svc.mint_leaf(req).await.expect_err("should reject");
        assert!(matches!(err, MintError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn local_ca_yields_single_entry_leaf_chain() {
        // Pre-S18 shape: a Ca constructed by ::generate() has no
        // intermediate chain → minted leaves are `[leaf]`. Existing
        // S17 callers and tests depend on this.
        let svc = LocalCertMintService::new(ca());
        let leaf = svc
            .mint_leaf(LeafRequest::new("h", vec!["h".into()], None, vec![]))
            .await
            .expect("mint");
        assert_eq!(
            leaf.cert_chain.len(),
            1,
            "local-mode mint must remain single-entry"
        );
    }

    #[tokio::test]
    async fn byoca_static_with_chain_pem_extends_leaf_chain() {
        // S18 acceptance: when the CA was loaded from disk with a
        // `chain.pem` present, every minted leaf's chain contains
        // [leaf, intermediate1, intermediate2, ...]. Built here
        // via `load_static` over a temp dir prepared with three
        // PEM blocks.
        use std::fs;
        use std::path::Path;

        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path: &Path = dir.path();
        // Write a valid CA fixture (cert + key).
        let written = Ca::generate().expect("test CA");
        fs::write(dir_path.join("ca.pem"), written.cert_pem()).expect("write ca.pem");
        fs::write(dir_path.join("ca.key"), written.key_pem()).expect("write ca.key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir_path.join("ca.key"), fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
        // Two synthetic intermediates (any CA-shaped cert is fine for
        // this test — chain-loading is a byte-stacking operation).
        let int_a = Ca::generate().expect("int a");
        let int_b = Ca::generate().expect("int b");
        let chain_pem = format!("{}{}", int_a.cert_pem(), int_b.cert_pem());
        fs::write(dir_path.join("chain.pem"), chain_pem).expect("write chain.pem");

        let loaded = Arc::new(Ca::load_static(dir_path).expect("load_static"));
        let svc = LocalCertMintService::new(loaded);

        let leaf = svc
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                None,
                vec![],
            ))
            .await
            .expect("mint");

        assert_eq!(
            leaf.cert_chain.len(),
            3,
            "chain must be [leaf, int_a, int_b]; got {} entries",
            leaf.cert_chain.len()
        );
        // Leaf's issuer DN matches the loaded CA's subject DN —
        // proves the leaf was signed by the operator-supplied key.
        let (_, leaf_parsed) =
            x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse leaf");
        let ca_der = pem::parse(written.cert_pem())
            .expect("parse CA pem")
            .into_contents();
        let (_, ca_parsed) = x509_parser::parse_x509_certificate(&ca_der).expect("parse CA");
        assert_eq!(
            leaf_parsed.issuer().to_string(),
            ca_parsed.subject().to_string(),
            "leaf issuer DN must equal operator CA subject DN"
        );
    }

    #[tokio::test]
    async fn deduplicates_san_entries_when_server_name_is_already_in_upstream_san() {
        let svc = LocalCertMintService::new(ca());
        let req = LeafRequest::new("h.example.com", vec!["h.example.com".into()], None, vec![]);
        let leaf = svc.mint_leaf(req).await.expect("mint");
        let (_, parsed) = x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse");
        let mut san_count = 0usize;
        for ext in parsed.extensions() {
            if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
                ext.parsed_extension()
            {
                for gn in &san.general_names {
                    if let x509_parser::extensions::GeneralName::DNSName(_) = gn {
                        san_count += 1;
                    }
                }
            }
        }
        assert_eq!(
            san_count, 1,
            "expected exactly one SAN entry, got {san_count}"
        );
    }
}
