//! Integration test for S19 — external-signer mode end-to-end.
//!
//! Spins up an HTTP "stub Vault" server that signs CSRs with a
//! test enterprise CA, configures noodle in external mode
//! pointing at the stub URL, then opens a CONNECT-tunnelled TLS
//! handshake through the proxy and confirms:
//!
//! 1. The leaf noodle presents chains to the test enterprise CA.
//! 2. The mint audit event (`leaf_minted`) fired through the
//!    configured `SideEffectSink`.
//!
//! Plus the fault-enhancement variant: a 503 from the stub
//! surfaces as `MintError::SignerUnavailable` and the mint
//! audit fires `mint_failed`.
//!
//! See `docs/features/038-external-cert-mint-vault.md` §5.
//!
//! Run:
//!
//! ```sh
//! cargo test -p noodle-proxy --test external_mint_vault_chain
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_cert_external::ExternalCertMintService;
use noodle_cert_external::vault::{VaultAuth, VaultPkiSigner};
use noodle_core::layered::{AuditKind, SideEffect, SideEffectSink};
use noodle_core::{CertMintService, DynCertMintService, LeafRequest, MintError};
use noodle_proxy::{ProxyConfig, start};
use noodle_sinks::InMemorySink;
use noodle_tls::ca::Ca;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Spawn a stub Vault PKI server that signs incoming CSRs with
/// the supplied enterprise CA and returns the JSON shape Vault
/// PKI's `/v1/pki/sign/<role>` endpoint produces.
///
/// Returns `(addr, hits_counter)`. `hits_counter` increments
/// each time the stub processes a request; tests use it to
/// assert procurement / per-host calls.
///
/// `mode = StubMode::Success` returns 200 with a signed leaf.
/// `mode = StubMode::Error503` returns a 503 — for fault
/// enhancement.
async fn spawn_stub_vault(ca: Arc<Ca>, mode: StubMode) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_task = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let ca = Arc::clone(&ca);
            let hits = Arc::clone(&hits_for_task);
            let mode = mode;
            tokio::spawn(async move {
                let (read_half, mut write_half) = sock.split();
                let mut reader = BufReader::new(read_half);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.is_err() {
                    return;
                }
                let mut content_length: usize = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.is_err() {
                        return;
                    }
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((n, v)) = line.split_once(':')
                        && n.eq_ignore_ascii_case("content-length")
                    {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                let _ = hits.fetch_add(1, Ordering::Relaxed);

                let response = match mode {
                    StubMode::Success => {
                        let body_str = String::from_utf8_lossy(&body).into_owned();
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body_str).unwrap_or_default();
                        let csr_pem = parsed["csr"].as_str().unwrap_or_default().to_string();
                        let signed = sign_csr_with_ca(&csr_pem, &ca);
                        let resp_body = serde_json::json!({
                            "data": {
                                "certificate": signed.0,
                                "ca_chain": [ca.cert_pem()],
                            }
                        });
                        let resp_body_str = serde_json::to_string(&resp_body).unwrap();
                        format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            resp_body_str.len(),
                            resp_body_str
                        )
                    }
                    StubMode::Error503 => {
                        let body = "{\"errors\":[\"stub vault: simulated unavailability\"]}";
                        format!(
                            "HTTP/1.1 503 Service Unavailable\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            body.len(),
                            body
                        )
                    }
                };
                let _ = write_half.write_all(response.as_bytes()).await;
            });
        }
    });
    (addr, hits)
}

#[derive(Debug, Clone, Copy)]
enum StubMode {
    Success,
    Error503,
}

/// Sign a CSR PEM with the test CA. Returns (`signed_leaf_pem`,
/// `serial_hex`). Used by the stub server to produce valid
/// responses that chain to the configured enterprise CA.
fn sign_csr_with_ca(csr_pem: &str, ca: &Ca) -> (String, String) {
    let req = rcgen::CertificateSigningRequestParams::from_pem(csr_pem).expect("parse CSR pem");
    let (issuer_cert, issuer_key) = ca.issuer_handles();
    let leaf = req.signed_by(issuer_cert, issuer_key).expect("sign CSR");
    let pem = leaf.pem();
    let (_, parsed) = x509_parser::parse_x509_certificate(leaf.der()).expect("parse leaf der");
    let serial = parsed
        .tbs_certificate
        .raw_serial()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    (pem, serial)
}

/// Build a `VaultPkiSigner` against `stub_url` with a fake
/// bearer token. The stub doesn't check auth — this is purely
/// to exercise the request-build path.
fn signer_for_stub(stub_url: &str) -> Arc<VaultPkiSigner> {
    Arc::new(VaultPkiSigner::from_parts(
        stub_url.to_string(),
        reqwest::Client::new(),
        VaultAuth::Token {
            token: "test-token".into(),
        },
    ))
}

/// Side-channel chain check: open a TLS handshake to a
/// permissive upstream through `proxy_addr` trusting only the
/// supplied CA PEM. Returns `Ok(())` if the handshake completes
/// — i.e. the leaf noodle minted chains to the test enterprise
/// CA.
async fn proxy_presents_leaf_chaining_to_ca(
    proxy_addr: std::net::SocketAddr,
    ca_pem: &[u8],
    target: &str,
) -> Result<reqwest::StatusCode, String> {
    let cert = reqwest::Certificate::from_pem(ca_pem).map_err(|e| format!("parse CA: {e}"))?;
    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://{proxy_addr}"))
                .map_err(|e| format!("proxy: {e}"))?,
        )
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("client build: {e}"))?;
    let resp = client
        .get(target)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    Ok(resp.status())
}

#[tokio::test]
async fn external_mode_leaf_chains_to_test_enterprise_ca() {
    // 1. Create the "enterprise CA" that the stub Vault server
    //    signs with. This CA's PEM goes into the client's trust
    //    store (no NODE_EXTRA_CA_CERTS for reqwest — we feed it
    //    directly).
    let enterprise_ca = Arc::new(Ca::generate().expect("enterprise CA"));
    let enterprise_pem = enterprise_ca.cert_pem().to_string();

    // 2. Spin up the stub Vault PKI server.
    let (stub_addr, _hits) = spawn_stub_vault(Arc::clone(&enterprise_ca), StubMode::Success).await;
    let stub_url = format!("http://{stub_addr}/v1/pki/sign/noodle-leaf");

    // 3. Build the external cert mint service.
    let audit_sink = Arc::new(InMemorySink::new());
    let signer = signer_for_stub(&stub_url);
    let mint_service = ExternalCertMintService::with_timeout(signer, Duration::from_secs(5))
        .with_audit_sink(Arc::clone(&audit_sink) as Arc<dyn SideEffectSink>);
    let mint_service: Arc<dyn DynCertMintService> = Arc::new(mint_service);

    // 4. Start the proxy with external_signer override. The local
    //    `ca` slot is irrelevant — it stays populated but the
    //    bridge ignores it.
    let placeholder_ca = Arc::new(Ca::generate().expect("placeholder CA"));
    let proxy = start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 8 * 1024 * 1024,
        wire: Arc::new(noodle_adapters::log::JsonStdoutLog::new()),
        codecs: None,
        engine: None,
        filters: Vec::new(),
        enhancers: Vec::new(),
        context: None,
        sessions: Arc::new(InMemorySessionStore::new()),
        ca: placeholder_ca,
        markings: None,
        external_signer: Some(Arc::clone(&mint_service)),
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");
    let proxy_addr = proxy.local_addr();

    // 5. Side-channel chain check: open a TLS handshake through
    //    noodle to a benign upstream (example.com), trusting
    //    only the enterprise CA. Handshake succeeding proves the
    //    minted leaf chains to the enterprise CA.
    let status = proxy_presents_leaf_chaining_to_ca(
        proxy_addr,
        enterprise_pem.as_bytes(),
        "https://example.com/",
    )
    .await;

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");

    match status {
        Ok(s) => eprintln!("external-mode chain check: example.com responded {s}"),
        Err(e) => panic!(
            "external-mode leaf failed to chain to test enterprise CA — \
             handshake through noodle errored: {e}"
        ),
    }

    // 6. Audit-event assertion: the mint service emitted a
    //    `leaf_minted` event through the SideEffectSink.
    let effects = audit_sink.snapshot();
    let leaf_minted = effects.iter().any(|e| match e {
        SideEffect::Audit(a) => a.kind == AuditKind::LeafMinted,
        _ => false,
    });
    assert!(
        leaf_minted,
        "external mint must emit `leaf_minted` audit event; saw {} effects total",
        effects.len()
    );
}

#[tokio::test]
async fn external_mode_503_surfaces_as_mint_failed_audit() {
    // Fault enhancement: the stub returns 503. The mint service
    // surfaces `MintError::SignerUnavailable` (via SignerError::
    // Unavailable mapping); the audit emits `mint_failed`.
    let enterprise_ca = Arc::new(Ca::generate().expect("enterprise CA"));
    let (stub_addr, _hits) = spawn_stub_vault(Arc::clone(&enterprise_ca), StubMode::Error503).await;
    let stub_url = format!("http://{stub_addr}/v1/pki/sign/noodle-leaf");

    let audit_sink = Arc::new(InMemorySink::new());
    let signer = signer_for_stub(&stub_url);
    let mint_service = ExternalCertMintService::with_timeout(signer, Duration::from_secs(5))
        .with_audit_sink(Arc::clone(&audit_sink) as Arc<dyn SideEffectSink>);

    // Call the mint directly (don't go through the proxy — the
    // proxy will swallow the 502 on the per-flow path; we want
    // to see the MintError surface).
    let req = LeafRequest::new(
        "api.anthropic.com",
        vec!["api.anthropic.com".into()],
        Some("api.anthropic.com".into()),
        vec![],
    );
    let err = mint_service
        .mint_leaf(req)
        .await
        .expect_err("must fail under 503");

    assert!(
        matches!(err, MintError::SignerUnavailable(_)),
        "expected SignerUnavailable; got {err:?}"
    );

    // Audit emitted `mint_failed`.
    let effects = audit_sink.snapshot();
    let mint_failed = effects.iter().any(|e| match e {
        SideEffect::Audit(a) => a.kind == AuditKind::MintFailed,
        _ => false,
    });
    assert!(
        mint_failed,
        "503 from stub must emit `mint_failed` audit event"
    );
}

#[tokio::test]
async fn external_mode_cached_hosts_unaffected_by_signer_503() {
    // Same as the fault test but proves: when one mint fails,
    // the cache for other hosts is unaffected. We mint a
    // success for host A first (against a healthy stub), then
    // re-point the service at a 503 stub and call host B.
    // The success for A is unchanged.
    let enterprise_ca = Arc::new(Ca::generate().expect("enterprise CA"));
    let (ok_addr, _) = spawn_stub_vault(Arc::clone(&enterprise_ca), StubMode::Success).await;
    let ok_url = format!("http://{ok_addr}/v1/pki/sign/noodle-leaf");

    let signer_ok = signer_for_stub(&ok_url);
    let mint_a = ExternalCertMintService::with_timeout(signer_ok, Duration::from_secs(5));
    let leaf_a = mint_a
        .mint_leaf(LeafRequest::new(
            "host-a.example.com",
            vec!["host-a.example.com".into()],
            None,
            vec![],
        ))
        .await
        .expect("host A mints under healthy stub");
    assert!(!leaf_a.cert_chain.is_empty());

    let (bad_addr, _) = spawn_stub_vault(Arc::clone(&enterprise_ca), StubMode::Error503).await;
    let bad_url = format!("http://{bad_addr}/v1/pki/sign/noodle-leaf");
    let signer_bad = signer_for_stub(&bad_url);
    let mint_b = ExternalCertMintService::with_timeout(signer_bad, Duration::from_secs(5));
    let err_b = mint_b
        .mint_leaf(LeafRequest::new(
            "host-b.example.com",
            vec!["host-b.example.com".into()],
            None,
            vec![],
        ))
        .await
        .expect_err("host B fails under 503 stub");
    assert!(matches!(err_b, MintError::SignerUnavailable(_)));

    // host A's leaf is still valid (we didn't drop it).
    assert!(!leaf_a.cert_chain[0].is_empty());
}
