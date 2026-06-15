//! Exec-claude e2e for the typed [`DecodedExchange`] feed
//! (S21 of the 027–031 refactor — refactor-overview.md §10).
//!
//! Spawns the real `claude` CLI through a real noodle proxy whose
//! `WireSink` is a real `TapJsonlLog`. The viewer's [`HubService`]
//! tails the same `tap.jsonl` through the new
//! [`DecodedTapJsonlSource`] and broadcasts typed
//! [`DecodedExchange`]s. After claude exits we assert:
//!
//! - At least one [`DecodedExchange`] has `marks.turn_id` populated
//!   (the proxy's marking detector stamped it).
//! - At least one carries a `content_blocks` entry of kind `Content`
//!   (text) OR `ToolUse`.
//! - At least one carries `usage.tokens.input > 0`.
//! - At least one carries `envelope.collector_app.name == "noodle"`.
//! - At least one carries `envelope.agent_app.name == AgentAppName::ClaudeCode`
//!   (`snake_case` `claude_code`).
//!
//! Observed values (`turn_ids`, token counts, content-block kinds)
//! are printed on stderr for human verification. Per the noodle e2e
//! contract (memory rule `feedback_no_fixture_extraction`),
//! fixture-replay is not acceptable; only exec-claude through real
//! noodle counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-viewer --test e2e_decoded_exchange_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::WireSink;
use noodle_domain::decoders::DecodedEvent;
use noodle_domain::observation_context::AgentAppName;
use noodle_proxy::{ProxyConfig, start};
use noodle_tap::TapJsonlLog;
use noodle_tls::ca::Ca;
use noodle_viewer::adapters::DecodedTapJsonlSource;
use noodle_viewer::hub::HubService;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn decoded_exchanges_carry_typed_fields_against_real_claude_session() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };
    eprintln!("e2e (S21): claude binary: {claude_bin}");

    let tap_dir = TempDir::new().expect("tempdir");
    let tap_path = tap_dir.path().join("tap.jsonl");
    eprintln!("e2e (S21): tap.jsonl path: {}", tap_path.display());

    let tap_sink = Arc::new(
        TapJsonlLog::spawn(tap_path.clone(), 1024)
            .await
            .expect("spawn tap sink"),
    );

    let ca = Arc::new(Ca::generate().expect("generate test CA"));
    let ca_pem_path = tap_dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("write CA pem");

    // Wire a real anthropic marking detector — without it the
    // proxy emits records but never stamps `marks.session_id` /
    // `marks.turn_id`, and the S21 e2e asserts the typed marks
    // path round-trips end-to-end.
    // ADR 052: frame-tree registry replaces the retired AnthropicMarkingDetector.
    let detector = Arc::new(noodle_adapters::marking::FrameTreeRegistry::new());

    let proxy = start(ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 8 * 1024 * 1024,
        wire: Arc::clone(&tap_sink) as Arc<dyn WireSink>,
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
    eprintln!("e2e (S21): noodle proxy listening on {proxy_addr}");

    // Belt-and-suspenders wait for the tap file to appear.
    for _ in 0..40 {
        if std::fs::metadata(&tap_path).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(std::fs::metadata(&tap_path).is_ok());

    // ── Real viewer hub + DecodedTapJsonlSource ─────────────────
    let hub = HubService::new();
    let decoded_source = DecodedTapJsonlSource::spawn(tap_path.clone(), 1024)
        .await
        .expect("spawn decoded tap source");
    let (_history, mut decoded_rx) = hub.subscribe_decoded().await;
    hub.attach_decoded_source(&decoded_source);

    // ── Drain the decoded broadcast into a Vec ──────────────────
    let (collect_tx, mut collect_rx) =
        tokio::sync::mpsc::unbounded_channel::<noodle_viewer::model::DecodedExchange>();
    let drain = tokio::spawn(async move {
        while let Ok(dx) = decoded_rx.recv().await {
            if collect_tx.send((*dx).clone()).is_err() {
                break;
            }
        }
    });

    eprintln!("e2e (S21): viewer hub running; launching claude");

    // ── Drive claude through noodle ─────────────────────────────
    //
    // Prompt shaped to provoke at least one tool call so the
    // ToolUse content-block assertion has signal. Same shape as
    // the S14 decoder e2e — `ls` is the cheapest tool we can rely
    // on across deployments.
    let prompt = format!(
        "Run `ls {tmp}` and briefly summarise what's in the directory.",
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
    eprintln!(
        "e2e (S21): claude stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Give the writer task and the tail one cycle to flush.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Shutdown ─────────────────────────────────────────────────
    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let sink = Arc::try_unwrap(tap_sink)
        .map_err(|_| "tap_sink still has other Arc holders")
        .unwrap();
    sink.shutdown().await;
    decoded_source.close();
    drop(decoded_source);

    // Drain the in-flight records, then stop the drain task.
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut collected: Vec<noodle_viewer::model::DecodedExchange> = Vec::new();
    while tokio::time::Instant::now() < drain_deadline {
        let timeout = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(timeout.min(Duration::from_millis(100)), collect_rx.recv()).await
        {
            Ok(Some(dx)) => collected.push(dx),
            Ok(None) | Err(_) => break,
        }
    }
    while let Ok(dx) = collect_rx.try_recv() {
        collected.push(dx);
    }
    drain.abort();
    let _ = drain.await;

    eprintln!(
        "e2e (S21): collected {} DecodedExchanges from the typed feed",
        collected.len()
    );
    assert!(
        !collected.is_empty(),
        "no DecodedExchange surfaced — claude didn't reach the proxy or the typed source dropped every record"
    );

    // ── Assertions on the typed shape ───────────────────────────

    // Print a sample of observed values for human verification.
    let turn_id_samples: Vec<&str> = collected
        .iter()
        .filter_map(|dx| {
            dx.marks
                .as_ref()
                .and_then(|m| m.turn_id.as_ref())
                .map(noodle_core::TurnId::as_str)
        })
        .take(5)
        .collect();
    eprintln!("e2e (S21): turn_id samples: {turn_id_samples:?}");

    let token_samples: Vec<(u64, u64)> = collected
        .iter()
        .filter_map(|dx| dx.usage.as_ref())
        .filter_map(|u| u.tokens.as_ref().map(|t| (t.input, t.output)))
        .take(5)
        .collect();
    eprintln!("e2e (S21): token (input,output) samples: {token_samples:?}");

    let block_kinds: Vec<&'static str> = collected
        .iter()
        .flat_map(|dx| dx.content_blocks.iter())
        .map(|e| match e {
            DecodedEvent::TurnStart { .. } => "turn_start",
            DecodedEvent::TurnEnd { .. } => "turn_end",
            DecodedEvent::Content { .. } => "content",
            DecodedEvent::ToolUse { .. } => "tool_use",
            DecodedEvent::VendorSpecific { .. } => "vendor_specific",
        })
        .collect();
    eprintln!(
        "e2e (S21): observed {} decoded events; kinds histogram: \
         content={} tool_use={} turn_start={} turn_end={} vendor_specific={}",
        block_kinds.len(),
        block_kinds.iter().filter(|k| **k == "content").count(),
        block_kinds.iter().filter(|k| **k == "tool_use").count(),
        block_kinds.iter().filter(|k| **k == "turn_start").count(),
        block_kinds.iter().filter(|k| **k == "turn_end").count(),
        block_kinds
            .iter()
            .filter(|k| **k == "vendor_specific")
            .count(),
    );

    // 1. At least one record has marks.turn_id non-empty.
    let has_turn_id = collected.iter().any(|dx| {
        dx.marks
            .as_ref()
            .and_then(|m| m.turn_id.as_ref())
            .is_some_and(|t| !t.as_str().is_empty())
    });
    assert!(
        has_turn_id,
        "no DecodedExchange carried marks.turn_id — the proxy did not stamp a marks block"
    );

    // 2. At least one content block of kind text or tool_use.
    let has_text_or_tool = collected
        .iter()
        .flat_map(|dx| dx.content_blocks.iter())
        .any(|e| {
            matches!(
                e,
                DecodedEvent::Content { .. } | DecodedEvent::ToolUse { .. }
            )
        });
    assert!(
        has_text_or_tool,
        "no Content / ToolUse DecodedEvent — the anthropic decoder didn't emit any content blocks"
    );

    // 3. At least one usage.tokens.input > 0.
    let has_input_tokens = collected
        .iter()
        .filter_map(|dx| dx.usage.as_ref())
        .filter_map(|u| u.tokens.as_ref())
        .any(|t| t.input > 0);
    assert!(
        has_input_tokens,
        "no usage.tokens.input > 0 — the proxy didn't surface a usage block"
    );

    // 4. envelope.collector_app.name == "noodle".
    let collector_match = collected
        .iter()
        .filter_map(|dx| dx.envelope.as_ref())
        .filter_map(|e| e.collector_app.as_ref())
        .any(|c| c.name == "noodle");
    assert!(
        collector_match,
        "no envelope.collector_app.name == \"noodle\" — collector_app stamp missing"
    );

    // 5. envelope.agent_app.name == ClaudeCode.
    let agent_match = collected
        .iter()
        .filter_map(|dx| dx.envelope.as_ref())
        .filter_map(|e| e.agent_app.as_ref())
        .any(|a| a.name == AgentAppName::ClaudeCode);
    assert!(
        agent_match,
        "no envelope.agent_app.name == ClaudeCode — User-Agent parse missed claude_code"
    );

    eprintln!(
        "e2e (S21): PASS — typed DecodedExchange carries marks.turn_id + content_blocks + \
         usage + envelope.collector_app + envelope.agent_app over a real claude session."
    );
}
