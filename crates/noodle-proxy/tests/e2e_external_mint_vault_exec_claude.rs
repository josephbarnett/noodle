//! End-to-end validation of S19 — `ExternalCertMintService` +
//! `VaultPkiSigner` — through the real `claude` CLI.
//!
//! Spins up a stub Vault PKI server that signs CSRs with a
//! test enterprise CA, configures noodle in external-signer
//! mode pointing at the stub URL, then runs claude with
//! `NODE_EXTRA_CA_CERTS` pointing at the test enterprise CA's
//! PEM. Claude completes a short request through noodle and
//! the `tap.jsonl` file records the round trip on
//! `api.anthropic.com` (proving the mint path worked end-to-
//! end via the stub).
//!
//! `#[ignore]`d by default; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy \
//!     --test e2e_external_mint_vault_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_cert_external::ExternalCertMintService;
use noodle_cert_external::vault::{VaultAuth, VaultPkiSigner};
use noodle_core::DynCertMintService;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;

fn claude_binary() -> Option<String> {
    let out = std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Spawn a stub Vault PKI server that signs every CSR with
/// `ca` and returns the standard `{ data: { certificate, ca_chain } }`
/// shape.
async fn spawn_stub_vault(ca: Arc<Ca>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let ca = Arc::clone(&ca);
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
                let resp_body = serde_json::json!({
                    "data": {
                        "certificate": leaf.pem(),
                        "ca_chain": [ca.cert_pem()],
                    }
                });
                let body_str = serde_json::to_string(&resp_body).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    body_str.len(),
                    body_str
                );
                let _ = write_half.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn external_cert_mint_service_serves_real_claude_via_stub_vault() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: using claude binary: {claude_bin}");

    // ── Enterprise CA (signs leaves) + stub Vault server.
    let enterprise_ca = Arc::new(Ca::generate().expect("enterprise CA"));
    let stub_addr = spawn_stub_vault(Arc::clone(&enterprise_ca)).await;
    let stub_url = format!("http://{stub_addr}/v1/pki/sign/noodle-leaf");
    eprintln!("e2e: stub vault listening on {stub_addr}");

    // ── Tap sink + working dir.
    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    let ca_pem_path = tap_dir.path().join("enterprise-ca.pem");
    std::fs::write(&ca_pem_path, enterprise_ca.cert_pem()).expect("write CA pem");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );

    // ── Build the ExternalCertMintService.
    let signer = Arc::new(VaultPkiSigner::from_parts(
        stub_url.clone(),
        reqwest::Client::new(),
        VaultAuth::Token {
            token: "test-token".into(),
        },
    ));
    let mint_service = ExternalCertMintService::with_timeout(signer, Duration::from_secs(5));
    let mint_service: Arc<dyn DynCertMintService> = Arc::new(mint_service);

    // Placeholder CA — the bridge ignores it in external mode,
    // but the field is still required on ProxyConfig.
    let placeholder_ca = Arc::new(Ca::generate().expect("placeholder CA"));

    let proxy = start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 8 * 1024 * 1024,
        wire: tap_sink.clone(),
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
    eprintln!("e2e: noodle proxy listening on {proxy_addr}");

    // ── Run claude through noodle with NODE_EXTRA_CA_CERTS
    //    pointing at the enterprise CA. The minted leaves chain
    //    to the enterprise CA, so Node accepts them.
    let prompt = "Reply with the single digit 7 and nothing else.";
    let result = Command::new(&claude_bin)
        .arg("-p")
        .arg(prompt)
        .env("HTTPS_PROXY", format!("http://{proxy_addr}"))
        .env("NODE_EXTRA_CA_CERTS", &ca_pem_path)
        .env("https_proxy", format!("http://{proxy_addr}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => panic!("spawn claude failed: {e}"),
    };
    eprintln!(
        "e2e: claude stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    eprintln!(
        "e2e: claude stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "claude exited non-zero: status={:?}, stderr=\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // ── Drain.
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // ── Assert: tap.jsonl shows records for api.anthropic.com.
    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    assert!(
        !contents.is_empty(),
        "tap.jsonl is empty — proxy didn't capture"
    );

    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse line"))
        .collect();
    eprintln!("e2e: parsed {} records", records.len());
    let anthro_records: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("api.anthropic.com"))
        })
        .collect();
    eprintln!("e2e: {} records on api.anthropic.com", anthro_records.len());
    assert!(
        !anthro_records.is_empty(),
        "no records on api.anthropic.com — \
         external-mint MITM path failed end-to-end via the stub"
    );
    eprintln!("e2e: PASS");
}
