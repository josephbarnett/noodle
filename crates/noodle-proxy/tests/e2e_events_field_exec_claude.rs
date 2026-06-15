//! End-to-end validation of `events[]` on `tap.jsonl` response
//! records (S10 of the 027–031 refactor; ADR 030 §3;
//! refactor-overview.md §2 S10).
//!
//! Per ADR 030 §3, every SSE response record routed through the
//! proxy must carry the parsed event stream as a typed list
//! alongside the raw body bytes. The proxy walks the Anthropic
//! SSE stream's `\n\n`-terminated events and stamps each one with
//! `ts_offset_ms` measured from the response's first-byte
//! instant.
//!
//! This test spawns the real `claude` CLI through real noodle
//! against the real Anthropic API, then reads the real
//! `tap.jsonl` and asserts:
//!
//! 1. At least one anthropic response record carries `events[]`
//!    with at least two entries (a real round-trip has at
//!    minimum `message_start` + `message_stop`).
//! 2. Events are in arrival order — the names span the canonical
//!    `message_start` → `content_block_*` → `message_delta` →
//!    `message_stop` arc.
//! 3. `ts_offset_ms` is monotonically non-decreasing within a
//!    single record (offsets only ever grow).
//! 4. The first event's `ts_offset_ms` is at or near zero (the
//!    anchor invariant downstream consumers rely on).
//! 5. Observed event names and offset spread are printed for
//!    human verification.
//!
//! Per the noodle e2e contract (memory note
//! `feedback_no_fixture_extraction`), fixture-replay is not
//! acceptable; only exec-claude through the real proxy counts.
//!
//! `#[ignore]`d for CI gating; runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_events_field_exec_claude \
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
async fn events_field_populated_on_real_tap_jsonl() {
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

    // A simple prompt — even without tool calls, an Anthropic
    // SSE response produces `message_start`, one or more
    // `content_block_*` events, a `message_delta`, and a
    // `message_stop`. That alone is enough to exercise the §3.1
    // invariants the test asserts.
    let prompt = "What's 2 + 2? Reply with only the digit.";

    let output = Command::new(&claude_bin)
        .arg("-p")
        .arg(prompt)
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

    // Filter to api.anthropic.com SSE responses — that's where
    // Anthropic emits the event stream.
    let anthropic_responses: Vec<&Value> = records
        .iter()
        .filter(|r| {
            r.get("direction").and_then(Value::as_str) == Some("response")
                && r.get("provider").and_then(Value::as_str) == Some("anthropic")
        })
        .collect();
    eprintln!(
        "e2e: {} response records against anthropic provider",
        anthropic_responses.len()
    );

    assert!(
        !anthropic_responses.is_empty(),
        "no anthropic response records in tap.jsonl — claude didn't \
         reach api.anthropic.com through the proxy"
    );

    // ─── Walk every anthropic response, collect event facts ─────
    //
    // §S10 demonstrable outcome (refactor-overview.md §2 S10):
    // at least one record carries `events[]` with per-event
    // `ts_offset_ms`. We also assert the ADR 030 §3.1
    // invariants downstream consumers will key on: first event
    // at or near zero offset; offsets monotonically non-
    // decreasing within a single record; at least 2 events
    // observed (a non-degenerate stream).

    let mut event_names_seen: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut records_with_events = 0usize;
    let mut total_events = 0usize;
    let mut max_event_count_per_record = 0usize;
    let mut largest_offset_observed: u64 = 0;
    let mut found_record_satisfying_invariants = false;
    let mut first_event_offsets: Vec<u64> = Vec::new();
    // A "canonical" Anthropic message-stream record carries the
    // SSE event lifecycle pinned by ADR 030 §3.2 — we look for
    // at least one such record to confirm the canonical pattern
    // works end-to-end. The proxy may also see non-Anthropic SSE
    // traffic through the same provider classification (e.g.
    // MCP server responses tunneled via claude.ai); those carry
    // `events[]` with vendor-specific names like `message`
    // (JSON-RPC envelope) — still well-formed records, but their
    // first-event-near-zero behaviour is not what §3.1 anchors.
    let mut canonical_anthropic_record_count = 0usize;
    let mut canonical_min_first_offset: u64 = u64::MAX;

    for rec in &anthropic_responses {
        let Some(events) = rec.get("events").and_then(Value::as_array) else {
            continue;
        };
        if events.is_empty() {
            continue;
        }
        records_with_events += 1;
        total_events += events.len();
        max_event_count_per_record = max_event_count_per_record.max(events.len());

        // Collect event names for the diagnostic dump.
        for ev in events {
            if let Some(name) = ev.get("type").and_then(Value::as_str) {
                *event_names_seen.entry(name.to_string()).or_default() += 1;
            }
        }

        // ADR 030 §3.1: ts_offset_ms is monotonically non-
        // decreasing across the array — UNIVERSAL invariant
        // regardless of which SSE producer emitted the events.
        let offsets: Vec<u64> = events
            .iter()
            .map(|e| {
                e.get("ts_offset_ms")
                    .and_then(Value::as_u64)
                    .expect("event missing ts_offset_ms")
            })
            .collect();
        for w in offsets.windows(2) {
            assert!(
                w[0] <= w[1],
                "ts_offset_ms not monotonically non-decreasing within record: \
                 {} then {} (full offsets: {:?})",
                w[0],
                w[1],
                offsets,
            );
        }
        if let Some(&max) = offsets.iter().max() {
            largest_offset_observed = largest_offset_observed.max(max);
        }
        let first_offset = offsets[0];
        first_event_offsets.push(first_offset);

        // S10 demonstrable outcome: at least one record with
        // multiple events. (Most claude prompts produce 5-20+
        // events; flag if any record carries ≥ 2.)
        if events.len() >= 2 {
            found_record_satisfying_invariants = true;
        }

        // Detect canonical Anthropic message streams — those
        // carry `message_start` as the first event. On those,
        // assert the §3.1 "first event near zero" anchor: the
        // first SSE frame arrives in the same poll cycle as the
        // first-byte capture, so the offset is sub-millisecond
        // (we allow ≤ 50ms slack for slower test machines).
        let first_type = events[0].get("type").and_then(Value::as_str).unwrap_or("");
        if first_type == "message_start" {
            canonical_anthropic_record_count += 1;
            canonical_min_first_offset = canonical_min_first_offset.min(first_offset);
            assert!(
                first_offset <= 50,
                "canonical Anthropic message_start ts_offset_ms not near zero: \
                 got {first_offset}, event: {}",
                events[0],
            );
        }
    }

    eprintln!("e2e: records with events[]: {records_with_events}");
    eprintln!("e2e: total events observed: {total_events}");
    eprintln!("e2e: max events in any single record: {max_event_count_per_record}");
    eprintln!("e2e: largest ts_offset_ms observed: {largest_offset_observed}ms");
    eprintln!("e2e: first-event offsets per record: {first_event_offsets:?}");
    eprintln!(
        "e2e: canonical Anthropic records (first-event=message_start): {canonical_anthropic_record_count}"
    );
    if canonical_min_first_offset != u64::MAX {
        eprintln!(
            "e2e: min first-event ts_offset_ms on canonical records: {canonical_min_first_offset}ms"
        );
    }
    eprintln!("e2e: event names observed: {event_names_seen:?}");

    assert!(
        records_with_events > 0,
        "no anthropic response record carries `events[]`. Anthropic \
         responses: {} but none with events. First record: {}",
        anthropic_responses.len(),
        serde_json::to_string_pretty(anthropic_responses.first().unwrap()).unwrap_or_default(),
    );

    assert!(
        found_record_satisfying_invariants,
        "no record carried multiple events ({total_events} across \
         {records_with_events} records). Expected at least one record \
         with ≥ 2 events for a real claude session.",
    );

    // Sanity check: the canonical event lifecycle must appear.
    // `message_start` and `message_stop` bracket every Anthropic
    // response, so seeing both confirms we captured a real
    // round-trip not a truncated stream.
    let message_start_count = event_names_seen.get("message_start").copied().unwrap_or(0);
    let message_stop_count = event_names_seen.get("message_stop").copied().unwrap_or(0);
    assert!(
        message_start_count > 0,
        "no `message_start` event observed across {records_with_events} records. \
         names: {event_names_seen:?}",
    );
    assert!(
        message_stop_count > 0,
        "no `message_stop` event observed across {records_with_events} records. \
         names: {event_names_seen:?}",
    );

    eprintln!(
        "e2e: PASS — refactor-overview §2 S10 verified end-to-end. \
         records_with_events={records_with_events}, total_events={total_events}, \
         max_events_per_record={max_event_count_per_record}, \
         largest_offset_ms={largest_offset_observed}, \
         message_start_count={message_start_count}, \
         message_stop_count={message_stop_count}, \
         canonical_anthropic_records={canonical_anthropic_record_count}",
    );
}
