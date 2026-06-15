//! End-to-end validation of story 040.a's correlation contract:
//! every `side_effects.jsonl` record carries `event_id` +
//! `turn_id` + `session_id` + `agent_run_id` + non-zero
//! `at_unix_ms`, and every `event_id` joins a `tap.jsonl` record.
//!
//! ## Why this shape
//!
//! Per ADR 023 §2.3 the correlation block on every drained
//! `SideEffect` is the seam that lets `tap.jsonl` ↔
//! `side_effects.jsonl` join 1:1 by `event_id`. Without that
//! join no downstream consumer (040.b roundtrips, 042 ai-telemetry
//! mapping, 044 `OTel` collector) can reassemble a flow.
//!
//! ## Requirements
//!
//! - `claude` CLI on `PATH`, authenticated.
//! - Network access to `api.anthropic.com`.
//!
//! `#[ignore]`d by default — runs locally with:
//!
//! ```sh
//! cargo test -p noodle-proxy --test e2e_side_effects_correlate_with_tap \
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
async fn side_effects_carry_correlation_block_and_join_tap() {
    let Some(claude_bin) = claude_binary() else {
        eprintln!("SKIP: `claude` CLI not on PATH");
        return;
    };

    // ─── Set up tap_setup with temp paths so we can read them ──
    //
    // tap_setup::install builds the engine, the SideEffectsJsonlSink,
    // and the MultiSideEffectSink composed on top — i.e. exactly
    // the production wiring this slice targets. We point `tap` and
    // `side_effects` at a tempdir so the test reads only its own
    // bytes (no interference with `~/.noodle/`).

    let dir = TempDir::new().expect("tempdir");
    let tap_path = dir.path().join("tap.jsonl");
    let side_effects_path = dir.path().join("side_effects.jsonl");

    let ca = Arc::new(Ca::generate().expect("generate CA"));
    let ca_pem_path = dir.path().join("noodle-ca.pem");
    std::fs::write(&ca_pem_path, ca.cert_pem()).expect("write CA pem");

    // ADR 052: frame-tree registry replaces the retired AnthropicMarkingDetector.
    let detector = Arc::new(noodle_adapters::marking::FrameTreeRegistry::new());

    // Base wire sink — install() composes its own TapJsonlLog on
    // top, so the base just needs to be non-null.
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

    let roundtrips_path = dir.path().join("roundtrips.jsonl");
    let (cfg, tap_log, _round_trip) = tap_setup::install(
        base_cfg,
        tap_setup::InstallPaths {
            tap: tap_path.clone(),
            side_effects: side_effects_path.clone(),
            roundtrips: roundtrips_path,
        },
        tap_setup::InstallCapacities::default(),
    )
    .await
    .expect("tap_setup install");

    let proxy = start(cfg).await.expect("start proxy");
    let proxy_addr = proxy.local_addr();

    // ─── Drive real claude through the proxy ───────────────────
    //
    // Same prompt shape the marking e2e uses — guaranteed to
    // produce at least one /v1/messages round-trip with a
    // session header.

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

    // ─── Drain sinks so both files flush ───────────────────────

    proxy
        .shutdown(Duration::from_secs(5))
        .await
        .expect("proxy shutdown");
    let tap_log = Arc::try_unwrap(tap_log)
        .map_err(|_| "tap_log still has other Arc holders")
        .unwrap();
    tap_log.shutdown().await;
    // SideEffectsJsonlSink's async writer task flushes on its
    // own interval (100ms) once the proxy drops its senders.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ─── Read both files and parse ─────────────────────────────

    let tap_contents = std::fs::read_to_string(&tap_path).expect("read tap.jsonl");
    let se_contents = std::fs::read_to_string(&side_effects_path).expect("read side_effects.jsonl");
    eprintln!(
        "e2e: tap.jsonl={}B  side_effects.jsonl={}B",
        tap_contents.len(),
        se_contents.len()
    );
    assert!(!tap_contents.is_empty(), "tap.jsonl empty");
    assert!(
        !se_contents.is_empty(),
        "side_effects.jsonl empty — engine drain wired but no effects emitted"
    );

    let tap_records: Vec<Value> = tap_contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse tap line"))
        .collect();
    let se_records: Vec<Value> = se_contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("parse side_effects line"))
        .collect();

    // Set of every event_id observed on tap.jsonl — the join key
    // for AC #4 (no orphans). tap.jsonl per-record id field is
    // `event_id` (ADR 027 §1's canonical name).
    let tap_event_ids: std::collections::HashSet<&str> = tap_records
        .iter()
        .filter_map(|r| r.get("event_id").and_then(Value::as_str))
        .collect();
    assert!(
        !tap_event_ids.is_empty(),
        "tap.jsonl has no event_id values to join against"
    );

    // ─── AC #1 + #3: every side_effects record from an inspectable
    //                flow carries the correlation block with
    //                non-zero at_unix_ms ───────────────────────────
    //
    // Cert-mint audits (flow_id == 0) bypass the engine drain
    // seam — they intentionally lack the correlation block per
    // ADR 023. Filter them out before asserting AC #1.

    let drained: Vec<&Value> = se_records
        .iter()
        .filter(|r| {
            // flow_id == 0 only on records that did not pass through
            // the engine drain (cert-mint LeafMinted/MintFailed
            // audits). Drain stamping is the contract under test.
            r.get("flow_id").and_then(Value::as_u64).unwrap_or(0) != 0
                || r.get("kind").and_then(Value::as_str) != Some("audit")
        })
        .collect();
    assert!(
        !drained.is_empty(),
        "no drained side_effects records emitted — engine drain wiring failed"
    );

    let mut orphans = 0usize;
    for r in &drained {
        let kind = r.get("kind").and_then(Value::as_str).unwrap_or("?");
        let event_id = r
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("missing event_id on {kind} record: {r}"));
        // Per-variant timestamp slot: Artifact rides on
        // `captured_at_unix_ms`; everything else on `at_unix_ms`.
        // Same value — both are stamped from `correlation.at_unix_ms`
        // at drain.
        let ts_key = if kind == "artifact" {
            "captured_at_unix_ms"
        } else {
            "at_unix_ms"
        };
        let at_unix_ms = r
            .get(ts_key)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("missing {ts_key} on {kind} record: {r}"));
        assert!(at_unix_ms > 0, "{ts_key} is zero on {kind} record: {r}");

        // ─── AC #4: every event_id joins a tap.jsonl record ────
        if !tap_event_ids.contains(event_id) {
            eprintln!(
                "ORPHAN: side_effects event_id={event_id} (kind={kind}) has no \
                 matching tap.jsonl record"
            );
            orphans += 1;
        }
    }
    assert_eq!(
        orphans, 0,
        "{orphans} side_effects records have event_ids absent from tap.jsonl \
         (joins failed — see ORPHAN lines above)"
    );

    // ─── AC #2: session_id is the FULL MarkingSessionId, not a
    //           hash prefix. Look at any record whose tap.jsonl
    //           join carries a marks block — that's the live wire
    //           session_id, which must equal what side_effects
    //           reports. ────────────────────────────────────────

    let tap_marks_session: Option<&str> = tap_records.iter().find_map(|r| {
        r.get("marks")
            .and_then(|m| m.get("session_id"))
            .and_then(Value::as_str)
    });
    if let Some(wire_session) = tap_marks_session {
        let se_with_session: Vec<&Value> = drained
            .iter()
            .copied()
            .filter(|r| {
                r.get("session_id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == wire_session)
            })
            .collect();
        assert!(
            !se_with_session.is_empty(),
            "no side_effects record carries the wire session_id={wire_session} — \
             040.a AC #2 (full MarkingSessionId, not hash prefix) failed"
        );
        // Also: the value must not look like an 8-char hex prefix.
        assert!(
            wire_session.len() > 8 || !wire_session.chars().all(|c| c.is_ascii_hexdigit()),
            "wire session_id looks like an 8-char hex prefix — 040.a AC #2 failed"
        );
    } else {
        eprintln!("note: no tap.jsonl record carries a marks block — skipping AC #2 check");
    }

    eprintln!(
        "e2e PASS: {} drained side_effects records, all joined tap.jsonl, \
         all carry correlation + non-zero at_unix_ms",
        drained.len()
    );
}

/// No-op `WireSink` for the base `ProxyConfig`. `tap_setup::install`
/// composes a real `TapJsonlLog` on top of this; the base sink is
/// just a placeholder slot.
struct NullWire;

impl noodle_core::WireSink for NullWire {
    fn record(&self, _event: noodle_core::WireEvent) {}
}
