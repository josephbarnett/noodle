//! Shipper orchestration: cursor + mapper + exporter as one loop.
//!
//! The [`Shipper`] runs the standard at-least-once cycle:
//!
//! 1. On startup, [`recover_in_flight`](crate::cursor::RollupsCursor::recover_in_flight)
//!    resets stale rows.
//! 2. [`claim_batch`](crate::cursor::RollupsCursor::claim_batch) flips
//!    up to N pending/retry rows to `'in_flight'`.
//! 3. The batch is mapped to OTLP and `POSTed`.
//! 4. On success → `ack_delivered`. On failure → `ack_failed`
//!    (transitions to `'retry'` or `'poison'`).
//! 5. Sleep for the poll interval. Repeat.

use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tracing::{info, warn};

use crate::cursor::{CursorError, DeliveryCounts, RollupsCursor};
use crate::exporter::{ExportError, OtlpExporter, Transport};

#[derive(Debug, Error)]
pub enum ShipperError {
    #[error(transparent)]
    Cursor(#[from] CursorError),

    #[error(transparent)]
    Export(#[from] ExportError),
}

#[derive(Debug, Clone)]
pub struct ShipperConfig {
    pub db_path: PathBuf,
    pub endpoint: String,
    pub transport: Transport,
    pub batch_size: usize,
    pub poll_interval: Duration,
    pub max_retries: u32,
}

/// Aggregated counters surfaced by [`Shipper::tick`]. Useful for
/// logging at the end of every loop iteration.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShipperStats {
    pub claimed: u64,
    pub delivered: u64,
    pub failed: u64,
    pub poisoned_now: u64,
}

pub struct Shipper {
    cursor: RollupsCursor,
    exporter: OtlpExporter,
    cfg: ShipperConfig,
}

impl Shipper {
    /// Open the database + build the exporter. Calls
    /// `recover_in_flight` so any rows left in flight by a prior
    /// crashed process are reset before the loop begins.
    pub fn new(cfg: ShipperConfig) -> Result<Self, ShipperError> {
        let mut cursor = RollupsCursor::open(&cfg.db_path, cfg.max_retries)?;
        cursor.recover_in_flight()?;
        let exporter = OtlpExporter::new(&cfg.endpoint, cfg.transport)?;
        Ok(Self {
            cursor,
            exporter,
            cfg,
        })
    }

    /// Run one claim → export → ack cycle. Returns the per-tick
    /// stats. Caller decides whether to sleep + tick again.
    pub async fn tick(&mut self) -> Result<ShipperStats, ShipperError> {
        let batch = self.cursor.claim_batch(self.cfg.batch_size)?;
        let claimed = batch.rows.len() as u64;
        if claimed == 0 {
            return Ok(ShipperStats::default());
        }

        match self.exporter.export(&batch.rows).await {
            Ok(result) => {
                let delivered = result.delivered.len() as u64;
                self.cursor.ack_delivered(&result.delivered)?;
                Ok(ShipperStats {
                    claimed,
                    delivered,
                    failed: 0,
                    poisoned_now: 0,
                })
            }
            Err(e) => {
                let ids: Vec<String> = batch.rows.iter().map(|r| r.event_id.clone()).collect();
                let msg = e.to_string();
                self.cursor.ack_failed(&ids, &msg)?;
                Ok(ShipperStats {
                    claimed,
                    delivered: 0,
                    failed: claimed,
                    poisoned_now: 0,
                })
            }
        }
    }

    /// Drain the loop forever. Sleeps `poll_interval` between
    /// ticks. Designed for the binary's main; tests call `tick`
    /// directly.
    pub async fn run(mut self) -> Result<(), ShipperError> {
        loop {
            match self.tick().await {
                Ok(stats) if stats.claimed > 0 => {
                    info!(
                        target: "noodle::shipper",
                        claimed = stats.claimed,
                        delivered = stats.delivered,
                        failed = stats.failed,
                        "tick complete"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        target: "noodle::shipper",
                        error = %e,
                        "tick failed; backing off"
                    );
                }
            }
            tokio::time::sleep(self.cfg.poll_interval).await;
        }
    }

    /// Return the current delivery-status counts. Used by the
    /// binary's --status mode and by the integration tests.
    pub fn counts(&self) -> Result<DeliveryCounts, CursorError> {
        self.cursor.counts()
    }
}
