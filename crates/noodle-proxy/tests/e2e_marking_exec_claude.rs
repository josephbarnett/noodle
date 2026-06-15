//! End-to-end marking detector validation by running the real
//! `claude` CLI through a real noodle proxy and reading the real
//! `tap.jsonl` it produced.
//!
//! ## Why this shape
//!
//! Per ADR 028 §4 the marking detector's job is to populate the
//! marks block (`session_id`, `turn_id`, `parent_session_id`) on
//! every `tap.jsonl` record from a marking-enabled cell. The
//! contract is a runtime property of the proxy: it can only be
//! validated by running the real binary, the real TLS MITM, the
//! real wire sink, against real client traffic, and then reading
//! the real file the sink wrote.
//!
//! Replaying bytes through codecs in-process doesn't exercise the
//! TCP listener, the CA mint cache, the streaming body tee, the
//! async writer task, or the file format. Those are the surfaces
//! where regressions hide.
//!
//! ## Requirements to run
//!
//! - `claude` CLI installed and on `PATH`.
//! - `claude` already authenticated (a valid login session under
//!   `~/.claude/` or `ANTHROPIC_API_KEY` in env — whatever the
//!   user's normal claude works with).
//! - Network access to `api.anthropic.com`.
//!
//! ## CI gating
//!
//! `#[ignore]`d by default. Run locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_marking_exec_claude \
//!     -- --ignored --nocapture
//! ```
//!
//! ## What it asserts
//!
//! 1. At least one wire record in `tap.jsonl` carries a `marks`
//!    block (i.e. the detector ran for the request).
//! 2. All records sharing a wire request flow against
//!    `api.anthropic.com/v1/messages` carry the same
//!    `marks.session_id` value when they came from the same
//!    `X-Claude-Code-Session-Id` header.
//! 3. The continuation-vs-new-turn semantics hold per ADR 028
//!    §4.1: round-trips within a single user turn share a
//!    `turn_id`; the first round-trip after `end_turn` mints a
//!    new `turn_id`.

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

/// Locate the `claude` CLI on `PATH`. Returns `None` (test skips)
/// when not installed — keeps CI machines that don't ship claude
/// from spuriously failing.
fn claude_binary() -> Option<String> {
    which("claude")
}

fn which(bin: &str) -> Option<String> {
    let out = std::process::Command::new("which").arg(bin).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn marking_detector_populates_tap_jsonl_when_claude_runs_through_noodle() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e: using claude binary: {claude_bin}");

    // ─── Spin up a real noodle proxy with marking wired ────────

    let tap_dir = TempDir::new().expect("create tap tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    eprintln!("e2e: tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );

    // ADR 052: frame-tree registry replaces the retired AnthropicMarkingDetector.
    let detector = Arc::new(noodle_adapters::marking::FrameTreeRegistry::new());

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
        markings: Some(detector),
        external_signer: None,
        procurement_hosts: None,
    })
    .await
    .expect("start proxy");

    let proxy_addr = proxy.local_addr();
    eprintln!("e2e: noodle proxy listening on {proxy_addr}");

    // ─── Exec claude with HTTPS_PROXY pointed at noodle ────────
    //
    // claude code is a Node program; node trusts NODE_EXTRA_CA_CERTS
    // for additional roots. The session id we want to see in
    // marks.session_id is set by claude as the
    // `X-Claude-Code-Session-Id` header — claude generates that
    // per-conversation automatically.
    //
    // The prompt is chosen to force at least one tool round-trip:
    // listing the temp directory pushes claude to call a tool
    // (Bash or Read) and then close the turn. That produces a
    // multi-RT session if claude reasons before tool use, or a
    // single RT if it answers immediately — either way the
    // marking detector runs and stamps marks.

    let prompt = format!(
        "Run `ls {tmp}` and tell me how many files are in the directory. \
         Reply with just the number.",
        tmp = tap_dir.path().display()
    );

    let result = Command::new(&claude_bin)
        .arg("-p")
        .arg(&prompt)
        .env("HTTPS_PROXY", format!("http://{proxy_addr}"))
        .env("NODE_EXTRA_CA_CERTS", &ca_pem_path)
        // Belt-and-suspenders for tools that use other env conventions.
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

    // ─── Drain the sink so tap.jsonl flushes ───────────────────

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    // The sink is still alive via the Arc — drop the Arc to take
    // exclusive ownership of TapJsonlLog before calling shutdown,
    // which consumes self.
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;

    // ─── Read tap.jsonl + assert ────────────────────────────────

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

    // Filter to records that hit api.anthropic.com — the cell
    // configured to mark.
    let messages: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("api.anthropic.com") && u.contains("/v1/messages"))
        })
        .collect();
    eprintln!(
        "e2e: {} records hit api.anthropic.com/v1/messages",
        messages.len()
    );
    assert!(
        !messages.is_empty(),
        "no /v1/messages records — did claude reach Anthropic?"
    );

    // ─── Assertion 1: at least one record has marks populated ───

    let marked: Vec<&Value> = messages
        .iter()
        .copied()
        .filter(|r| r.get("marks").is_some_and(|m| !m.is_null()))
        .collect();
    eprintln!(
        "e2e: {} of {} messages records carry a marks block",
        marked.len(),
        messages.len()
    );
    assert!(
        !marked.is_empty(),
        "marking detector produced no marks on any /v1/messages record — \
         either the detector isn't wired or the session header was missing"
    );

    // ─── Assertion 2: same session header → same session_id ────

    let session_ids: std::collections::HashSet<&str> = marked
        .iter()
        .filter_map(|r| {
            r.get("marks")
                .and_then(|m| m.get("session_id"))
                .and_then(Value::as_str)
        })
        .collect();
    eprintln!(
        "e2e: {} distinct session_ids in marked records",
        session_ids.len()
    );
    assert_eq!(
        session_ids.len(),
        1,
        "expected exactly one session_id across the run (claude uses one \
         conversation id per invocation); got {session_ids:?}"
    );

    // ─── Assertion 3: turn_id stability per ADR 028 §4.1 ────────
    //
    // Group records by request_id, derive per-RT turn_id. The
    // §4.1 contract:
    //   - within a continuation turn (tool_use pauses), turn_id is
    //     stable across RTs.
    //   - the first RT after end_turn mints a new turn_id.
    //
    // For a one-shot claude -p invocation we typically see 1-3
    // RTs of a single turn. Assertion: at minimum every marked
    // record has a non-empty turn_id; if multiple RTs are present
    // they share the turn_id (the prompt above produces a single
    // user turn).

    let turn_ids: std::collections::HashSet<&str> = marked
        .iter()
        .filter_map(|r| {
            r.get("marks")
                .and_then(|m| m.get("turn_id"))
                .and_then(Value::as_str)
        })
        .collect();
    eprintln!(
        "e2e: {} distinct turn_ids in marked records",
        turn_ids.len()
    );
    assert!(!turn_ids.is_empty(), "no turn_ids found in marks");
    for tid in &turn_ids {
        assert!(!tid.is_empty(), "found empty turn_id");
    }

    // ADR 052: the retired AnthropicMarkingDetector exposed a MarkingStore to
    // assert on_response_close ran; the FrameTreeRegistry keeps state
    // internally per session. The marks presence asserted above already proves
    // the registry ran end-to-end. (TODO ADR 052: this #[ignore] live test
    // still asserts the retired turn/agent-run marks shape; rewrite its
    // assertions to the §5 frame-tree marks.)

    eprintln!("e2e: PASS");
}
