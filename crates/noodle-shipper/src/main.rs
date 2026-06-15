//! `noodle-shipper` binary entry point.
//!
//! Reads `ai-telemetry` rows from `noodle-embellish`'s `SQLite` rollups
//! database, maps each to an OTLP `LogRecord`, and POSTs to a
//! configurable `OTel` collector endpoint. Designed to run as a
//! long-lived process per ADR 022 §3; the polling cadence + retry
//! cap + batch size are all CLI-configurable.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use noodle_shipper::{
    DEFAULT_BATCH_SIZE, DEFAULT_MAX_RETRIES, DEFAULT_POLL_INTERVAL_SECS, Shipper, ShipperConfig,
    Transport,
};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[derive(Debug, Parser)]
#[command(
    name = "noodle-shipper",
    about = "Read ai-telemetry rollups, emit OTLP/HTTP to a collector (story 043, ADR 022 §3)."
)]
struct Args {
    /// Path to the rollups `SQLite` database. Defaults to
    /// `$NOODLE_ROLLUPS_DB` or
    /// `~/.noodle/rollups.db` to
    /// mirror the existing macOS shipper convention (E5 §A).
    #[arg(long, env = "NOODLE_ROLLUPS_DB")]
    db: Option<PathBuf>,

    /// `OTel` collector endpoint (e.g. `http://127.0.0.1:4318`). The
    /// shipper appends `/v1/logs`. Required — there is no useful
    /// default for production deployments.
    #[arg(long, env = "NOODLE_OTLP_ENDPOINT")]
    endpoint: String,

    /// Rows claimed per cycle.
    #[arg(long, default_value_t = DEFAULT_BATCH_SIZE)]
    batch: usize,

    /// Seconds between polling cycles.
    #[arg(long, default_value_t = DEFAULT_POLL_INTERVAL_SECS)]
    poll_secs: u64,

    /// Failure cap before a row is moved to `'poison'`.
    #[arg(long, default_value_t = DEFAULT_MAX_RETRIES)]
    max_retries: u32,

    /// Print the current per-state row counts and exit instead of
    /// running the loop. Useful for ops scripts.
    #[arg(long)]
    status: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            EnvFilter::builder()
                .with_default_directive(tracing::level_filters::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let args = Args::parse();
    let db_path = resolve_db_path(args.db);
    let cfg = ShipperConfig {
        db_path,
        endpoint: args.endpoint,
        transport: Transport::HttpJson,
        batch_size: args.batch,
        poll_interval: Duration::from_secs(args.poll_secs),
        max_retries: args.max_retries,
    };

    let shipper = Shipper::new(cfg)?;

    if args.status {
        let counts = shipper.counts()?;
        println!(
            "noodle-shipper: pending={pending} in_flight={in_flight} delivered={delivered} retry={retry} poison={poison}",
            pending = counts.pending,
            in_flight = counts.in_flight,
            delivered = counts.delivered,
            retry = counts.retry,
            poison = counts.poison,
        );
        return Ok(());
    }

    shipper.run().await?;
    Ok(())
}

fn resolve_db_path(arg: Option<PathBuf>) -> PathBuf {
    if let Some(p) = arg {
        return p;
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".noodle/rollups.db")
}
