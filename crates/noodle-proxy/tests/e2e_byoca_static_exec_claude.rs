//! End-to-end validation of S18 — BYOCA-static mode — through
//! the real `claude` CLI against `api.anthropic.com`.
//!
//! Mirrors the shape of `e2e_cert_mint_local_exec_claude.rs` from
//! S17. The difference: instead of `Ca::generate()`, this test
//! provisions a tempdir with `ca.pem` + `ca.key` (mode 0600) and
//! loads the CA via `Ca::load_static`. Everything downstream —
//! `LocalCertMintService`, the `cert_bridge`, the rama
//! `TlsMitmRelay` — is the same code path as S17. Success
//! demonstrates the chain `[leaf signed by enterprise CA]`
//! validates against the enterprise CA as `NODE_EXTRA_CA_CERTS`.
//!
//! Per the noodle e2e contract (`AGENTS.md` "End-to-end test
//! discipline (exec-claude)"), this is the only acceptable
//! validation for the TLS-MITM contract in BYOCA-static mode.
//! Fixture replay is not acceptable.
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
//!     --test e2e_byoca_static_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::{CERT_FILE, Ca, KEY_FILE};
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

#[cfg(unix)]
fn chmod_0600(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
}

#[cfg(not(unix))]
fn chmod_0600(_p: &Path) {}

/// Provision a tempdir with `ca.pem` + `ca.key` for an
/// independently-generated "enterprise CA", with 0600 on the key
/// so `Ca::load_static`'s permission check accepts it.
fn provision_enterprise_ca(dir: &Path) -> Ca {
    let ca = Ca::generate().expect("enterprise CA");
    fs::write(dir.join(CERT_FILE), ca.cert_pem()).expect("write ca.pem");
    fs::write(dir.join(KEY_FILE), ca.key_pem()).expect("write ca.key");
    chmod_0600(&dir.join(KEY_FILE));
    ca
}

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn byoca_static_mode_serves_real_claude_against_api_anthropic_com() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: using claude binary: {claude_bin}");

    // ── Tap sink for the proof-by-tap-records side of the
    //    acceptance.
    let work = TempDir::new().expect("workdir");
    let tap_path = work.path().join("tap.jsonl");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );

    // ── Provision an enterprise CA at a separate "operator
    //    dropped these here" directory and load via BYOCA-static.
    //    This is what `Ca::load(CaMode::ByocaStatic, dir)` does
    //    on the production startup path.
    let ca_dir = work.path().join("enterprise-ca");
    fs::create_dir_all(&ca_dir).expect("create ca dir");
    let enterprise = provision_enterprise_ca(&ca_dir);
    let ca_pem_path = work.path().join("enterprise-ca.pem");
    fs::write(&ca_pem_path, enterprise.cert_pem()).expect("write CA pem for client trust");

    let ca = Arc::new(Ca::load_static(&ca_dir).expect("load_static enterprise CA"));
    eprintln!(
        "e2e: BYOCA-static loaded — cert {} bytes, key {} bytes",
        ca.cert_pem().len(),
        ca.key_pem().len()
    );

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
    eprintln!("e2e: noodle proxy listening on {proxy_addr} (BYOCA-static mode)");

    // ── Exec claude through the proxy with the enterprise CA
    //    trusted as the only NODE_EXTRA_CA_CERTS. A successful
    //    response proves: (a) the proxy minted a leaf using the
    //    operator-supplied CA's key, (b) the leaf chains to that
    //    CA, (c) Node's TLS stack accepted the chain.

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
        "claude exited non-zero in BYOCA-static mode: status={:?}, stderr=\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    // ── Drain so tap.jsonl flushes
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // ── Read tap.jsonl + assert records present for
    //    api.anthropic.com (proves the BYOCA-loaded CA's leaf
    //    actually carried HTTPS traffic across noodle).
    let contents = fs::read_to_string(&tap_path).expect("read tap.jsonl");
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

    let anthro_records: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("api.anthropic.com"))
        })
        .collect();
    eprintln!(
        "e2e: {} records hit api.anthropic.com (proves BYOCA-static mint path worked)",
        anthro_records.len()
    );
    assert!(
        !anthro_records.is_empty(),
        "no records on api.anthropic.com — \
         the BYOCA-static MITM path didn't terminate TLS, \
         or minting against the enterprise CA failed"
    );

    eprintln!("e2e: PASS (S18 BYOCA-static end-to-end)");
}
