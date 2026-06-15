//! End-to-end validation of the envelope-level operational-context
//! block (S6 of the 027–031 refactor; ADR 029 §2.4).
//!
//! Per the ADR every `tap.jsonl` record carries an `envelope`
//! object with three sub-fields:
//!
//! - `envelope.agent_app` — typed [`AgentApp`] for the harness
//!   that originated the request, parsed from `User-Agent`.
//! - `envelope.machine` — host facts (`hostname`, `os_family`,
//!   `architecture`, etc.).
//! - `envelope.collector_app` — the noodle build (version,
//!   git SHA, build date, features).
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API. The proxy stamps the envelope
//! at request open; the sink writes it onto every JSONL record.
//! The test reads the real `tap.jsonl` and asserts:
//!
//! 1. **Every record** carries an `envelope` block.
//! 2. **Every record's `envelope.collector_app.name` is
//!    `"noodle"`** — the build-info embedding worked.
//! 3. **Every record's `envelope.machine` is populated** — at
//!    least `os_family` and `architecture` are present
//!    (deterministic from `std::env::consts`).
//! 4. **At least one record carries
//!    `envelope.agent_app.name == "claude_code"`** — claude
//!    sends a recognizable User-Agent and the proxy parses it.
//!
//! Per the noodle e2e contract, fixture-replay is not acceptable;
//! only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_envelope_fields_exec_claude \
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
async fn envelope_fields_appear_on_real_tap_jsonl() {
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

    // ─── Assertion 1: every record carries an envelope block ───

    let with_envelope = records
        .iter()
        .filter(|r| r.get("envelope").is_some_and(Value::is_object))
        .count();
    assert_eq!(
        with_envelope,
        records.len(),
        "{} of {} records missing `envelope` block — \
         envelope stamping isn't reaching every wire event",
        records.len() - with_envelope,
        records.len(),
    );

    // ─── Assertion 2: collector_app.name == "noodle" everywhere ─

    for (i, rec) in records.iter().enumerate() {
        let name = rec
            .get("envelope")
            .and_then(|e| e.get("collector_app"))
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("record {i} missing envelope.collector_app.name"));
        assert_eq!(
            name, "noodle",
            "record {i}: envelope.collector_app.name = {name:?}, want \"noodle\""
        );
    }

    // collector_app should also carry a version + build_hash on
    // every record (compile-time embedded).
    let first_collector = records[0]
        .get("envelope")
        .and_then(|e| e.get("collector_app"))
        .expect("first record envelope.collector_app");
    let version = first_collector
        .get("version")
        .and_then(Value::as_str)
        .expect("collector_app.version is a string");
    assert!(!version.is_empty(), "collector_app.version is empty");
    let build_hash = first_collector
        .get("build_hash")
        .and_then(Value::as_str)
        .expect("collector_app.build_hash is a string");
    assert!(
        !build_hash.is_empty(),
        "collector_app.build_hash is empty — build.rs didn't run?"
    );
    eprintln!("e2e: collector_app — version={version}, build_hash={build_hash}");

    // ─── Assertion 3: machine block populated everywhere ───────

    for (i, rec) in records.iter().enumerate() {
        let machine = rec
            .get("envelope")
            .and_then(|e| e.get("machine"))
            .unwrap_or_else(|| panic!("record {i} missing envelope.machine"));
        let os_family = machine
            .get("os_family")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("record {i} machine.os_family not a string"));
        let arch = machine
            .get("architecture")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("record {i} machine.architecture not a string"));
        // Open-set membership — any of the snake_case names from
        // the enum is acceptable.
        assert!(
            matches!(os_family, "macos" | "linux" | "windows" | "unknown"),
            "record {i}: machine.os_family = {os_family:?}"
        );
        assert!(
            matches!(arch, "x86_64" | "aarch64" | "unknown"),
            "record {i}: machine.architecture = {arch:?}"
        );
    }
    let first_machine = records[0]
        .get("envelope")
        .and_then(|e| e.get("machine"))
        .expect("first record envelope.machine");
    eprintln!(
        "e2e: machine — {}",
        serde_json::to_string(&first_machine).unwrap()
    );

    // ─── Assertion 4: at least one record claims claude_code ───
    //
    // claude code sends a recognizable User-Agent on its
    // `/v1/messages` calls. If we see ZERO records carrying
    // `agent_app.name == "claude_code"`, either the UA parser
    // missed the format claude is shipping or the envelope
    // isn't being stamped from headers.

    let claude_code_records = records
        .iter()
        .filter(|r| {
            r.get("envelope")
                .and_then(|e| e.get("agent_app"))
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                == Some("claude_code")
        })
        .count();
    let agent_app_names: Vec<String> = records
        .iter()
        .filter_map(|r| {
            r.get("envelope")
                .and_then(|e| e.get("agent_app"))
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect();
    eprintln!("e2e: agent_app.name values observed: {agent_app_names:?}");
    assert!(
        claude_code_records > 0,
        "no records carried envelope.agent_app.name = \"claude_code\" — \
         the UA parser didn't recognize claude's User-Agent header"
    );

    // Source should be UserAgentHeader (snake_case
    // `user_agent_header`) wherever a claude_code agent_app is
    // present.
    for rec in &records {
        let Some(agent) = rec.get("envelope").and_then(|e| e.get("agent_app")) else {
            continue;
        };
        if agent.get("name").and_then(Value::as_str) == Some("claude_code") {
            let source = agent
                .get("source")
                .and_then(Value::as_str)
                .expect("agent_app.source");
            assert_eq!(source, "user_agent_header");
        }
    }

    eprintln!("e2e: PASS — envelope ADR 029 §2.4 verified end-to-end");
}
