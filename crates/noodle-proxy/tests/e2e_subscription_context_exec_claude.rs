//! End-to-end validation of the envelope-level **subscription
//! context** block (S7 of the 027–031 refactor; ADR 029 §2.4
//! family 13).
//!
//! Per the ADR every `tap.jsonl` record that carries
//! attribution-relevant facts has an `envelope.subscription`
//! object with three sub-fields:
//!
//! - `envelope.subscription.api_key` — typed
//!   [`ApiKeyFingerprint`] derived from the credential header
//!   the proxy observed at request open. Carries `prefix` (12
//!   chars from the credential), `kind` (`api_key` / `session`
//!   / `oauth` / `unknown`), and `source` (`authorization_header`
//!   / `x_api_key` / etc.).
//! - `envelope.subscription.organization` — typed
//!   [`OrganizationContext`] populated from either the
//!   `claude.ai` URL path (`/api/organizations/{org}/...`) at
//!   request open OR the `Anthropic-Organization-Id` response
//!   header at response close. v1 leaves
//!   `parent_organization_id` / `account_type` un-enriched.
//! - `envelope.subscription.tier` — left empty for v1 (Console
//!   API enrichment is the embellishment plane's job).
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API. The proxy stamps the
//! subscription block at request open; the sink writes it onto
//! every JSONL record. The test reads the real `tap.jsonl` and
//! asserts:
//!
//! 1. At least one record carries
//!    `envelope.subscription.api_key.prefix` populated.
//! 2. That prefix is exactly 12 characters.
//! 3. `envelope.subscription.api_key.kind` is one of
//!    `api_key` / `session` / `oauth` (NOT `unknown`).
//! 4. **Bonus:** if any response carried
//!    `Anthropic-Organization-Id`, the matching record's
//!    `envelope.subscription.organization.organization_id` is
//!    populated.
//!
//! Per the noodle e2e contract, fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_subscription_context_exec_claude \
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
async fn subscription_context_appears_on_real_tap_jsonl() {
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
    eprintln!("e2e: {} total tap records", records.len());

    assert!(
        !records.is_empty(),
        "no tap.jsonl records — claude didn't traverse the proxy"
    );

    // ─── Assertion 1: at least one record has api_key populated ───

    let records_with_api_key: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("envelope")
                .and_then(|e| e.get("subscription"))
                .and_then(|s| s.get("api_key"))
                .and_then(|a| a.get("prefix"))
                .and_then(Value::as_str)
                .is_some()
        })
        .collect();
    assert!(
        !records_with_api_key.is_empty(),
        "no records carried envelope.subscription.api_key.prefix — \
         the proxy didn't observe a credential on any request, or \
         the subscription stamping isn't reaching the wire log"
    );
    eprintln!(
        "e2e: {} of {} records carry envelope.subscription.api_key.prefix",
        records_with_api_key.len(),
        records.len()
    );

    // ─── Assertion 2: prefix is exactly 12 characters ─────────────

    for (i, rec) in records_with_api_key.iter().enumerate() {
        let prefix = rec
            .get("envelope")
            .and_then(|e| e.get("subscription"))
            .and_then(|s| s.get("api_key"))
            .and_then(|a| a.get("prefix"))
            .and_then(Value::as_str)
            .expect("api_key.prefix is a string");
        assert_eq!(
            prefix.len(),
            12,
            "record {i}: api_key.prefix = {prefix:?} ({} bytes), want 12",
            prefix.len()
        );
    }

    // Log a redacted view of the first observed prefix so the
    // test output is auditable without leaking the full credential.
    let first_prefix = records_with_api_key[0]
        .get("envelope")
        .and_then(|e| e.get("subscription"))
        .and_then(|s| s.get("api_key"))
        .and_then(|a| a.get("prefix"))
        .and_then(Value::as_str)
        .unwrap();
    // Keep first 4 chars of the prefix in the test log so a
    // human can recognise the vendor family (`sk-a…`) without
    // surfacing all 12 chars in CI output.
    let truncated = &first_prefix[..first_prefix.len().min(4)];
    eprintln!("e2e: first observed api_key.prefix (12 chars): {truncated}…");

    // ─── Assertion 3: kind is one of api_key / session / oauth ─

    let allowed_kinds = ["api_key", "session", "oauth"];
    for (i, rec) in records_with_api_key.iter().enumerate() {
        let kind = rec
            .get("envelope")
            .and_then(|e| e.get("subscription"))
            .and_then(|s| s.get("api_key"))
            .and_then(|a| a.get("kind"))
            .and_then(Value::as_str)
            .expect("api_key.kind is a string");
        assert!(
            allowed_kinds.contains(&kind),
            "record {i}: api_key.kind = {kind:?}, want one of {allowed_kinds:?}",
        );
    }
    let kinds_observed: std::collections::BTreeSet<&str> = records_with_api_key
        .iter()
        .filter_map(|r| {
            r.get("envelope")
                .and_then(|e| e.get("subscription"))
                .and_then(|s| s.get("api_key"))
                .and_then(|a| a.get("kind"))
                .and_then(Value::as_str)
        })
        .collect();
    eprintln!("e2e: api_key.kind values observed: {kinds_observed:?}");

    // Source should be one of the documented values too — most
    // records will be `authorization_header` (Bearer) but
    // `x_api_key` is also valid for claude code.
    let allowed_sources = ["authorization_header", "x_api_key"];
    for rec in &records_with_api_key {
        let source = rec
            .get("envelope")
            .and_then(|e| e.get("subscription"))
            .and_then(|s| s.get("api_key"))
            .and_then(|a| a.get("source"))
            .and_then(Value::as_str)
            .expect("api_key.source is a string");
        assert!(
            allowed_sources.contains(&source),
            "api_key.source = {source:?}, want one of {allowed_sources:?}",
        );
    }

    // ─── Assertion 4 (bonus): if any response carried the org
    //     header, the matching record's organization_id is set ──

    let mut org_ids_in_response_headers: Vec<String> = Vec::new();
    for rec in &records {
        if rec.get("direction").and_then(Value::as_str) != Some("response") {
            continue;
        }
        if let Some(headers) = rec.get("headers").and_then(Value::as_object) {
            for (name, values) in headers {
                if !name.eq_ignore_ascii_case("anthropic-organization-id") {
                    continue;
                }
                if let Some(arr) = values.as_array() {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            org_ids_in_response_headers.push(s.to_owned());
                        }
                    }
                }
            }
        }
    }

    if org_ids_in_response_headers.is_empty() {
        eprintln!(
            "e2e: no Anthropic-Organization-Id response header observed — \
             organization assertion deferred (this is not a failure, \
             not every cell ships the header)"
        );
    } else {
        eprintln!(
            "e2e: {} response(s) carried Anthropic-Organization-Id; \
             asserting the subscription block reflects it",
            org_ids_in_response_headers.len()
        );
        // At least one record should have organization_id populated
        // on its subscription block (since the proxy folds the
        // response header into the envelope before emitting the
        // response wire event).
        let with_org_id = records
            .iter()
            .filter(|r| {
                r.get("envelope")
                    .and_then(|e| e.get("subscription"))
                    .and_then(|s| s.get("organization"))
                    .and_then(|o| o.get("organization_id"))
                    .and_then(Value::as_str)
                    .is_some()
            })
            .count();
        assert!(
            with_org_id > 0,
            "Anthropic-Organization-Id was observed on responses but \
             envelope.subscription.organization.organization_id was \
             never populated — the response-header merge isn't reaching \
             the wire event",
        );
        eprintln!("e2e: {with_org_id} records carry organization.organization_id");
    }

    eprintln!("e2e: PASS — subscription context ADR 029 §2.4 family 13 verified end-to-end");
}
