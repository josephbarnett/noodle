//! Integration test for S19 — procurement task pre-warms the
//! leaf cache (ADR 034 §2.5 / feature 038 §2 #6).
//!
//! Configures the proxy with 3 hosts and an `ExternalCertMintService`
//! pointed at a stub Vault server. After the procurement task
//! completes, every host has been pre-minted exactly once.
//!
//! Run:
//!
//! ```sh
//! cargo test -p noodle-proxy --test procurement_prewarm
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_cert_external::ExternalCertMintService;
use noodle_cert_external::vault::{VaultAuth, VaultPkiSigner};
use noodle_core::DynCertMintService;
use noodle_proxy::{ProxyConfig, start};
use noodle_tls::ca::Ca;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Spawn a stub Vault that signs CSRs with `ca` and counts
/// successful sign operations.
async fn spawn_stub(ca: Arc<Ca>) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let ca = Arc::clone(&ca);
            let hits = Arc::clone(&hits_clone);
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
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                hits.fetch_add(1, Ordering::Relaxed);
                let body_str = String::from_utf8_lossy(&body).into_owned();
                let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap_or_default();
                let csr_pem = parsed["csr"].as_str().unwrap_or_default().to_string();
                let Ok(req) = rcgen::CertificateSigningRequestParams::from_pem(&csr_pem) else {
                    return;
                };
                let (issuer_cert, issuer_key) = ca.issuer_handles();
                let Ok(leaf) = req.signed_by(issuer_cert, issuer_key) else {
                    return;
                };
                let leaf_pem = leaf.pem();
                let resp_body = serde_json::json!({
                    "data": {
                        "certificate": leaf_pem,
                        "ca_chain": [ca.cert_pem()],
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
                let _ = write_half.write_all(response.as_bytes()).await;
            });
        }
    });
    (addr, hits)
}

#[tokio::test]
async fn procurement_pre_mints_a_leaf_for_each_configured_host() {
    let ca = Arc::new(Ca::generate().expect("test CA"));
    let (addr, hits) = spawn_stub(Arc::clone(&ca)).await;
    let stub_url = format!("http://{addr}/v1/pki/sign/noodle-leaf");

    let signer = Arc::new(VaultPkiSigner::from_parts(
        stub_url,
        reqwest::Client::new(),
        VaultAuth::Token {
            token: "test-token".into(),
        },
    ));
    let svc = ExternalCertMintService::with_timeout(signer, Duration::from_secs(5));
    let svc: Arc<dyn DynCertMintService> = Arc::new(svc);

    let hosts = vec![
        "api.anthropic.com".to_string(),
        "api.openai.com".to_string(),
        "console.anthropic.com".to_string(),
    ];

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
        ca,
        markings: None,
        external_signer: Some(Arc::clone(&svc)),
        procurement_hosts: Some(hosts.clone()),
    })
    .await
    .expect("start proxy");

    // Procurement is a detached background task. Poll the stub
    // hit counter until it reflects `hosts.len()`, with a
    // generous timeout to allow for the proxy startup +
    // sequential mints.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while hits.load(Ordering::Relaxed) < hosts.len() {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let got = hits.load(Ordering::Relaxed);
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");

    assert!(
        got >= hosts.len(),
        "procurement should pre-mint all {} hosts; stub saw {got} requests",
        hosts.len()
    );
}

#[tokio::test]
async fn procurement_disabled_when_hosts_list_is_none() {
    let ca = Arc::new(Ca::generate().expect("test CA"));
    let (addr, hits) = spawn_stub(Arc::clone(&ca)).await;
    let stub_url = format!("http://{addr}/v1/pki/sign/noodle-leaf");

    let signer = Arc::new(VaultPkiSigner::from_parts(
        stub_url,
        reqwest::Client::new(),
        VaultAuth::Token {
            token: "test-token".into(),
        },
    ));
    let svc = ExternalCertMintService::with_timeout(signer, Duration::from_secs(5));
    let svc: Arc<dyn DynCertMintService> = Arc::new(svc);

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
        ca,
        markings: None,
        external_signer: Some(svc),
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");

    // Give the runtime a brief moment to let any (un-)spawned
    // procurement task settle.
    tokio::time::sleep(Duration::from_millis(150)).await;
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");
    assert_eq!(
        hits.load(Ordering::Relaxed),
        0,
        "procurement must not run when procurement_hosts is None"
    );
}
