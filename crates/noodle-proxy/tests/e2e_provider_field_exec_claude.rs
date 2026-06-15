//! End-to-end validation of the `provider` field on `tap.jsonl`
//! records (S4 of the 027–031 refactor; ADR 025 §3.7).
//!
//! Per ADR 029 the canonical `noodle-domain::envelope_metadata::ProviderId`
//! enum is the typed vocabulary downstream consumers parse — but on
//! the wire (`tap.jsonl`) the field is the snake-cased string form
//! (`"anthropic"`, `"openai"`, `"google"`, …) that matches
//! `ProviderId`'s serde representation.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against real Anthropic, reads the real `tap.jsonl` noodle
//! wrote, and asserts every `api.anthropic.com` record carries
//! `provider: "anthropic"`. Per the noodle e2e contract, fixture-
//! replay is not acceptable — only exec-claude through the real
//! proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_provider_field_exec_claude \
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
async fn provider_field_is_anthropic_on_real_tap_jsonl() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: claude binary: {claude_bin}");

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

    let prompt = format!(
        "Run `ls {tmp}` and tell me how many files are in the directory. \
         Reply with just the number.",
        tmp = tap_dir.path().display()
    );

    let output = Command::new(&claude_bin)
        .arg("-p")
        .arg(&prompt)
        .env("HTTPS_PROXY", format!("http://{proxy_addr}"))
        .env("NODE_EXTRA_CA_CERTS", &ca_pem_path)
        .env("https_proxy", format!("http://{proxy_addr}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn claude");

    eprintln!(
        "e2e: claude stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "claude exited non-zero: {:?}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    let records: Vec<Value> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap.jsonl line"))
        .collect();
    assert!(!records.is_empty(), "tap.jsonl had no records");
    eprintln!("e2e: {} total tap records", records.len());

    // ─── Provider field present on every record ─────────────────
    //
    // ADR 025 §3.7: every cell-claimed flow carries a provider
    // identifier. The proxy stamps it; the tap writer surfaces it.
    // A record without a provider field is a missed stamping —
    // i.e. the proxy hot path failed to identify the cell.

    let missing_provider: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("provider")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        })
        .collect();
    assert!(
        missing_provider.is_empty(),
        "expected every tap record to carry a provider, but {} did not: {:?}",
        missing_provider.len(),
        missing_provider
            .iter()
            .map(|r| r.get("url"))
            .collect::<Vec<_>>()
    );

    // ─── Every api.anthropic.com record reads `anthropic` ───────

    let anthropic_records: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("api.anthropic.com"))
        })
        .collect();
    eprintln!(
        "e2e: {} records hit api.anthropic.com",
        anthropic_records.len()
    );
    assert!(
        !anthropic_records.is_empty(),
        "no api.anthropic.com records — did claude actually call Anthropic?"
    );

    for rec in &anthropic_records {
        let p = rec
            .get("provider")
            .and_then(Value::as_str)
            .expect("anthropic-hit record missing provider field");
        assert_eq!(
            p,
            "anthropic",
            "api.anthropic.com record carried provider={p:?}, expected \"anthropic\". \
             URL: {:?}",
            rec.get("url")
        );
    }
    eprintln!(
        "e2e: all {} anthropic records carry provider=\"anthropic\"",
        anthropic_records.len()
    );

    // ─── Provider matches the noodle-domain enum's wire shape ───
    //
    // Parse one record's provider field through
    // `noodle_domain::envelope_metadata::ProviderId` to assert
    // wire compatibility — the on-disk value MUST round-trip
    // into the typed enum without an `Other(...)` fallback.

    let first_provider = anthropic_records[0]
        .get("provider")
        .and_then(Value::as_str)
        .unwrap();
    let typed: noodle_domain::envelope_metadata::ProviderId =
        serde_json::from_str(&format!("{first_provider:?}"))
            .expect("provider string parses as noodle_domain::ProviderId");
    assert!(
        matches!(
            typed,
            noodle_domain::envelope_metadata::ProviderId::Anthropic
        ),
        "expected ProviderId::Anthropic, got {typed:?}"
    );

    eprintln!("e2e: PASS — provider field is wire-compatible with noodle-domain");
}
