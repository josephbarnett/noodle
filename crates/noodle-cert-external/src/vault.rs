//! `HashiCorp` Vault PKI backend for [`super::ExternalSignerBackend`]
//! (ADR 034 §2.4, S19).
//!
//! Submits the CSR to Vault's PKI secrets engine via HTTPS, parses
//! the signed leaf + chain from the response.
//!
//! ## Wire shape
//!
//! Vault PKI exposes `POST $endpoint` (typically
//! `https://vault.corp.example.com:8200/v1/pki/sign/<role>`) with
//! a JSON body:
//!
//! ```json
//! { "csr": "-----BEGIN CERTIFICATE REQUEST-----\n..." }
//! ```
//!
//! Success returns 200 with a payload of the shape:
//!
//! ```json
//! {
//!   "data": {
//!     "certificate": "-----BEGIN CERTIFICATE-----\n...",
//!     "issuing_ca": "-----BEGIN CERTIFICATE-----\n...",
//!     "ca_chain": ["-----BEGIN CERTIFICATE-----\n...", ...],
//!     "serial_number": "ab:cd:..."
//!   }
//! }
//! ```
//!
//! `ca_chain` is preferred; falls back to `issuing_ca` if absent.
//!
//! ## Auth
//!
//! Two modes (ADR 034 §5.3):
//!
//! - **token**: `X-Vault-Token: <token>` header. Token loaded from
//!   `token_path` at construction time. Mode 0600 required on Unix
//!   for the token file (parity with `ca.key` permission policy).
//! - **mtls**: client cert + key loaded from disk; reqwest's
//!   `Identity` carries them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use super::{CertChain, CertificationRequest, ExternalSignerBackend, SignContext, SignerError};

/// Default per-request HTTP timeout for the Vault client. The
/// outer [`super::ExternalCertMintService::timeout`] is the mint-
/// level budget; this is the lower-level HTTP timeout so a stuck
/// TCP connect or TLS handshake doesn't dominate the budget.
const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// How to authenticate to Vault.
///
/// Constructed via the [`VaultPkiSigner::with_token`] /
/// [`VaultPkiSigner::with_mtls`] builders so file-loading + 0600
/// checks happen at construction time, not on every mint.
pub enum VaultAuth {
    /// Bearer-token mode. `token` is the raw Vault token string;
    /// sent as `X-Vault-Token` on every request.
    Token { token: String },
    /// mTLS mode. `identity` is a reqwest `Identity` built from
    /// the operator's client cert + key.
    Mtls { identity: reqwest::Identity },
}

/// Vault PKI signer.
///
/// Holds:
/// - The endpoint URL to POST CSRs to (`/v1/pki/sign/<role>` in
///   typical deployments).
/// - The HTTP client configured with auth + (optional) trust
///   bundle for Vault's server cert.
///
/// Cheap to clone (one `Arc` per inner client); the proxy holds a
/// single instance and shares it through the rama cache layer.
pub struct VaultPkiSigner {
    endpoint: String,
    client: reqwest::Client,
    /// When `Token { .. }` mode, the bearer goes in a request
    /// header. mTLS auth is plumbed into the client; this field
    /// stays `None` to keep the per-request hot path branch-free.
    bearer_token: Option<String>,
}

/// Errors that can occur while building a [`VaultPkiSigner`].
///
/// Distinct from [`SignerError`] because build-time failures are
/// configuration problems the operator must fix before startup;
/// they should not surface as a generic "signer unavailable" at
/// runtime.
#[derive(Debug, Error)]
pub enum VaultBuildError {
    /// Filesystem I/O failure while reading credentials.
    #[error("io error reading {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Token / cert file has loose permissions. ADR 034 §5.3:
    /// noodle requires `token_path` and `client_key` to be 0600
    /// (or stricter) on Unix.
    #[error("file {path} has mode {mode:o}; required 0600 or stricter")]
    InsecurePermissions {
        /// Filesystem path to the offending file.
        path: PathBuf,
        /// Mode bits (lower 9 of the file permissions).
        mode: u32,
    },
    /// Vault expects an identity PEM with both a cert + key; we
    /// concatenate them on disk read. This error surfaces if the
    /// PEM round-trip through reqwest fails (malformed PEM,
    /// unsupported algorithm).
    #[error("malformed identity for mTLS: {0}")]
    MalformedIdentity(String),
    /// reqwest client construction failed (e.g. invalid TLS
    /// bundle).
    #[error("reqwest client build failed: {0}")]
    Client(reqwest::Error),
}

impl VaultPkiSigner {
    /// Build a Vault PKI signer with bearer-token auth.
    ///
    /// `token_path` must point to a file containing the Vault
    /// token (trimmed of trailing whitespace). On Unix the file
    /// must be mode `0600` (or stricter); otherwise build fails.
    ///
    /// `ca_cert_pem` is an optional CA bundle (PEM) used to
    /// verify Vault's server certificate. Pass `None` to use
    /// reqwest's built-in trust roots (suitable when Vault's
    /// server cert is signed by a public CA or a CA already in
    /// the system trust store).
    pub fn with_token(
        endpoint: impl Into<String>,
        token_path: &Path,
        ca_cert_pem: Option<&[u8]>,
        http_timeout: Option<Duration>,
    ) -> Result<Self, VaultBuildError> {
        check_secure_permissions(token_path)?;
        let token = std::fs::read_to_string(token_path).map_err(|source| VaultBuildError::Io {
            path: token_path.to_path_buf(),
            source,
        })?;
        let token = token.trim().to_string();

        let mut builder =
            reqwest::Client::builder().timeout(http_timeout.unwrap_or(DEFAULT_HTTP_TIMEOUT));
        if let Some(pem_bytes) = ca_cert_pem {
            let cert =
                reqwest::Certificate::from_pem(pem_bytes).map_err(VaultBuildError::Client)?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(VaultBuildError::Client)?;

        Ok(Self {
            endpoint: endpoint.into(),
            client,
            bearer_token: Some(token),
        })
    }

    /// Build a Vault PKI signer with mTLS auth.
    ///
    /// `client_cert_path` is a PEM-encoded client certificate
    /// (single block or chain). `client_key_path` is a PEM-
    /// encoded PKCS#8 private key. On Unix the key file must be
    /// `0600` or stricter; the cert file's permissions are not
    /// enforced (public material).
    pub fn with_mtls(
        endpoint: impl Into<String>,
        client_cert_path: &Path,
        client_key_path: &Path,
        ca_cert_pem: Option<&[u8]>,
        http_timeout: Option<Duration>,
    ) -> Result<Self, VaultBuildError> {
        check_secure_permissions(client_key_path)?;
        let cert_pem = std::fs::read(client_cert_path).map_err(|source| VaultBuildError::Io {
            path: client_cert_path.to_path_buf(),
            source,
        })?;
        let key_pem = std::fs::read(client_key_path).map_err(|source| VaultBuildError::Io {
            path: client_key_path.to_path_buf(),
            source,
        })?;
        // reqwest's Identity::from_pem expects cert+key concatenated.
        let mut bundle = Vec::with_capacity(cert_pem.len() + key_pem.len() + 1);
        bundle.extend_from_slice(&cert_pem);
        bundle.push(b'\n');
        bundle.extend_from_slice(&key_pem);
        let identity = reqwest::Identity::from_pem(&bundle)
            .map_err(|e| VaultBuildError::MalformedIdentity(e.to_string()))?;

        let mut builder = reqwest::Client::builder()
            .identity(identity)
            .timeout(http_timeout.unwrap_or(DEFAULT_HTTP_TIMEOUT));
        if let Some(pem_bytes) = ca_cert_pem {
            let cert =
                reqwest::Certificate::from_pem(pem_bytes).map_err(VaultBuildError::Client)?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(VaultBuildError::Client)?;
        Ok(Self {
            endpoint: endpoint.into(),
            client,
            bearer_token: None,
        })
    }

    /// Construct a signer from a pre-built `Client` + `VaultAuth`.
    ///
    /// Test-and-tooling escape hatch: lets unit tests supply a
    /// mock-friendly `Client` (e.g. a wiremock-backed
    /// `reqwest::Client`) without going through the file-loading
    /// path. The on-disk loaders (`with_token`, `with_mtls`)
    /// remain the supported binary path.
    #[must_use]
    pub fn from_parts(endpoint: String, client: reqwest::Client, auth: VaultAuth) -> Self {
        let bearer_token = match auth {
            VaultAuth::Token { token } => Some(token),
            VaultAuth::Mtls { .. } => None,
        };
        Self {
            endpoint,
            client,
            bearer_token,
        }
    }
}

/// JSON envelope returned by Vault PKI's sign endpoint.
#[derive(Debug, Deserialize)]
struct VaultPkiResponse {
    data: VaultPkiData,
}

#[derive(Debug, Deserialize)]
struct VaultPkiData {
    /// PEM-encoded signed leaf.
    certificate: String,
    /// Optional issuing CA (single PEM). Vault returns this for
    /// every successful sign; we keep it as a fallback when
    /// `ca_chain` is absent.
    #[serde(default)]
    issuing_ca: Option<String>,
    /// Optional intermediate chain. When present, contains zero
    /// or more PEM blocks in leaf-presenting order.
    #[serde(default)]
    ca_chain: Option<Vec<String>>,
}

/// Vault error response (HTTP 4xx/5xx with JSON body):
/// `{"errors": ["msg1", "msg2"]}`.
#[derive(Debug, Deserialize)]
struct VaultErrorResponse {
    #[serde(default)]
    errors: Vec<String>,
}

impl ExternalSignerBackend for VaultPkiSigner {
    fn name(&self) -> &'static str {
        "vault-pki"
    }

    async fn sign_csr(
        &self,
        csr: CertificationRequest,
        _ctx: SignContext,
    ) -> Result<CertChain, SignerError> {
        let body = serde_json::json!({ "csr": csr.csr_pem });
        let mut req = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json");
        if let Some(token) = self.bearer_token.as_deref() {
            req = req.header("X-Vault-Token", token);
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| SignerError::Unavailable(format!("vault HTTP transport: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            // 4xx is denial; 5xx is unavailable. ADR 034 §3.2.
            let bytes = resp.bytes().await.unwrap_or_default();
            let msg = match serde_json::from_slice::<VaultErrorResponse>(&bytes) {
                Ok(parsed) if !parsed.errors.is_empty() => parsed.errors.join("; "),
                _ => String::from_utf8_lossy(&bytes).into_owned(),
            };
            if status.is_client_error() {
                return Err(SignerError::Denied(format!("vault {status}: {msg}")));
            }
            return Err(SignerError::Unavailable(format!("vault {status}: {msg}")));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SignerError::Unavailable(format!("vault body read: {e}")))?;
        let parsed: VaultPkiResponse = serde_json::from_slice(&bytes)
            .map_err(|e| SignerError::Malformed(format!("vault response JSON: {e}")))?;

        let leaf_der = pem::parse(parsed.data.certificate.as_bytes())
            .map_err(|e| SignerError::Malformed(format!("leaf PEM: {e}")))?
            .into_contents();

        let mut chain_der: Vec<Vec<u8>> = Vec::new();
        if let Some(chain) = parsed.data.ca_chain {
            for (i, c) in chain.into_iter().enumerate() {
                let der = pem::parse(c.as_bytes())
                    .map_err(|e| SignerError::Malformed(format!("ca_chain[{i}] PEM: {e}")))?
                    .into_contents();
                chain_der.push(der);
            }
        } else if let Some(issuing) = parsed.data.issuing_ca {
            // Fall back to `issuing_ca` for Vault setups that
            // don't populate `ca_chain` (e.g. single-CA mounts).
            let der = pem::parse(issuing.as_bytes())
                .map_err(|e| SignerError::Malformed(format!("issuing_ca PEM: {e}")))?
                .into_contents();
            chain_der.push(der);
        }

        Ok(CertChain {
            leaf_der,
            chain_der,
        })
    }
}

/// Refuse to load the file when permissions are looser than
/// 0600 on Unix. No-op on non-Unix (ACL-based; out of scope for
/// v1, see ADR 034 §4.2 footnote).
fn check_secure_permissions(path: &Path) -> Result<(), VaultBuildError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|source| VaultBuildError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(VaultBuildError::InsecurePermissions {
                path: path.to_path_buf(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Build a `VaultPkiSigner` over `from_parts` with token
    /// auth so unit tests can drive the request/response path
    /// without writing to disk.
    fn signer_for_endpoint(endpoint: &str) -> VaultPkiSigner {
        VaultPkiSigner::from_parts(
            endpoint.to_string(),
            reqwest::Client::new(),
            VaultAuth::Token {
                token: "test-token".into(),
            },
        )
    }

    #[tokio::test]
    async fn vault_signer_sends_csr_with_x_vault_token_header() {
        // Spin up a tiny HTTP server with `tokio::net` that
        // captures the body + headers of one request, then
        // returns a synthetic signed response.
        use std::sync::Mutex;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let captured = Arc::new(Mutex::new(None::<(String, String)>));
        let captured_for_task = Arc::clone(&captured);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = sock.split();
            let mut reader = BufReader::new(read_half);
            let mut request_line = String::new();
            reader
                .read_line(&mut request_line)
                .await
                .expect("read request line");
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read header");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let line = line.trim_end_matches("\r\n");
                if let Some((name, value)) = line.split_once(':') {
                    let n = name.trim().to_string();
                    let v = value.trim().to_string();
                    if n.eq_ignore_ascii_case("content-length") {
                        content_length = v.parse().unwrap_or(0);
                    }
                    headers.push((n, v));
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await.expect("read body");
            let token_hdr = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("x-vault-token"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let body_str = String::from_utf8(body).expect("utf8");
            *captured_for_task.lock().unwrap() = Some((token_hdr, body_str));

            // Synthetic response: a fixed signed leaf produced by
            // a one-shot test CA. We DO need a parseable PEM
            // certificate here for the test to assert the
            // signer's response-parse path works end-to-end.
            let ca = noodle_tls::ca::Ca::generate().expect("test CA");
            // Re-issue the CA as the "leaf" for the response —
            // it's a self-signed cert structurally, parses as
            // PEM. The test only checks the parse path.
            let leaf_pem = ca.cert_pem().to_string();
            let issuing_pem = ca.cert_pem().to_string();
            let resp_body = serde_json::json!({
                "data": {
                    "certificate": leaf_pem,
                    "issuing_ca": issuing_pem,
                }
            });
            let resp_body_str = serde_json::to_string(&resp_body).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                resp_body_str.len(),
                resp_body_str
            );
            write_half
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let endpoint = format!("http://{addr}/v1/pki/sign/noodle-leaf");
        let signer = signer_for_endpoint(&endpoint);
        let csr = CertificationRequest {
            csr_pem:
                "-----BEGIN CERTIFICATE REQUEST-----\nFAKE\n-----END CERTIFICATE REQUEST-----\n"
                    .to_string(),
            server_name: "api.anthropic.com".into(),
            sans: vec!["api.anthropic.com".into()],
            cn: Some("api.anthropic.com".into()),
        };
        let chain = signer
            .sign_csr(csr, SignContext::default())
            .await
            .expect("sign");
        assert!(!chain.leaf_der.is_empty(), "leaf DER present");
        assert_eq!(chain.chain_der.len(), 1, "issuing_ca fallback yields chain");
        // Wait for the server task to record what we sent.
        server.await.expect("server task");
        let (token_hdr, body_str) = captured.lock().unwrap().clone().expect("captured");
        assert_eq!(token_hdr, "test-token", "X-Vault-Token must be set");
        let parsed: serde_json::Value = serde_json::from_str(&body_str).expect("body is JSON");
        assert!(
            parsed["csr"]
                .as_str()
                .unwrap_or("")
                .contains("BEGIN CERTIFICATE REQUEST"),
            "body must carry the CSR PEM; got {body_str}"
        );
        // Critical: the request body must NOT contain a private key
        // block — the leaf private key never crosses the wire.
        assert!(
            !body_str.contains("PRIVATE KEY"),
            "request body leaked private key:\n{body_str}"
        );
    }

    #[tokio::test]
    async fn vault_signer_maps_5xx_to_unavailable() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let (read_half, mut write_half) = sock.split();
            let mut reader = BufReader::new(read_half);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).await.unwrap();
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((n, v)) = line.split_once(':')
                    && n.eq_ignore_ascii_case("content-length")
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await.unwrap();
            let body = "{\"errors\":[\"upstream PKI is down\"]}";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                body.len(),
                body
            );
            write_half.write_all(response.as_bytes()).await.unwrap();
        });

        let signer = signer_for_endpoint(&format!("http://{addr}/v1/pki/sign/x"));
        let csr = CertificationRequest {
            csr_pem:
                "-----BEGIN CERTIFICATE REQUEST-----\nFAKE\n-----END CERTIFICATE REQUEST-----\n"
                    .to_string(),
            server_name: "h".into(),
            sans: vec![],
            cn: None,
        };
        let err = signer
            .sign_csr(csr, SignContext::default())
            .await
            .expect_err("must fail");
        match err {
            SignerError::Unavailable(msg) => {
                assert!(msg.contains("upstream PKI is down"), "got: {msg}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn vault_signer_maps_4xx_to_denied() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = sock.split();
            let mut reader = BufReader::new(read_half);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).await.unwrap();
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((n, v)) = line.split_once(':')
                    && n.eq_ignore_ascii_case("content-length")
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await.unwrap();
            let body = "{\"errors\":[\"role not found\"]}";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
                body.len(),
                body
            );
            write_half.write_all(response.as_bytes()).await.unwrap();
        });

        let signer = signer_for_endpoint(&format!("http://{addr}/v1/pki/sign/x"));
        let csr = CertificationRequest {
            csr_pem:
                "-----BEGIN CERTIFICATE REQUEST-----\nFAKE\n-----END CERTIFICATE REQUEST-----\n"
                    .to_string(),
            server_name: "h".into(),
            sans: vec![],
            cn: None,
        };
        let err = signer
            .sign_csr(csr, SignContext::default())
            .await
            .expect_err("must fail");
        match err {
            SignerError::Denied(msg) => assert!(msg.contains("role not found"), "got: {msg}"),
            other => panic!("expected Denied, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn with_token_rejects_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault-token");
        std::fs::write(&path, "s.hvs.AAAAAAAA").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        match VaultPkiSigner::with_token("https://vault.example/v1/pki/sign/r", &path, None, None) {
            Err(VaultBuildError::InsecurePermissions { .. }) => {}
            Err(other) => panic!("expected InsecurePermissions; got {other:?}"),
            Ok(_) => panic!("expected build to reject 0644 token"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn with_token_accepts_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vault-token");
        std::fs::write(&path, "s.hvs.AAAAAAAA").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        match VaultPkiSigner::with_token("https://vault.example/v1/pki/sign/r", &path, None, None) {
            Ok(_) => {}
            Err(e) => panic!("0600 token must load; got {e:?}"),
        }
    }
}
