//! End-to-end validation of the `LocalCertMintService` path
//! (refactor S17 / ADR 034 §2.2) by running the real `claude`
//! CLI through a real noodle proxy and reading the real
//! `tap.jsonl` it produced.
//!
//! ## Why this shape
//!
//! S17's load-bearing safety property is "byte-identical client
//! behaviour vs the pre-refactor `InMemoryBoringMitmCertIssuer`
//! path". The only meaningful way to validate that is to run
//! real claude through the real proxy and observe that:
//!
//! 1. The TLS handshake succeeds (so the new
//!    `NoodleCertMintIssuer` bridge produces a leaf that
//!    `BoringSSL` accepts on the client side).
//! 2. claude reaches `api.anthropic.com` and gets responses
//!    (so request bodies cross the MITM and responses come
//!    back).
//! 3. The on-disk `tap.jsonl` shows records hitting
//!    `api.anthropic.com` (so the proxy minted leaves for that
//!    host — a cache miss → mint path was exercised).
//!
//! Per the noodle e2e contract
//! (`memory:feedback_no_fixture_extraction`), extracting bytes
//! from `captures/*.mitm` into Rust fixtures is NOT acceptable.
//! Only exec-claude through real noodle counts for TLS-MITM
//! contracts.
//!
//! ## Bonus: side-channel leaf-chain verification
//!
//! Once we know the proxy minted a leaf, we open a fresh TLS
//! handshake through it ourselves (a short-lived reqwest client
//! that trusts only our test CA) and assert the leaf certificate
//! it presents chains to the test CA. This confirms the bridge
//! produces a leaf with the right SAN + issuer for the target
//! host.
//!
//! ## Requirements to run
//!
//! - `claude` CLI installed and on `PATH`.
//! - `claude` already authenticated.
//! - Network access to `api.anthropic.com`.
//!
//! ## CI gating
//!
//! `#[ignore]`d by default. Run locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy \
//!     --test e2e_cert_mint_local_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use serde_json::Value;
use tempfile::TempDir;
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

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn local_cert_mint_service_serves_real_claude_against_api_anthropic_com() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: using claude binary: {claude_bin}");

    // ── Spin up a real noodle proxy with the LocalCertMintService
    //    path (the only path wired by start() today; this is the
    //    code under test).

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );

    let ca = Arc::new(Ca::generate().expect("generate test CA"));
    let ca_pem_path = tap_dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("write CA pem");

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
        ca: Arc::clone(&ca),
        markings: None,
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");

    let proxy_addr = proxy.local_addr();
    eprintln!("e2e: noodle proxy listening on {proxy_addr}");

    // ── Exec claude with HTTPS_PROXY pointed at noodle. The
    //    NODE_EXTRA_CA_CERTS env var teaches Node (claude is a
    //    Node process) to trust the test CA, so leaves minted by
    //    `LocalCertMintService` validate on the client side.
    //
    // A short prompt is sufficient — we are validating cert
    // minting, not tool use. Any successful response proves the
    // mint path worked.

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

    // ── Side-channel leaf verification ───────────────────────
    //
    // While the proxy is still running, open our own
    // CONNECT-tunnelled TLS handshake to `api.anthropic.com`
    // through noodle and observe the leaf chain noodle presents.
    // This is independent of claude — it directly exercises the
    // `LocalCertMintService` → bridge → cache → `TlsMitmRelay`
    // path again and confirms the leaf chains to our test CA
    // (proving the mint happened with the right issuer).
    //
    // We use `reqwest` with the test CA bundled as the only
    // trusted root; if the handshake completes the chain by
    // definition validates against our CA.
    let side_channel_status = side_channel_chain_check(proxy_addr, &ca_pem_path).await;
    eprintln!("e2e: side-channel chain check result: {side_channel_status:?}");
    assert!(
        side_channel_status.is_ok(),
        "side-channel handshake through noodle failed: {side_channel_status:?}",
    );

    // ── Drain the sink so tap.jsonl flushes ─────────────────

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // ── Read tap.jsonl + assert ─────────────────────────────

    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    assert!(
        !contents.is_empty(),
        "tap.jsonl is empty — proxy didn't capture"
    );

    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap.jsonl line"))
        .collect();
    eprintln!("e2e: parsed {} tap records", records.len());
    assert!(!records.is_empty(), "no records parsed from tap.jsonl");

    // Filter to records that hit api.anthropic.com. Presence of
    // any such record validates that the MITM cert-minting path
    // worked end-to-end: the TLS handshake completed and the
    // request body crossed the proxy.
    let anthro_records: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("api.anthropic.com"))
        })
        .collect();
    eprintln!(
        "e2e: {} records hit api.anthropic.com (proves mint path worked)",
        anthro_records.len()
    );
    assert!(
        !anthro_records.is_empty(),
        "no records on api.anthropic.com — \
         the MITM path didn't terminate TLS, or minting failed"
    );

    eprintln!("e2e: PASS");
}

/// Open a CONNECT-tunnelled TLS handshake to `api.anthropic.com`
/// through `proxy_addr`, trusting only the CA at `ca_pem_path`.
/// Returns `Ok(())` if the handshake completes (i.e. the leaf
/// noodle minted chains to our test CA).
///
/// This is the bonus chain-verification helper from the S17
/// brief. A successful TLS round trip is a strong assertion:
/// reqwest's HTTPS client will only complete if the presented
/// chain validates against the supplied root, so success
/// implies the leaf was issued by our CA.
async fn side_channel_chain_check(
    proxy_addr: std::net::SocketAddr,
    ca_pem_path: &std::path::Path,
) -> Result<(), String> {
    let ca_pem = std::fs::read(ca_pem_path).map_err(|e| format!("read CA pem: {e}"))?;
    let cert = reqwest::Certificate::from_pem(&ca_pem).map_err(|e| format!("parse CA pem: {e}"))?;
    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://{proxy_addr}"))
                .map_err(|e| format!("proxy: {e}"))?,
        )
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build reqwest client: {e}"))?;

    // A GET to the root path is fine — Anthropic returns some
    // status (likely 404 or 405), but the TLS handshake completes
    // before any HTTP semantics. That's all we need.
    let resp = client
        .get("https://api.anthropic.com/")
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    eprintln!(
        "e2e: side-channel: api.anthropic.com responded {} via noodle",
        resp.status()
    );
    Ok(())
}
