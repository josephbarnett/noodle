//! `noodle-shipper` — out-of-proxy `OTel` shipper.
//!
//! Reads rows from `noodle-embellish`'s `ai-telemetry` rollups `SQLite`,
//! maps them to OTLP records, and pushes to a configurable `OTel`
//! collector endpoint over OTLP/HTTP. Lives in its own process per
//! ADR 022 §3 (file boundary, separate-process shipper). Story 043.
//!
//! ## Module map
//!
//! - [`cursor`] — the cursor-on-flag state machine (`pending →
//!   in_flight → delivered | failed → retry → poison`) over the
//!   shared `ai_telemetry_v_0_0_2` table.
//! - [`mapping`] — `ai-telemetry` row → OTLP `LogRecord` JSON. The
//!   correlation block lands at resource + record scope per
//!   E4 §B placement strategy.
//! - [`exporter`] — POSTs the assembled OTLP HTTP/JSON to the
//!   collector's `/v1/logs` endpoint.
//! - [`shipper`] — the orchestration loop wiring cursor + mapper +
//!   exporter together for the binary and the integration tests.

#![forbid(unsafe_code)]

pub mod cursor;
pub mod exporter;
pub mod mapping;
pub mod otel_genai;
pub mod shipper;

pub use cursor::{ClaimedBatch, DeliveryStatus, RollupsCursor, RollupsRow};
pub use exporter::{ExportError, ExportResult, OtlpExporter, Transport};
pub use mapping::row_to_otlp_log;
pub use otel_genai::{
    CorrelatedRoundTrip, GenAiSpan, Role, Trace, agent_span, assemble_trace, chat_span,
};
pub use shipper::{Shipper, ShipperConfig, ShipperStats};

/// The retry cap above which rows are moved to `'poison'` and parked.
/// Mirrors the existing macOS shipper's default per E5 §A.
pub const DEFAULT_MAX_RETRIES: u32 = 5;

/// Default polling interval — how often the main loop checks for
/// new pending rows. Slow enough to keep the process near-idle when
/// the collector is healthy and the table is empty.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Default batch size — how many rows the cursor claims per round.
pub const DEFAULT_BATCH_SIZE: usize = 100;
