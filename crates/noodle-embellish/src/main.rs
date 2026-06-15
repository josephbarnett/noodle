//! `noodle-embellish` CLI binary.
//!
//! Two modes:
//!
//! - **Batch** (default): read a `tap.jsonl` file end-to-end, pair every
//!   request/response, emit one `ai-telemetry` v0.0.2 row per pair, print
//!   a summary, exit.
//! - **Tail** (`--watch`): follow a live `tap.jsonl` the proxy is writing,
//!   mapping new pairs as they arrive. This is the mode the Kubernetes
//!   sidecar (ADR 043) runs — a one-shot batch process would map the
//!   empty startup file and exit, never seeing live traffic.
//!
//! ```sh
//! # one-shot
//! noodle-embellish --tap /path/to/tap.jsonl --db /path/to/out.sqlite
//! # continuous sidecar
//! noodle-embellish --watch --tap /path/to/tap.jsonl --db /path/to/out.sqlite
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use noodle_embellish::{
    Embellisher, EmbellisherStats, FileTailSource, RoundTripView, read_roundtrips_jsonl,
};

#[derive(Debug, Parser)]
#[command(
    name = "noodle-embellish",
    about = "Embellishment processor — read tap.jsonl, write ai-telemetry v0.0.2 events to SQLite.",
    long_about = None,
    version,
)]
struct Args {
    /// Path to the `tap.jsonl` file written by the noodle proxy.
    #[arg(long, value_name = "FILE")]
    tap: PathBuf,

    /// Path to the output `SQLite` database. Created if it doesn't
    /// exist; reused if it does (subject to schema-version check).
    #[arg(long, value_name = "FILE")]
    db: PathBuf,

    /// Tail `tap.jsonl` continuously, mapping new pairs as the proxy
    /// writes them. Without this flag the binary does a single
    /// read-to-EOF batch pass and exits.
    #[arg(long)]
    watch: bool,

    /// Tail poll interval in milliseconds (watch mode only). The driver
    /// sleeps this long when `tap.jsonl` has no new complete lines.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    poll_ms: u64,
}

fn main() -> ExitCode {
    if let Err(err) = run() {
        eprintln!("noodle-embellish: error: {err:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    let mut embellisher = Embellisher::open(&args.db)
        .with_context(|| format!("opening sqlite db at {}", args.db.display()))?;

    if args.watch {
        run_watch(&mut embellisher, &args.tap, args.poll_ms)
    } else {
        run_batch(&mut embellisher, &args.tap)
    }
}

fn run_batch(embellisher: &mut Embellisher, tap: &Path) -> anyhow::Result<()> {
    let stats = embellisher
        .process_file(tap)
        .with_context(|| format!("processing tap.jsonl at {}", tap.display()))?;
    println!(
        "noodle-embellish: read={} requests={} responses={} rows_written={} unpaired_req={} orphan_resp={}",
        stats.records_read,
        stats.requests,
        stats.responses,
        stats.rows_written,
        stats.unpaired_requests,
        stats.orphan_responses
    );
    Ok(())
}

/// Continuous tail driver. Follows `tap.jsonl` via `FileTailSource`,
/// refreshing the `roundtrips.jsonl` join index each poll so late-
/// arriving attribution can enrich pairs, and feeding new records into
/// the persistent pairing buffers via `process_record`.
fn run_watch(embellisher: &mut Embellisher, tap: &Path, poll_ms: u64) -> anyhow::Result<()> {
    let roundtrips_path = tap.parent().map_or_else(
        || PathBuf::from("roundtrips.jsonl"),
        |p| p.join("roundtrips.jsonl"),
    );
    let poll = Duration::from_millis(poll_ms);
    let stop = Arc::new(AtomicBool::new(false));
    let mut source = FileTailSource::new(tap.to_path_buf(), poll, stop);
    let mut stats = EmbellisherStats::default();

    eprintln!(
        "noodle-embellish: watching {} → sqlite (poll {}ms); roundtrips from {}",
        tap.display(),
        poll_ms,
        roundtrips_path.display(),
    );

    let mut last_reported_rows = 0usize;
    loop {
        // Best-effort roundtrips refresh — a missing/partial file maps
        // to an empty index (rows still land as `wire_only`).
        let index: HashMap<String, RoundTripView> = read_roundtrips_jsonl(&roundtrips_path)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|rt| rt.event_id().map(str::to_owned).map(|id| (id, rt)))
            .collect();
        embellisher.set_roundtrip_index(index);

        let batch = source
            .poll_batch()
            .with_context(|| format!("tailing tap.jsonl at {}", tap.display()))?;

        if batch.is_empty() {
            std::thread::sleep(poll);
            continue;
        }

        for record in batch {
            embellisher.process_record(record, &mut stats)?;
        }

        if stats.rows_written != last_reported_rows {
            eprintln!(
                "noodle-embellish: rows_written={} requests={} responses={}",
                stats.rows_written, stats.requests, stats.responses,
            );
            last_reported_rows = stats.rows_written;
        }
    }
}
