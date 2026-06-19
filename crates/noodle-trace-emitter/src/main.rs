//! Offline OTLP trace emitter (story 061, ADR 057 — dev-only).
//!
//! Replays a committed capture through the **real** ADR 052 §5 frame-tree
//! detector ([`noodle_adapters::marking`]) and the **real** story-060 shipper
//! exporter ([`noodle_shipper::OtlpExporter`]) so a reconstructed `GenAI` trace
//! lands in a local `otel-collector → Tempo → Grafana` stack — proving the
//! `correlate → assemble → OTLP → collector → Tempo → Grafana` path without a
//! live proxy + Claude run.
//!
//! It reads each `*_request.json` in order, reconstructs the round-trip's
//! marks, maps it to a [`RollupsRow`], and POSTs the batch. The capture carries
//! no wall-clock timestamps, so the emitter **synthesises** an ordered clock
//! from each entry's `_link.global_seq` (1 s spacing, fixed 800 ms latency) —
//! enough for spans to nest and order correctly; it is not real latency.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;

use noodle_adapters::marking::frame_signals::request_signals;
use noodle_adapters::marking::{FrameMarks, FrameTreeDetector, RoundTripSignals};
use noodle_shipper::exporter::build_resource_spans_payload;
use noodle_shipper::{OtlpExporter, RollupsRow, Transport};

/// Synthetic per-round-trip duration (ms). Not real latency — a fixed window so
/// chat spans have non-zero duration and frame spans visibly bracket them.
const SYNTH_LATENCY_MS: i64 = 800;

#[derive(Parser)]
#[command(
    about = "Replay a committed capture into a local OTLP collector as a reconstructed GenAI trace (story 061)."
)]
struct Args {
    /// Capture directory holding `*_request.json` files with `_link` blocks.
    #[arg(long, default_value = "analysis/claude-parallel-subagents")]
    capture: PathBuf,

    /// OTLP collector endpoint (the emitter appends `/v1/traces` + `/v1/logs`).
    #[arg(
        long,
        env = "NOODLE_OTLP_ENDPOINT",
        default_value = "http://127.0.0.1:4318"
    )]
    endpoint: String,

    /// Print the assembled `/v1/traces` payload as JSON instead of sending it.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let rows = reconstruct(&args.capture)?;
    eprintln!(
        "reconstructed {} round-trips from {}",
        rows.len(),
        args.capture.display()
    );

    if args.dry_run {
        let payload = build_resource_spans_payload(&rows);
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let exporter =
        OtlpExporter::new(&args.endpoint, Transport::HttpJson).context("building OTLP exporter")?;
    let result = exporter
        .export(&rows)
        .await
        .with_context(|| format!("exporting to {}", args.endpoint))?;
    eprintln!(
        "exported {} rows to {} (/v1/traces + /v1/logs)",
        result.delivered.len(),
        args.endpoint
    );
    Ok(())
}

/// Replay the capture through the real detector and map every round-trip to a
/// [`RollupsRow`]. Files are processed in lexicographic order, which the corpus
/// names to match the depth-first wire order (`001..NNN`).
fn reconstruct(dir: &Path) -> Result<Vec<RollupsRow>> {
    let files = request_files(dir)?;
    // Anchor the synthetic clock just before now so the reconstructed trace
    // lands inside Tempo's recent-time window and Grafana's default range.
    let base_ms = now_unix_ms() - i64::try_from(files.len()).unwrap_or(0) * 1_000;
    let mut det = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
    let mut rows = Vec::with_capacity(files.len());

    for path in &files {
        let v: Value = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        let link = &v["_link"];

        // Frame identity is the agent-id header; body signals drive side-call /
        // turn classification — exactly the proxy's edge path.
        let agent_id = header(&v["headers"], "x-claude-code-agent-id").map(str::to_owned);
        let body = serde_json::to_vec(&v["body"])?;
        let s = request_signals(&body);
        let marks = det.on_round_trip(&RoundTripSignals {
            max_tokens: s.max_tokens,
            trailing_wrapper_kind: s.trailing_wrapper_kind,
            agent_id,
            side_call: s.side_call,
            stop_reason: link["stop_reason"].as_str().map(str::to_owned),
            response_tool_uses: Vec::new(),
        });

        let seq = link["global_seq"]
            .as_i64()
            .unwrap_or_else(|| i64::try_from(rows.len()).unwrap_or(0) + 1);
        rows.push(row_from(path, base_ms, seq, &v, &marks));
    }
    Ok(rows)
}

/// Current Unix time in milliseconds (saturates to 0 before the epoch — never
/// in practice).
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Build a [`RollupsRow`] from one capture entry + its reconstructed marks.
/// Token counts come from the curated `_link.tokens` block; the session id from
/// the request header. Everything the embellisher would normally derive
/// (brain/policy/context columns) stays `None` — this is a trace-shape proof.
fn row_from(path: &Path, base_ms: i64, seq: i64, v: &Value, marks: &FrameMarks) -> RollupsRow {
    let link = &v["_link"];
    let body = &v["body"];
    let event_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rt")
        .to_owned();

    RollupsRow {
        event_id,
        schema_id: "ai-telemetry".to_owned(),
        schema_version: "0.0.2".to_owned(),
        event_type: "api_call".to_owned(),
        timestamp: base_ms + seq * 1_000,
        provider: "anthropic".to_owned(),
        model: body["model"].as_str().unwrap_or_default().to_owned(),
        endpoint_path: "/v1/messages".to_owned(),
        streaming: true,
        status_code: 200,
        error_type: None,
        latency_ms: SYNTH_LATENCY_MS,
        input_tokens: link["tokens"]["in"].as_i64().unwrap_or(0),
        output_tokens: link["tokens"]["out"].as_i64().unwrap_or(0),
        api_key_prefix: None,
        api_key_type: None,
        session_id: header(&v["headers"], "x-claude-code-session-id").map(str::to_owned),
        session_hash: None,
        client_user_agent: header(&v["headers"], "user-agent").map(str::to_owned),
        agent_version: "0.0.1".to_owned(),
        agent_arch: "aarch64".to_owned(),
        context_json: None,
        provider_metadata_json: None,
        correlation_quality: "full".to_owned(),
        retry_count: 0,
        brain_thread_id: None,
        brain_thread_turn_index: None,
        brain_compaction_detected: None,
        brain_compaction_directive_present: None,
        brain_compaction_directive_kind: None,
        brain_blocks_dropped: None,
        brain_blocks_added: None,
        brain_estimated_window_tokens: None,
        brain_api_context_management_beta: None,
        context_input_tokens: None,
        context_cache_read_tokens: None,
        context_cache_creation_tokens: None,
        context_output_tokens: None,
        context_system_bytes: None,
        context_tools_bytes: None,
        context_tools_count: None,
        context_preamble_bytes: None,
        policy_decision: None,
        policy_mode: None,
        policy_risk: None,
        policy_rule: None,
        policy_rationale: None,
        policy_surface: None,
        turn_id: marks.turn_id.clone(),
        role: Some(marks.role.as_str().to_owned()),
        frame_id: marks.frame_id.clone(),
        parent_frame_id: marks.parent_frame_id.clone(),
        depth: marks.depth.map(i64::from),
    }
}

/// Case-insensitive header lookup over the capture's `headers` object.
fn header<'a>(headers: &'a Value, key: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, val)| val.as_str())
}

/// Sorted `*_request.json` paths in the capture dir.
fn request_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading capture dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_request.json"))
        })
        .collect();
    files.sort();
    anyhow::ensure!(
        !files.is_empty(),
        "no *_request.json files in {}",
        dir.display()
    );
    Ok(files)
}
