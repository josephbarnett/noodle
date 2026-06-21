//! Offline OTLP trace emitter (story 061, ADR 057 — dev-only) — CLI.
//!
//! Replays a committed capture through the **real** ADR 052 §5 frame-tree
//! detector and the **real** story-060 shipper exporter so a reconstructed
//! `GenAI` trace lands in a local `otel-collector → Tempo → Grafana` stack —
//! proving the `correlate → assemble → OTLP → collector → Tempo → Grafana`
//! path without a live proxy + Claude run. Reconstruction lives in the crate
//! library ([`noodle_trace_emitter::reconstruct`]); this is the POST wrapper.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use noodle_shipper::exporter::build_resource_spans_payload;
use noodle_shipper::{OtlpExporter, Transport};
use noodle_trace_emitter::{DEFAULT_CAPTURE, reconstruct};

#[derive(Parser)]
#[command(
    about = "Replay a committed capture into a local OTLP collector as a reconstructed GenAI trace (story 061)."
)]
struct Args {
    /// Capture directory holding `*_request.json` files with `_link` blocks.
    #[arg(long, default_value = DEFAULT_CAPTURE)]
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
