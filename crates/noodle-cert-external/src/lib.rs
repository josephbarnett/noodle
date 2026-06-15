//! External-signer leaf-mint path for noodle (ADR 034 §2.2–§2.5, S19).
//!
//! Where `noodle-adapters::cert::LocalCertMintService` signs leaves
//! in-process from the loaded `noodle-adapters::tls::ca::Ca`,
//! [`ExternalCertMintService`] generates the leaf keypair locally
//! and delegates **signing** to a remote authority (`HashiCorp`
//! Vault PKI today; AWS / Azure / SCEP follow per ADR 034 §2.4).
//!
//! Carved out of `noodle-adapters` per ADR 039 §4 — `reqwest`
//! and the HTTPS signer client are proxy-host-only concerns and
//! must not appear in the plugin-host crate graph.
//!
//! ```ignore
//! #![forbid(unsafe_code)]
//! ```
//!
//! The leaf private key never leaves the noodle host — the
//! external signer sees only the CSR (per ADR 034 §5.2). The
//! signed leaf returned by the signer chains to the enterprise's
//! existing trust anchor; fleet devices trust noodle's leaves
//! automatically (ADR 034 §2.3).
//!
//! ## Module shape
//!
//! - [`ExternalSignerBackend`] — the strategy trait. One impl
//!   per signing protocol; v1 ships [`vault::VaultPkiSigner`].
//! - [`CertificationRequest`] / [`SignContext`] — inputs to
//!   [`ExternalSignerBackend::sign_csr`].
//! - [`CertChain`] / [`SignerError`] — outputs.
//! - [`ExternalCertMintService`] — `CertMintService` impl that
//!   composes a backend with local keypair generation,
//!   CSR construction, timeout enforcement, and audit emission.
//!
//! ## Latency budget (ADR 034 §3.3)
//!
//! Each mint is bounded by `signer_timeout` (default 2s via
//! `ExternalCertMintService::with_default_timeout`). Timeouts
//! surface as [`noodle_core::MintError::Timeout`] which the rip-
//! cord (S20) treats as a `SignerUnavailable`-equivalent
//! health-degradation signal.

pub mod vault;

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use noodle_core::layered::{AuditEvent, AuditKind, Layer, SideEffect, SideEffectSink};
use noodle_core::{CertMintService, LeafCert, LeafRequest, MintError};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use thiserror::Error;
use time::{Duration as TimeDuration, OffsetDateTime};

/// Default timeout per mint, ADR 034 §3.3.
pub const DEFAULT_SIGNER_TIMEOUT: Duration = Duration::from_secs(2);

/// Validity window for the locally-generated leaf "intent": the
/// signer chooses the actual lifetime. These values populate the
/// CSR's metadata where applicable. Mirrors the local-signer
/// defaults (90d + 1h skew) so audit logs are consistent.
const LEAF_VALIDITY_DAYS: i64 = 90;
const LEAF_SKEW_HOURS: i64 = 1;

/// CSR + context the external signer receives.
///
/// Holds:
/// - **PEM CSR**: PKCS#10 request bearing the locally-generated
///   public key, the SAN list, the CN, and EKU=ServerAuth.
/// - **Server name**: the upstream host this leaf is for; carried
///   alongside the CSR so backends that route by host (Vault's
///   `role` parameter, SCEP profile, etc.) can use it.
#[derive(Debug, Clone)]
pub struct CertificationRequest {
    /// PEM-encoded PKCS#10 certificate signing request. Contains
    /// the public key, requested SANs, CN, and key/EKU usages.
    /// The matching private key stays on the noodle host.
    pub csr_pem: String,
    /// The host the requested leaf is for (e.g. `api.anthropic.com`).
    /// Used by backends that select a signing profile by host;
    /// also folded into audit `detail` payloads.
    pub server_name: String,
    /// Cloned-out SAN DNS names — backends may need them in a
    /// shape that doesn't require re-parsing the CSR.
    pub sans: Vec<String>,
    /// Cloned-out CN (may be empty).
    pub cn: Option<String>,
}

/// Out-of-CSR context the signer may need (auth, routing, etc.).
///
/// Populated by [`ExternalCertMintService::mint_leaf`] before
/// calling the backend. Backends use whatever subset they need.
#[derive(Debug, Clone, Default)]
pub struct SignContext {
    /// ALPN protocol identifiers the leaf will serve. Not
    /// encoded in the CSR; some backends (Vault PKI) accept this
    /// as request metadata for routing or policy.
    pub alpn: Vec<Vec<u8>>,
}

/// Successful sign response.
///
/// `leaf_der` is DER-encoded X.509. `chain_der` is the
/// intermediate chain (excluding the leaf, excluding the root) —
/// `[intermediate_1, intermediate_2, ...]` in leaf-presenting
/// order. The mint service assembles `[leaf, intermediate*,]`
/// into the [`LeafCert::cert_chain`].
#[derive(Debug, Clone)]
pub struct CertChain {
    /// Leaf certificate, DER.
    pub leaf_der: Vec<u8>,
    /// Intermediate chain (DER), leaf-presenting order. May be
    /// empty if the signer returns the leaf directly under a
    /// root the fleet trusts.
    pub chain_der: Vec<Vec<u8>>,
}

/// Errors a backend may report.
///
/// Mapped onto [`noodle_core::MintError`] by
/// [`ExternalCertMintService`] for uniform consumer handling.
#[derive(Debug, Error)]
pub enum SignerError {
    /// Backend is reachable but rejected the request (policy
    /// denial, invalid role, malformed CSR per backend rules).
    /// Maps to `MintError::SignerDenied`.
    #[error("signer denied request: {0}")]
    Denied(String),
    /// Backend is unreachable, returned 5xx, or otherwise failed
    /// transport. Maps to `MintError::SignerUnavailable`. Drives
    /// the rip-cord (ADR 034 §3.2).
    #[error("signer unavailable: {0}")]
    Unavailable(String),
    /// Backend returned a malformed response (un-parseable PEM,
    /// missing fields, etc.). Maps to `MintError::SignerDenied`
    /// — the backend is reachable but the result is unusable.
    #[error("signer returned malformed response: {0}")]
    Malformed(String),
}

/// Pluggable signing backend.
///
/// **Strategy** seam (per refactor S19): one impl per protocol.
/// V1 ships [`vault::VaultPkiSigner`]; AWS ACM PCA, Azure Key
/// Vault, SCEP/EST, and a webhook adapter follow as separate
/// strategies once customer demand drives them (ADR 034 §2.4).
///
/// Implementations are `Send + Sync` so the
/// [`ExternalCertMintService`] can hold them behind `Arc`.
pub trait ExternalSignerBackend: Send + Sync {
    /// Submit the CSR to the signing authority, return the
    /// signed cert + chain.
    fn sign_csr(
        &self,
        csr: CertificationRequest,
        ctx: SignContext,
    ) -> impl Future<Output = Result<CertChain, SignerError>> + Send;

    /// Stable backend name for audit / log output (e.g.
    /// `"vault-pki"`, `"acm-pca"`).
    fn name(&self) -> &'static str;
}

/// `CertMintService` that builds CSRs locally and delegates
/// signing to a pluggable [`ExternalSignerBackend`].
///
/// Clone is cheap (an `Arc` per field). Hold a single instance
/// per proxy and clone into the bridge — the rama cache layer
/// upstream still single-flights concurrent requests for the
/// same host (ADR 034 §2.3).
pub struct ExternalCertMintService<B>
where
    B: ExternalSignerBackend + 'static,
{
    backend: Arc<B>,
    timeout: Duration,
    audit_sink: Option<Arc<dyn SideEffectSink>>,
}

impl<B> ExternalCertMintService<B>
where
    B: ExternalSignerBackend + 'static,
{
    /// Build a mint service over `backend` with the
    /// `DEFAULT_SIGNER_TIMEOUT` (2s, per ADR 034 §3.3).
    #[must_use]
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            timeout: DEFAULT_SIGNER_TIMEOUT,
            audit_sink: None,
        }
    }

    /// Build a mint service over `backend` with an explicit
    /// per-mint timeout.
    #[must_use]
    pub fn with_timeout(backend: Arc<B>, timeout: Duration) -> Self {
        Self {
            backend,
            timeout,
            audit_sink: None,
        }
    }

    /// Install an audit sink. When set, every successful mint
    /// emits a `LeafMinted` audit event, every failure emits a
    /// `MintFailed` audit event (ADR 034 §5.4).
    #[must_use]
    pub fn with_audit_sink(mut self, sink: Arc<dyn SideEffectSink>) -> Self {
        self.audit_sink = Some(sink);
        self
    }

    /// Access the underlying backend. Exposed so callers that
    /// need both the mint service and the backend (e.g. for
    /// procurement that reuses the backend's connection pool)
    /// can re-use the same `Arc`.
    #[must_use]
    pub fn backend(&self) -> &Arc<B> {
        &self.backend
    }

    /// The per-mint timeout.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    fn emit_audit(&self, kind: AuditKind, detail: serde_json::Value) {
        if let Some(sink) = self.audit_sink.as_ref() {
            sink.record(SideEffect::Audit(AuditEvent {
                kind,
                layer: Layer::Tls,
                transform: self.backend.name().into(),
                // Mint operations are not flow-scoped; ADR 034
                // §5.4 carries `host` in the detail payload,
                // which is the operationally meaningful key.
                flow_id: 0,
                at_unix_ms: unix_ms_now(),
                detail,
                // Cert-mint audits do not pass through the engine
                // drain seam (no inspection flow), so no
                // correlation block is stamped. Consumers detect
                // this path by `flow_id == 0` per ADR 023.
                correlation: None,
            }));
        }
    }
}

impl<B> CertMintService for ExternalCertMintService<B>
where
    B: ExternalSignerBackend + 'static,
{
    #[allow(clippy::too_many_lines)]
    async fn mint_leaf(&self, request: LeafRequest) -> Result<LeafCert, MintError> {
        if request.server_name.is_empty() && request.upstream_san.is_empty() {
            let err =
                MintError::InvalidRequest("leaf request carries no server name and no SANs".into());
            self.emit_audit(
                AuditKind::MintFailed,
                serde_json::json!({
                    "event": "mint_failed",
                    "host": request.server_name,
                    "signer": self.backend.name(),
                    "error": err.to_string(),
                }),
            );
            return Err(err);
        }

        // Build the local keypair + CSR. The private key never
        // leaves this function frame except into the LeafCert
        // we return to the proxy — ADR 034 §5.2.
        let (csr_pem, key_pem, sans) = match build_csr(&request) {
            Ok(v) => v,
            Err(e) => {
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": request.server_name,
                        "signer": self.backend.name(),
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        };

        let cert_req = CertificationRequest {
            csr_pem,
            server_name: request.server_name.clone(),
            sans,
            cn: request.upstream_cn.clone(),
        };
        let ctx = SignContext {
            alpn: request.alpn.clone(),
        };

        let host = request.server_name.clone();
        let started = Instant::now();
        let backend = Arc::clone(&self.backend);
        let result = tokio::time::timeout(self.timeout, backend.sign_csr(cert_req, ctx)).await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let chain = match result {
            Ok(Ok(c)) => c,
            Ok(Err(SignerError::Denied(msg))) => {
                let err = MintError::SignerDenied(msg.clone());
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": host,
                        "signer": self.backend.name(),
                        "error": format!("SignerDenied: {msg}"),
                        "latency_ms": latency_ms,
                    }),
                );
                return Err(err);
            }
            Ok(Err(SignerError::Unavailable(msg))) => {
                let err = MintError::SignerUnavailable(msg.clone());
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": host,
                        "signer": self.backend.name(),
                        "error": format!("SignerUnavailable: {msg}"),
                        "latency_ms": latency_ms,
                    }),
                );
                return Err(err);
            }
            Ok(Err(SignerError::Malformed(msg))) => {
                let err = MintError::SignerDenied(format!("malformed signer response: {msg}"));
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": host,
                        "signer": self.backend.name(),
                        "error": format!("Malformed: {msg}"),
                        "latency_ms": latency_ms,
                    }),
                );
                return Err(err);
            }
            Err(_elapsed) => {
                let err = MintError::Timeout;
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": host,
                        "signer": self.backend.name(),
                        "error": "Timeout",
                        "latency_ms": latency_ms,
                    }),
                );
                return Err(err);
            }
        };

        // Parse the leaf's serial for audit; pem is DER so we
        // run x509-parser to read serial bytes. Failure to parse
        // the serial is not fatal — log "unknown" and continue.
        let serial_hex = leaf_serial_hex(&chain.leaf_der);

        // Assemble the cert chain: [leaf, intermediates...]. The
        // signer's `chain_der` is already in leaf-presenting
        // order.
        let mut cert_chain = Vec::with_capacity(1 + chain.chain_der.len());
        cert_chain.push(chain.leaf_der);
        cert_chain.extend(chain.chain_der);

        // Convert the rcgen-emitted PEM private key into DER for
        // the LeafCert. rcgen serializes PKCS#8 PEM, which is
        // what BoringSSL wants in DER form.
        let key_der = match pem_pkcs8_to_der(&key_pem) {
            Ok(b) => b,
            Err(e) => {
                let err =
                    MintError::SignerDenied(format!("private key DER conversion failed: {e}"));
                self.emit_audit(
                    AuditKind::MintFailed,
                    serde_json::json!({
                        "event": "mint_failed",
                        "host": host,
                        "signer": self.backend.name(),
                        "error": err.to_string(),
                        "latency_ms": latency_ms,
                    }),
                );
                return Err(err);
            }
        };

        self.emit_audit(
            AuditKind::LeafMinted,
            serde_json::json!({
                "event": "leaf_minted",
                "host": host,
                "signer": self.backend.name(),
                "latency_ms": latency_ms,
                "cached": false,
                "serial": serial_hex,
            }),
        );

        Ok(LeafCert {
            cert_chain,
            private_key_der: key_der,
        })
    }
}

/// Build the local keypair and PKCS#10 CSR (PEM) for `request`.
///
/// Returns `(csr_pem, key_pem, sans)`. The SAN list is returned
/// alongside so callers can populate
/// [`CertificationRequest::sans`] without re-parsing.
///
/// CSR contents:
///
/// - Subject: CN = upstream CN or `server_name`.
/// - SANs: every upstream SAN + the `server_name` (deduplicated).
/// - Key usages: digitalSignature, keyEncipherment.
/// - Extended key usages: serverAuth.
///
/// The leaf's validity window is set by the signer, not the CSR;
/// rcgen requires `not_before` / `not_after` to construct
/// `CertificateParams` so we set them to sensible defaults
/// (90d + 1h skew) — Vault PKI ignores these in favour of role
/// policy, but signers that pass them through get sane lifetimes.
fn build_csr(request: &LeafRequest) -> Result<(String, String, Vec<String>), MintError> {
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

    let mut params = CertificateParams::new(sans.clone())
        .map_err(|e| MintError::InvalidRequest(format!("invalid SAN list: {e}")))?;

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
    params.not_before = now - TimeDuration::hours(LEAF_SKEW_HOURS);
    params.not_after = now + TimeDuration::days(LEAF_VALIDITY_DAYS);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let csr_pem = params
        .serialize_request(&leaf_key)
        .map_err(|e| MintError::SignerDenied(format!("CSR construction failed: {e}")))?
        .pem()
        .map_err(|e| MintError::SignerDenied(format!("CSR PEM encoding failed: {e}")))?;
    let key_pem = leaf_key.serialize_pem();
    Ok((csr_pem, key_pem, sans))
}

/// Extract the leaf's serial number in hex (colon-separated)
/// for audit emission. Returns `"unknown"` if parsing fails —
/// we never want serial parsing to fail the mint.
fn leaf_serial_hex(der: &[u8]) -> String {
    use std::fmt::Write as _;
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(der) else {
        return "unknown".to_string();
    };
    let bytes = parsed.tbs_certificate.raw_serial();
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Convert a PEM-wrapped PKCS#8 private key to DER bytes.
fn pem_pkcs8_to_der(pem_str: &str) -> Result<Vec<u8>, String> {
    let block = pem::parse(pem_str).map_err(|e| format!("parse pem: {e}"))?;
    Ok(block.into_contents())
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodle_core::layered::SideEffect;
    use noodle_sinks::InMemorySink;
    use std::sync::Mutex;
    use x509_parser::certification_request::X509CertificationRequest;
    use x509_parser::prelude::FromDer;

    /// Test backend: records every received CSR and returns the
    /// supplied response. Lets unit tests assert that the leaf
    /// keypair never leaks (the recorded CSR carries only the
    /// public key) and the request shape matches expectations.
    struct CapturingBackend {
        responses: Mutex<Vec<Result<CertChain, SignerError>>>,
        received: Mutex<Vec<CertificationRequest>>,
        contexts: Mutex<Vec<SignContext>>,
    }

    impl CapturingBackend {
        fn new(responses: Vec<Result<CertChain, SignerError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                received: Mutex::new(Vec::new()),
                contexts: Mutex::new(Vec::new()),
            }
        }

        fn last_csr(&self) -> CertificationRequest {
            self.received
                .lock()
                .expect("lock")
                .last()
                .cloned()
                .expect("at least one CSR received")
        }

        fn call_count(&self) -> usize {
            self.received.lock().expect("lock").len()
        }
    }

    impl ExternalSignerBackend for CapturingBackend {
        fn name(&self) -> &'static str {
            "capturing-test-backend"
        }

        async fn sign_csr(
            &self,
            csr: CertificationRequest,
            ctx: SignContext,
        ) -> Result<CertChain, SignerError> {
            self.received.lock().expect("lock").push(csr);
            self.contexts.lock().expect("lock").push(ctx);
            let mut r = self.responses.lock().expect("lock");
            if r.is_empty() {
                return Err(SignerError::Unavailable("no more canned responses".into()));
            }
            r.remove(0)
        }
    }

    #[tokio::test]
    async fn external_mint_generates_keypair_locally_and_sends_csr_only() {
        // The leaf private key MUST never appear in the CSR sent
        // to the signer — only the CSR (with public key) goes
        // upstream. Property: scan the bytes the backend
        // received; assert no PEM private-key block appears.
        let backend = Arc::new(CapturingBackend::new(vec![]));
        let svc = ExternalCertMintService::new(Arc::clone(&backend));

        // We don't actually need the response to succeed for
        // this assertion — once the backend has the CSR we can
        // inspect it. Hand it an Unavailable response so we
        // return cleanly.
        backend
            .responses
            .lock()
            .unwrap()
            .push(Err(SignerError::Unavailable("don't care".into())));

        let _ = svc
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                Some("api.anthropic.com".into()),
                vec![b"h2".to_vec()],
            ))
            .await;

        let csr = backend.last_csr();
        // The CSR PEM contains a "BEGIN CERTIFICATE REQUEST" block
        // and never a private-key block.
        assert!(
            csr.csr_pem.contains("BEGIN CERTIFICATE REQUEST"),
            "must send a PEM CSR; got:\n{}",
            csr.csr_pem
        );
        assert!(
            !csr.csr_pem.contains("PRIVATE KEY"),
            "CSR must not leak private key material; got:\n{}",
            csr.csr_pem
        );
        // Server name + SANs propagate.
        assert_eq!(csr.server_name, "api.anthropic.com");
        assert!(csr.sans.contains(&"api.anthropic.com".to_string()));
        assert_eq!(csr.cn.as_deref(), Some("api.anthropic.com"));
    }

    #[tokio::test]
    async fn external_mint_returns_chain_when_backend_succeeds() {
        // First call: synthesize the chain for whatever CSR the
        // mint service generated. We need a two-phase fixture
        // because the CSR PEM is built per-call; pre-canning
        // a response would mismatch. We use a custom backend
        // that signs each CSR with a throwaway test CA.
        struct AutoSignBackend {
            ca_arc: Arc<noodle_tls::ca::Ca>,
            calls: Mutex<u32>,
        }
        impl ExternalSignerBackend for AutoSignBackend {
            fn name(&self) -> &'static str {
                "auto-sign-test-backend"
            }
            async fn sign_csr(
                &self,
                csr: CertificationRequest,
                _ctx: SignContext,
            ) -> Result<CertChain, SignerError> {
                *self.calls.lock().unwrap() += 1;
                let (issuer_cert, issuer_key) = self.ca_arc.issuer_handles();
                let req = rcgen::CertificateSigningRequestParams::from_pem(&csr.csr_pem)
                    .map_err(|e| SignerError::Malformed(format!("CSR parse: {e}")))?;
                let leaf = req
                    .signed_by(issuer_cert, issuer_key)
                    .map_err(|e| SignerError::Denied(format!("sign: {e}")))?;
                Ok(CertChain {
                    leaf_der: leaf.der().to_vec(),
                    chain_der: vec![],
                })
            }
        }

        let ca = Arc::new(noodle_tls::ca::Ca::generate().expect("test CA"));
        let backend = Arc::new(AutoSignBackend {
            ca_arc: Arc::clone(&ca),
            calls: Mutex::new(0),
        });
        let svc = ExternalCertMintService::new(Arc::clone(&backend));
        let leaf = svc
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                None,
                vec![],
            ))
            .await
            .expect("mint");
        assert!(!leaf.cert_chain.is_empty(), "non-empty chain");
        assert!(!leaf.private_key_der.is_empty(), "private key present");

        // Verify the leaf chains to the test CA.
        let (_, leaf_parsed) =
            x509_parser::parse_x509_certificate(&leaf.cert_chain[0]).expect("parse leaf");
        let ca_pem = ca.cert_pem();
        let ca_der = pem::parse(ca_pem).expect("parse ca pem").into_contents();
        let (_, ca_parsed) = x509_parser::parse_x509_certificate(&ca_der).expect("parse ca");
        assert_eq!(
            leaf_parsed.issuer().to_string(),
            ca_parsed.subject().to_string(),
            "external-mode leaf must be signed by the (stub) signer's CA"
        );
        leaf_parsed
            .verify_signature(Some(ca_parsed.public_key()))
            .expect("leaf signature verifies");
    }

    #[tokio::test]
    async fn external_mint_emits_leaf_minted_audit_event_on_success() {
        struct AutoSignBackend {
            ca: Arc<noodle_tls::ca::Ca>,
        }
        impl ExternalSignerBackend for AutoSignBackend {
            fn name(&self) -> &'static str {
                "vault-pki"
            }
            async fn sign_csr(
                &self,
                csr: CertificationRequest,
                _ctx: SignContext,
            ) -> Result<CertChain, SignerError> {
                let (cert, key) = self.ca.issuer_handles();
                let req = rcgen::CertificateSigningRequestParams::from_pem(&csr.csr_pem)
                    .map_err(|e| SignerError::Malformed(e.to_string()))?;
                let leaf = req
                    .signed_by(cert, key)
                    .map_err(|e| SignerError::Denied(e.to_string()))?;
                Ok(CertChain {
                    leaf_der: leaf.der().to_vec(),
                    chain_der: vec![],
                })
            }
        }
        let sink = Arc::new(InMemorySink::new());
        let ca = Arc::new(noodle_tls::ca::Ca::generate().unwrap());
        let backend = Arc::new(AutoSignBackend {
            ca: Arc::clone(&ca),
        });
        let svc = ExternalCertMintService::new(Arc::clone(&backend))
            .with_audit_sink(Arc::clone(&sink) as Arc<dyn SideEffectSink>);

        svc.mint_leaf(LeafRequest::new(
            "api.anthropic.com",
            vec!["api.anthropic.com".into()],
            None,
            vec![],
        ))
        .await
        .expect("mint");

        let effects = sink.snapshot();
        let audit = effects.iter().find_map(|e| match e {
            SideEffect::Audit(a) if a.kind == AuditKind::LeafMinted => Some(a),
            _ => None,
        });
        assert!(audit.is_some(), "leaf_minted audit must fire");
        let a = audit.unwrap();
        assert_eq!(a.transform.as_str(), "vault-pki");
        let detail = &a.detail;
        assert_eq!(detail["host"], "api.anthropic.com");
        assert_eq!(detail["signer"], "vault-pki");
        assert!(detail["latency_ms"].is_number());
        assert_eq!(detail["cached"], false);
        let serial = detail["serial"].as_str().expect("serial string");
        // Hex serial like "ab:cd:..." with at least one byte.
        assert!(!serial.is_empty());
    }

    #[tokio::test]
    async fn external_mint_emits_mint_failed_audit_event_on_unavailable() {
        let sink = Arc::new(InMemorySink::new());
        let backend = Arc::new(CapturingBackend::new(vec![Err(SignerError::Unavailable(
            "connection refused".into(),
        ))]));
        let svc = ExternalCertMintService::new(Arc::clone(&backend))
            .with_audit_sink(Arc::clone(&sink) as Arc<dyn SideEffectSink>);

        let err = svc
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                None,
                vec![],
            ))
            .await
            .expect_err("must fail");
        assert!(matches!(err, MintError::SignerUnavailable(_)));

        let effects = sink.snapshot();
        let audit = effects.iter().find_map(|e| match e {
            SideEffect::Audit(a) if a.kind == AuditKind::MintFailed => Some(a),
            _ => None,
        });
        assert!(audit.is_some(), "mint_failed audit must fire");
        let detail = &audit.unwrap().detail;
        assert_eq!(detail["host"], "api.anthropic.com");
        assert!(
            detail["error"]
                .as_str()
                .unwrap()
                .contains("connection refused")
        );
    }

    #[tokio::test]
    async fn external_mint_returns_timeout_when_backend_hangs() {
        struct HangingBackend;
        impl ExternalSignerBackend for HangingBackend {
            fn name(&self) -> &'static str {
                "hanging"
            }
            async fn sign_csr(
                &self,
                _csr: CertificationRequest,
                _ctx: SignContext,
            ) -> Result<CertChain, SignerError> {
                // Sleep way longer than the timeout.
                tokio::time::sleep(Duration::from_mins(1)).await;
                Err(SignerError::Unavailable("never returns".into()))
            }
        }
        let svc = ExternalCertMintService::with_timeout(
            Arc::new(HangingBackend),
            Duration::from_millis(20),
        );
        let err = svc
            .mint_leaf(LeafRequest::new(
                "api.anthropic.com",
                vec!["api.anthropic.com".into()],
                None,
                vec![],
            ))
            .await
            .expect_err("must timeout");
        assert!(matches!(err, MintError::Timeout), "got {err:?}");
    }

    #[tokio::test]
    async fn external_mint_rejects_empty_request() {
        let backend = Arc::new(CapturingBackend::new(vec![]));
        let svc = ExternalCertMintService::new(Arc::clone(&backend));
        let err = svc
            .mint_leaf(LeafRequest::new("", vec![], None, vec![]))
            .await
            .expect_err("must reject");
        assert!(matches!(err, MintError::InvalidRequest(_)));
        assert_eq!(
            backend.call_count(),
            0,
            "backend must not be called for invalid request"
        );
    }

    // ─── Property test: CSR carries SAN/CN/server_name with no leakage ───

    use proptest::prelude::*;
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// For any well-formed LeafRequest (server_name + 0..5
        /// upstream SANs + optional CN), the CSR PEM must:
        ///   1. parse as a valid PKCS#10 CSR
        ///   2. contain every SAN DNS name we asked for
        ///   3. contain the CN we asked for
        ///   4. NOT contain a "BEGIN ... PRIVATE KEY" block
        #[test]
        fn property_csr_pem_contains_all_san_and_cn_dns_names(
            host in "[a-z]{3,12}\\.example\\.com",
            sans in proptest::collection::vec("[a-z]{3,10}\\.example\\.com", 0..5),
            cn in proptest::option::of("[a-z]{3,10}"),
        ) {
            let req = LeafRequest::new(host.clone(), sans.clone(), cn.clone(), vec![]);
            let (csr_pem, _key_pem, returned_sans) = build_csr(&req)
                .expect("CSR build must succeed for well-formed input");

            // (4) no private key leakage
            prop_assert!(
                !csr_pem.contains("PRIVATE KEY"),
                "CSR PEM leaks private key"
            );
            // (1) is a CSR PEM
            prop_assert!(
                csr_pem.contains("BEGIN CERTIFICATE REQUEST"),
                "expected CSR PEM, got: {csr_pem}"
            );

            // (2,3) For SAN + CN inspection we use the rcgen-emitted CSR
            //   PEM directly. The CSR text contains both DER-only
            //   bits (we can't inspect those from text) and the
            //   subject CN as readable ASCII via the subject DER.
            //   We parse the CSR DER through x509-parser's
            //   `X509CertificationRequest` type.
            let csr_der = pem::parse(&csr_pem)
                .expect("parse csr pem")
                .into_contents();
            let (_, parsed) = X509CertificationRequest::from_der(&csr_der).expect("parse csr der");

            // SAN extension lives in extension requests
            let mut found_dns: Vec<String> = Vec::new();
            if let Some(extensions) = parsed.requested_extensions() {
                for ext in extensions {
                    if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(
                        san_ext,
                    ) = ext {
                        for gn in &san_ext.general_names {
                            if let x509_parser::extensions::GeneralName::DNSName(name) = gn {
                                found_dns.push((*name).to_string());
                            }
                        }
                    }
                }
            }

            // The CSR's SANs must equal the returned_sans set
            // (which is what build_csr asserts on the wire).
            for s in &returned_sans {
                prop_assert!(
                    found_dns.iter().any(|f| f == s),
                    "SAN {s} missing from CSR; got {found_dns:?}"
                );
            }
            // And the host itself must be in there.
            prop_assert!(
                found_dns.iter().any(|f| f == &host),
                "server_name {host} missing from CSR SANs; got {found_dns:?}"
            );

            // CN check.
            let expected_cn = cn.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| host.clone());
            let cn_iter = parsed
                .certification_request_info
                .subject
                .iter_common_name()
                .filter_map(|c| c.as_str().ok())
                .collect::<Vec<_>>();
            prop_assert!(
                cn_iter.contains(&expected_cn.as_str()),
                "expected CN {expected_cn} in subject; got {cn_iter:?}"
            );
        }
    }
}
