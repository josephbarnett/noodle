//! Integration test for S18 — BYOCA-static mode end-to-end.
//!
//! Builds the proxy with a `Ca::load_static` loaded from a tempdir
//! that we populated with a fresh "enterprise CA" + key, then opens
//! a CONNECT-tunnelled TLS handshake through the proxy and confirms
//! the leaf the proxy presents chains to the test CA.
//!
//! This is the integration test from `docs/features/037-byoca-static-mode.md`
//! §5 and the slice S18 deliverable in
//! `docs/adrs/refactor-overview.md` §9. It is the analog of the
//! S17 side-channel chain check, but uses the BYOCA-static load
//! path instead of `Ca::generate()`.
//!
//! Unlike the exec-claude e2e test
//! (`e2e_byoca_static_exec_claude.rs`), this test does NOT require
//! the `claude` CLI — it spawns its own TLS client (via `reqwest`).
//! It exercises the noodle MITM cert path against a permissive
//! upstream (`http://example.com`) so the test doesn't depend on
//! Anthropic auth or network reachability to `api.anthropic.com`.
//!
//! Run:
//!
//! ```sh
//! cargo test -p noodle-proxy --test byoca_static_chain
//! ```

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::{CertMintService, LeafRequest};
use noodle_proxy::{ProxyConfig, start};
use noodle_tls::LocalCertMintService;
use noodle_tls::ca::{CERT_FILE, CHAIN_FILE, Ca, KEY_FILE};

#[cfg(unix)]
fn chmod_0600(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
}

#[cfg(not(unix))]
fn chmod_0600(_p: &Path) {
    // No-op on Windows — see ca.rs `check_key_permissions` for
    // the platform-policy discussion.
}

/// Write a fresh enterprise CA at `dir` with `0600` permissions on
/// the key. Returns the CA so the test can extract the cert PEM
/// for the client's trust store. Mirrors what an operator's MDM
/// would do at fleet provisioning time (ADR 034 §4.2).
fn write_enterprise_ca(dir: &Path) -> Ca {
    let ca = Ca::generate().expect("enterprise CA");
    fs::write(dir.join(CERT_FILE), ca.cert_pem()).expect("write ca.pem");
    fs::write(dir.join(KEY_FILE), ca.key_pem()).expect("write ca.key");
    chmod_0600(&dir.join(KEY_FILE));
    ca
}

/// Side-channel check: open a TLS handshake to a permissive upstream
/// through `proxy_addr`, trusting only the supplied CA PEM. Returns
/// `Ok(())` if the handshake completes — i.e. the leaf noodle minted
/// chains to the test enterprise CA. The HTTP response status is
/// irrelevant; the TLS handshake is the assertion.
async fn proxy_presents_leaf_chaining_to_ca(
    proxy_addr: std::net::SocketAddr,
    ca_pem: &[u8],
    target: &str,
) -> Result<reqwest::StatusCode, String> {
    let cert = reqwest::Certificate::from_pem(ca_pem).map_err(|e| format!("parse CA pem: {e}"))?;
    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::https(format!("http://{proxy_addr}"))
                .map_err(|e| format!("proxy: {e}"))?,
        )
        .add_root_certificate(cert)
        .tls_built_in_root_certs(false)
        .danger_accept_invalid_hostnames(false)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("build reqwest client: {e}"))?;

    let resp = client
        .get(target)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    Ok(resp.status())
}

#[tokio::test]
async fn byoca_static_leaf_chains_to_operator_ca() {
    // ── Pre-place the operator's CA at a tempdir
    //    (acceptance #2: noodle loads ca.pem + ca.key from the
    //    configured directory).
    let dir = tempfile::tempdir().expect("tempdir");
    let written = write_enterprise_ca(dir.path());
    let ca_pem = written.cert_pem().to_string();

    // ── Load via BYOCA-static and start the proxy
    //    (acceptance #4: LocalCertMintService is reused with the
    //    loaded `Ca`).
    let ca = Arc::new(Ca::load_static(dir.path()).expect("load_static enterprise CA"));

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
        ca: Arc::clone(&ca),
        markings: None,
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");
    let proxy_addr = proxy.local_addr();

    // ── Side-channel chain check (acceptance #5: a leaf minted in
    //    BYOCA-static mode chains to the operator's CA).
    //
    //    `example.com` is a stable Akamai-hosted endpoint that
    //    serves HTTPS reliably and demands no auth; the TLS
    //    handshake is what proves the chain validates.
    let status =
        proxy_presents_leaf_chaining_to_ca(proxy_addr, ca_pem.as_bytes(), "https://example.com/")
            .await;

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    match status {
        Ok(s) => {
            eprintln!("byoca_static chain check: example.com responded {s}");
            // Any 2xx/3xx/4xx is fine — the TLS handshake having
            // completed is what we're asserting on. Network
            // failure (e.g. captive portal) would surface as Err.
        }
        Err(e) => panic!(
            "BYOCA-static leaf failed to chain to operator CA — \
             handshake through noodle errored: {e}"
        ),
    }
}

#[tokio::test]
async fn byoca_static_with_chain_pem_extends_leaf_chain() {
    // Variant: operator supplies an intermediate `chain.pem`.
    // Acceptance #2: "if `chain.pem` is present, it is included
    // in the leaf chain." We can't easily build a real
    // PKI hierarchy in a unit test, so we synthesize two
    // intermediates from independently-generated CAs and check
    // they're loaded into the CA's intermediate slice and reach
    // the minted leaf via `LocalCertMintService`.
    //
    // (End-to-end chain validation against the operator's
    // *root* — distinct from the signing CA — is impractical
    // without a real PKI setup; the load-and-extend behaviour
    // is what S18 ships. Real-PKI validation lives in the
    // operator's deployment runbook.)

    let dir = tempfile::tempdir().expect("tempdir");
    let _ = write_enterprise_ca(dir.path());

    let int_a = Ca::generate().expect("int a");
    let int_b = Ca::generate().expect("int b");
    fs::write(
        dir.path().join(CHAIN_FILE),
        format!("{}{}", int_a.cert_pem(), int_b.cert_pem()),
    )
    .expect("write chain.pem");

    let ca = Arc::new(Ca::load_static(dir.path()).expect("load_static + chain"));
    assert_eq!(
        ca.intermediate_chain_der().len(),
        2,
        "chain.pem with two blocks → two intermediate DERs on the Ca"
    );

    // Mint a leaf via the same service the proxy uses and assert
    // the chain contains [leaf, int_a, int_b].
    let svc = LocalCertMintService::new(Arc::clone(&ca));
    let leaf = svc
        .mint_leaf(LeafRequest::new(
            "api.anthropic.com",
            vec!["api.anthropic.com".into()],
            None,
            vec![],
        ))
        .await
        .expect("mint leaf");
    assert_eq!(
        leaf.cert_chain.len(),
        3,
        "BYOCA-static leaf must include operator-supplied intermediates"
    );
}

#[tokio::test]
async fn byoca_static_fails_loud_when_files_missing() {
    // Acceptance #3: the proxy fails to start with a clear error
    // pointing at the configured path. It does NOT silently fall
    // back to generating a local CA.
    let dir = tempfile::tempdir().expect("tempdir");
    // Note: dir is empty — no ca.pem, no ca.key.
    let result = Ca::load_static(dir.path());
    let Err(err) = result else {
        panic!("expected load to fail on empty dir");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("ca.pem") || msg.contains("ca.key"),
        "error message must name the missing file; got: {msg}"
    );
    // And nothing was written behind our back.
    assert!(!dir.path().join("ca.pem").exists());
    assert!(!dir.path().join("ca.key").exists());
}
