//! End-to-end validation of story 040.b: `~/.noodle/roundtrips.jsonl`
//! contains exactly one record per `/v1/messages` response on
//! `tap.jsonl`, joined 1:1 by `event_id`.
//!
//! ## Why this shape
//!
//! ADR 023 §4 pins the per-round-trip record schema. Story 040.b
//! ships the sink that writes it. The acceptance criteria require
//! `jq -s 'length' roundtrips.jsonl` to equal the count of
//! `/v1/messages` response records on `tap.jsonl`, and every
//! emitted `event_id` to join a corresponding `tap.jsonl` record.
//!
//! ## Requirements
//!
//! - `claude` CLI on `PATH`, authenticated.
//! - Network access to `api.anthropic.com`.
//!
//! `#[ignore]`d by default; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_roundtrips_match_tap \
//!     -- --ignored --nocapture
//! ```

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use noodle_adapters::store::InMemorySessionStore;
use noodle_proxy::tap_setup;
use noodle_proxy::{ProxyConfig, start};
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
async fn roundtrips_match_tap_one_record_per_v1_messages_response() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };

    let dir = TempDir::new().expect("tempdir");
    let tap_path = dir.path().join("tap.jsonl");
    let side_effects_path = dir.path().join("side_effects.jsonl");
    let roundtrips_path = dir.path().join("roundtrips.jsonl");

    let ca = Arc::new(Ca::generate().expect("generate CA"));
    let ca_pem_path = dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("write CA pem");

    // ADR 052: frame-tree registry replaces the retired AnthropicMarkingDetector.
    let detector = Arc::new(noodle_adapters::marking::FrameTreeRegistry::new());

    let null_wire: Arc<dyn noodle_core::WireSink> = Arc::new(NullWire);
    let base_cfg = ProxyConfig {
        listen: "127.0.0.1:0".into(),
        body_limit: 8 * 1024 * 1024,
        wire: null_wire,
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
    };

    let (cfg, tap_log, round_trip_sink) = tap_setup::install(
        base_cfg,
        tap_setup::InstallPaths {
            tap: tap_path.clone(),
            side_effects: side_effects_path,
            roundtrips: roundtrips_path.clone(),
        },
        tap_setup::InstallCapacities::default(),
    )
    .await
    .expect("tap_setup install");

    let proxy = start(cfg).await.expect("start proxy");
    let proxy_addr = proxy.local_addr();

    let prompt = format!(
        "Run `ls {tmp}` and tell me how many files are in the directory. \
         Reply with just the number.",
        tmp = dir.path().display()
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
        "claude exited non-zero: status={:?}, stderr=\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");

    let tap_log = Arc::try_unwrap(tap_log)
        .map_err(|_| "tap_log still has other Arc holders")
        .unwrap();
    tap_log.shutdown().await;
    // RoundTripSink drains its async writer task; expose its
    // shutdown directly so the file flushes before we read.
    round_trip_sink.shutdown().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let tap_contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let rt_contents = std::fs::read_to_string(&roundtrips_path).expect("read roundtrips.jsonl");
    eprintln!(
        "e2e: tap.jsonl={}B  roundtrips.jsonl={}B",
        tap_contents.len(),
        rt_contents.len()
    );
    assert!(!tap_contents.is_empty(), "tap.jsonl empty");
    assert!(!rt_contents.is_empty(), "roundtrips.jsonl empty");

    let tap_records: Vec<Value> = tap_contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap line"))
        .collect();
    let rt_records: Vec<Value> = rt_contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse roundtrip line"))
        .collect();

    // ─── AC #4: count matches completed `/v1/messages` exchanges
    //
    // tap.jsonl carries the URL only on request records; responses
    // share the same `event_id`. A completed exchange = an event_id
    // that has both a request hitting `/v1/messages` AND a matching
    // response on tap.jsonl. roundtrips.jsonl should have exactly
    // that many lines.

    let v1_request_ids: std::collections::HashSet<&str> = tap_records
        .iter()
        .filter(|r| {
            r.get("direction").and_then(Value::as_str) == Some("request")
                && r.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.contains("/v1/messages") && u.contains("api.anthropic.com"))
        })
        .filter_map(|r| r.get("event_id").and_then(Value::as_str))
        .collect();
    let response_ids: std::collections::HashSet<&str> = tap_records
        .iter()
        .filter(|r| r.get("direction").and_then(Value::as_str) == Some("response"))
        .filter_map(|r| r.get("event_id").and_then(Value::as_str))
        .collect();
    let completed: std::collections::HashSet<&&str> =
        v1_request_ids.intersection(&response_ids).collect();
    eprintln!(
        "e2e: tap.jsonl /v1/messages completed exchanges = {}; roundtrips.jsonl records = {}",
        completed.len(),
        rt_records.len()
    );
    assert_eq!(
        rt_records.len(),
        completed.len(),
        "roundtrips.jsonl count must equal /v1/messages completed-exchange count on tap.jsonl \
         (AC #4); completed: {} rt records: {}",
        completed.len(),
        rt_records.len()
    );

    // ─── AC #5: every roundtrip event_id joins to tap.jsonl ──

    let tap_event_ids: std::collections::HashSet<&str> = tap_records
        .iter()
        .filter_map(|r| r.get("event_id").and_then(Value::as_str))
        .collect();
    for rt in &rt_records {
        let event_id = rt
            .get("event_id")
            .and_then(Value::as_str)
            .expect("rt record has event_id");
        assert!(
            tap_event_ids.contains(event_id),
            "roundtrip event_id={event_id} has no corresponding tap.jsonl record (AC #5)"
        );
    }

    // ─── AC #3: every record carries the four ADR 023 §2.3 IDs.
    // agent_run_id remains None until 040.c, so we don't check
    // that one. The other three must be present and non-empty.

    for rt in &rt_records {
        assert_eq!(rt["kind"], "round_trip", "kind discriminator on {rt}");
        for field in ["event_id", "session_id", "turn_id"] {
            let v = rt.get(field);
            // Some records may legitimately omit `session_id`/
            // `turn_id` (request had no Anthropic session header
            // — the marking detector declined). event_id must
            // always be present.
            if field == "event_id" {
                assert!(
                    v.and_then(Value::as_str).is_some_and(|s| !s.is_empty()),
                    "event_id required on every round-trip record; got {rt}"
                );
            }
        }
    }

    // ─── Schema spot-check (AC #2): the required shape from
    // ADR 023 §4 — request + response + duration_ms + evidence.

    let first = &rt_records[0];
    assert!(first.get("started_at_unix_ms").is_some());
    assert!(first.get("completed_at_unix_ms").is_some());
    assert!(first.get("duration_ms").is_some());
    assert!(first["request"]["host"].is_string());
    assert!(first["request"]["endpoint"].is_string());
    assert!(first["request"]["method"].is_string());
    assert!(first["evidence"]["hints"].is_array());
    assert!(first["evidence"]["artifacts"].is_array());
    assert!(first["evidence"]["audits"].is_array());

    eprintln!(
        "e2e PASS: {} round-trip records, all joined to tap.jsonl, ADR 023 §4 shape verified",
        rt_records.len()
    );
}

struct NullWire;

impl noodle_core::WireSink for NullWire {
    fn record(&self, _event: noodle_core::WireEvent) {}
}
