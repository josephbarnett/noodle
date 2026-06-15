//! Real-claude end-to-end test for [`AnthropicDecoder`]
//! (refactor-overview.md §2 S14; ADR 029 §7).
//!
//! Spawns the real `claude` CLI through real noodle against the
//! real Anthropic API, lets it issue at least one tool call,
//! flushes the tap sink, then opens a [`WireSource`] on the
//! captured `tap.jsonl`, runs [`AnthropicDecoder`] against it, and
//! asserts:
//!
//! 1. The decoder produced > 0 events.
//! 2. At least one [`DecodedEvent::TurnStart`] is observed.
//! 3. At least one [`DecodedEvent::TurnEnd`] is observed.
//! 4. At least one [`DecodedEvent::Content`] is observed.
//! 5. At least one [`DecodedEvent::ToolUse`] is observed with a
//!    non-empty `tool_use_id`.
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! Source: the test uses an inline batch reader on the finished
//! `tap.jsonl` (S13's `WireSource::FileRead` hasn't landed yet —
//! tracked in refactor-overview.md §2). The decoder is unchanged;
//! the source-agnostic contract means the same code path runs
//! against whichever `WireSource` impl ships first.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-domain --test e2e_anthropic_decoder_exec_claude \
//!     -- --ignored --nocapture
//! ```

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_core::WireSource;
use noodle_domain::decoders::{AnthropicDecoder, DecodedEvent, ProviderDecoder};
use noodle_domain::envelope_metadata::ProviderId;
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

/// Inline batch reader over a finished `tap.jsonl`. Substitutes for
/// `noodle-tap::source::FileRead` (S13) which hasn't merged yet.
/// Identical contract: `Ok(Some(record))` per JSONL line, then
/// `Ok(None)` at EOF (batch mode per `WireSource` trait docs).
struct VecBatchSource {
    records: VecDeque<Value>,
}

impl VecBatchSource {
    fn from_jsonl_string(s: &str) -> Self {
        let records: VecDeque<Value> = s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("parse tap.jsonl line"))
            .collect();
        Self { records }
    }
}

impl WireSource for VecBatchSource {
    type Record = Value;
    type Error = std::convert::Infallible;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        Ok(self.records.pop_front())
    }
}

#[tokio::test]
#[ignore = "requires claude CLI + valid Anthropic auth; runs locally"]
#[allow(clippy::too_many_lines)]
async fn anthropic_decoder_produces_typed_events_against_real_tap_jsonl() {
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

    // Prompt shaped to force at least one tool call (Bash / Read /
    // LS) so we exercise the ToolUse code path. Identical pattern
    // to e2e_content_blocks_exec_claude.
    let prompt = format!(
        "Run `ls {tmp}` and then briefly describe what's in the directory.",
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

    // ─── Open the captured tap.jsonl as a WireSource ─────────
    let contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    eprintln!("e2e: tap.jsonl size: {} bytes", contents.len());
    let mut src = VecBatchSource::from_jsonl_string(&contents);
    eprintln!("e2e: tap.jsonl records loaded: {}", src.records.len());

    // ─── Drive the decoder until EOF ────────────────────────
    let dec = AnthropicDecoder::new();
    let mut events: Vec<DecodedEvent> = Vec::new();
    loop {
        // The decoder consumes one record per call. We track when
        // it's at EOF by detecting that no record was pulled.
        let before = src.records.len();
        for ev in dec.decode_record(&mut src) {
            events.push(ev);
        }
        if src.records.len() == before {
            break;
        }
    }
    eprintln!("e2e: decoder produced {} events", events.len());

    // ─── Assertions ─────────────────────────────────────────
    assert!(
        !events.is_empty(),
        "decoder produced zero events despite a real anthropic session"
    );

    // Every emitted event must be tagged as anthropic.
    for ev in &events {
        assert_eq!(
            ev.provider(),
            &ProviderId::Anthropic,
            "non-anthropic event leaked through anthropic decoder: {ev:?}"
        );
    }

    let turn_starts = events
        .iter()
        .filter(|e| matches!(e, DecodedEvent::TurnStart { .. }))
        .count();
    let turn_ends = events
        .iter()
        .filter(|e| matches!(e, DecodedEvent::TurnEnd { .. }))
        .count();
    let contents_count = events
        .iter()
        .filter(|e| matches!(e, DecodedEvent::Content { .. }))
        .count();
    let tool_uses: Vec<&DecodedEvent> = events
        .iter()
        .filter(|e| matches!(e, DecodedEvent::ToolUse { .. }))
        .collect();

    eprintln!(
        "e2e: counts — turn_start={turn_starts} turn_end={turn_ends} \
         content={contents_count} tool_use={}",
        tool_uses.len()
    );

    assert!(
        turn_starts > 0,
        "no TurnStart events decoded; events={events:#?}"
    );
    assert!(
        turn_ends > 0,
        "no TurnEnd events decoded; events={events:#?}"
    );
    assert!(
        contents_count > 0,
        "no Content events decoded; events={events:#?}"
    );
    assert!(
        !tool_uses.is_empty(),
        "no ToolUse events decoded — claude should have called a tool. \
         events={events:#?}"
    );

    // Every observed ToolUse must have a non-empty tool_use_id.
    for ev in &tool_uses {
        if let DecodedEvent::ToolUse {
            tool_use_id,
            tool_name,
            ..
        } = ev
        {
            assert!(
                !tool_use_id.is_empty(),
                "ToolUse with empty tool_use_id: tool_name={tool_name}"
            );
            assert!(
                !tool_name.is_empty(),
                "ToolUse with empty tool_name (id={tool_use_id})"
            );
        }
    }

    eprintln!(
        "e2e: PASS — S14 verified end-to-end. \
         turn_start={turn_starts} turn_end={turn_ends} \
         content={contents_count} tool_use={}",
        tool_uses.len()
    );
}
