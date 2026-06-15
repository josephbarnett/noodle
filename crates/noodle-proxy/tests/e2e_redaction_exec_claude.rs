//! End-to-end validation of the prefix-preserving header redaction
//! policy (S5 of the 027–031 refactor; ADR 027 §9).
//!
//! Per the ADR the on-disk `tap.jsonl` redacts sensitive header
//! values to `<first N chars>...<redacted>` with per-header N. The
//! preserved prefix is the vendor-tagged credential identifier
//! downstream consumers join against billing / inventory; the
//! marker `...<redacted>` is the literal sentinel.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API. claude sends an
//! `Authorization: Bearer sk-ant-…` (or similar) header on every
//! `/v1/messages` request. The test reads the real `tap.jsonl`
//! and asserts:
//!
//! 1. Every `Authorization` value on the wire matches the
//!    redacted shape — `Bearer <12 chars>...<redacted>` or
//!    `Bearer <redacted>` for short tokens.
//! 2. NO `Authorization` value carries more than 12 visible
//!    credential characters. This is the security property the
//!    ADR pins — a regression here is a real-world credential
//!    leak.
//! 3. `Cookie` and `Set-Cookie` (if present) are fully redacted
//!    (`<redacted>`), N=0 per the ADR table.
//! 4. The redaction marker is the literal `...<redacted>` string,
//!    not the legacy `...REDACTED` shape — pins the wire format.
//!
//! Per the noodle e2e contract, fixture-replay is not acceptable;
//! only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_redaction_exec_claude \
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

/// The redaction marker every prefix-preserving sensitive header
/// value ends with per ADR 027 §9.
const REDACTION_MARKER: &str = "...<redacted>";

/// Compute the number of bytes of preserved credential characters
/// in a redacted header value. The visible portion is what sits
/// between an optional auth-scheme prefix (`Bearer `, `Basic `, …)
/// and the redaction marker.
fn visible_credential_chars(value: &str) -> usize {
    let Some(idx) = value.find(REDACTION_MARKER) else {
        return 0;
    };
    let head = &value[..idx];
    for scheme in ["Bearer ", "Basic ", "Token ", "Digest "] {
        if let Some(stripped) = head.strip_prefix(scheme) {
            return stripped.len();
        }
        if head
            .get(..scheme.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
        {
            return head.len() - scheme.len();
        }
    }
    head.len()
}

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
async fn redaction_policy_holds_on_real_tap_jsonl() {
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

    // ─── Collect every sensitive header value seen on the wire ──

    let mut auth_values = Vec::new();
    let mut x_api_values = Vec::new();
    let mut anthropic_api_values = Vec::new();
    let mut cookie_values = Vec::new();
    let mut set_cookie_values = Vec::new();
    let mut proxy_auth_values = Vec::new();

    for rec in &records {
        let Some(headers) = rec.get("headers").and_then(Value::as_object) else {
            continue;
        };
        for (name, values) in headers {
            let lower = name.to_ascii_lowercase();
            let strs: Vec<&str> = values
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            match lower.as_str() {
                "authorization" => auth_values.extend(strs.iter().map(|s| (*s).to_owned())),
                "x-api-key" => x_api_values.extend(strs.iter().map(|s| (*s).to_owned())),
                "anthropic-api-key" => {
                    anthropic_api_values.extend(strs.iter().map(|s| (*s).to_owned()));
                }
                "cookie" => cookie_values.extend(strs.iter().map(|s| (*s).to_owned())),
                "set-cookie" => set_cookie_values.extend(strs.iter().map(|s| (*s).to_owned())),
                "proxy-authorization" => {
                    proxy_auth_values.extend(strs.iter().map(|s| (*s).to_owned()));
                }
                _ => {}
            }
        }
    }
    eprintln!(
        "e2e: collected — auth={}, x-api={}, anthropic-api={}, cookie={}, set-cookie={}, proxy-auth={}",
        auth_values.len(),
        x_api_values.len(),
        anthropic_api_values.len(),
        cookie_values.len(),
        set_cookie_values.len(),
        proxy_auth_values.len(),
    );

    // claude code authenticates via Authorization on the wire.
    // If we don't see any auth header, the test is meaningless
    // (claude went around the proxy somehow).
    assert!(
        !auth_values.is_empty(),
        "no Authorization headers in tap.jsonl — claude didn't authenticate \
         through the proxy"
    );

    // ─── Assertion 1: every Authorization is in the redacted shape ─

    for v in &auth_values {
        let acceptable = v == "Bearer <redacted>"
            || v == "<redacted>"
            || (v.starts_with("Bearer ") && v.ends_with(REDACTION_MARKER))
            || v.ends_with(REDACTION_MARKER);
        assert!(
            acceptable,
            "Authorization value not in redacted shape: {v:?}"
        );
    }

    // ─── Assertion 2: visible credential portion ≤ 12 chars ────
    //
    // This is the load-bearing security property. The visible
    // portion is what sits between the optional auth-scheme prefix
    // (`Bearer `, `Basic `, …) and the redaction marker — counted
    // by `visible_credential_chars` at module scope. Assert byte
    // length ≤ 12 for every observed value.

    for v in &auth_values {
        let visible = visible_credential_chars(v);
        assert!(
            visible <= 12,
            "Authorization exposes {visible} credential chars (>12): {v:?}"
        );
    }
    for v in &x_api_values {
        let visible = visible_credential_chars(v);
        assert!(
            visible <= 12,
            "X-Api-Key exposes {visible} credential chars (>12): {v:?}"
        );
    }
    for v in &anthropic_api_values {
        let visible = visible_credential_chars(v);
        assert!(
            visible <= 12,
            "Anthropic-Api-Key exposes {visible} credential chars (>12): {v:?}"
        );
    }

    // ─── Assertion 3: Cookie + Set-Cookie + Proxy-Authorization fully opaque ─

    for v in &cookie_values {
        assert_eq!(v, "<redacted>", "Cookie not fully redacted: {v:?}");
    }
    for v in &set_cookie_values {
        assert_eq!(v, "<redacted>", "Set-Cookie not fully redacted: {v:?}");
    }
    for v in &proxy_auth_values {
        assert_eq!(
            v, "<redacted>",
            "Proxy-Authorization not fully redacted: {v:?}"
        );
    }

    // ─── Assertion 4: wire format uses the new marker ───────────

    let has_marker = auth_values.iter().any(|v| v.contains(REDACTION_MARKER));
    let has_legacy = auth_values.iter().any(|v| v.contains("...REDACTED"));
    assert!(
        has_marker,
        "no Authorization value uses the new marker '...<redacted>' — \
         tap.jsonl format regressed?"
    );
    assert!(
        !has_legacy,
        "Authorization still uses the legacy marker '...REDACTED' — \
         the redaction policy didn't actually update"
    );

    // ─── Assertion 5: at least one value has a visible vendor prefix ─
    //
    // The whole point of prefix preservation is operator
    // reconciliation. If every value reduces to `Bearer <redacted>`
    // (length cutoff hit on every observed token), the policy is
    // technically correct but the wire isn't carrying the
    // reconciliation value the ADR specifies. Anthropic keys
    // (`sk-ant-…`) are always >> 12 chars so this assertion
    // should always pass against a real claude session.

    let has_visible_prefix = auth_values.iter().any(|v| {
        v.starts_with("Bearer sk")
            && v.ends_with(REDACTION_MARKER)
            && visible_credential_chars(v) == 12
    });
    assert!(
        has_visible_prefix,
        "no Authorization value showed a `Bearer sk-…(12 chars)...<redacted>` \
         shape — prefix preservation isn't reaching tap.jsonl. \
         Observed values: {auth_values:?}"
    );

    eprintln!("e2e: PASS — redaction policy ADR 027 §9 verified end-to-end");
}
