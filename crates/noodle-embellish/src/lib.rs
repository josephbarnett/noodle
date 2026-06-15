//! `noodle-embellish` — the embellishment processor.
//!
//! Reads `tap.jsonl` line-by-line, pairs each request record with its
//! matching response, applies the ADR 031 §5 `ai-telemetry` v0.0.2
//! mapping, and writes the resulting rows to a local `SQLite` database.
//!
//! This is the **validating consumer** of the tap.jsonl boundary
//! (ADR 027 §2.1 / ADR 031 §1) — it proves the proxy's externalised
//! evidence is consumable end-to-end without forcing the proxy to
//! ship telemetry directly.
//!
//! ## Module map
//!
//! - [`reader`] — minimal batch reader for `tap.jsonl` files (stand-in
//!   `tap.jsonl` view types + the batch `read_tap_jsonl` helper (now a
//!   thin wrapper over the `WireSource::FileRead` impl).
//! - [`wire_source`] — file-backed `WireSource` impls: `FileReadSource`
//!   (S13, batch) and `FileTailSource` (S12, tail). The tail source is
//!   what makes the continuous sidecar real, and is the consumption
//!   seam ADR 044's data plane builds on.
//! - [`decoded`] — runs paired records through
//!   `noodle_domain::decoders::AnthropicDecoder` (refactor slice S23).
//!   The mapper consumes the resulting [`DecodedPair`] instead of
//!   re-parsing the JSON; same decoder the viewer uses (S21).
//! - [`mapper`] — ADR 031 §5 mapping: decoded pair → telemetry row.
//! - [`sqlite`] — schema setup + row writer.
//! - [`embellisher`] — public `Embellisher` API that wires the four
//!   together (read → decode → map → write) for callers (the CLI
//!   binary and the e2e harness).
//!
//! ## Modes
//!
//! - **Batch** (`noodle-embellish --tap … --db …`) — read to EOF, emit
//!   one row per pair, exit. The original S16 shape.
//! - **Tail** (`--watch`) — follow a live `tap.jsonl` via
//!   `WireSource::FileTail`, mapping new pairs as the proxy writes
//!   them. This is the mode the Kubernetes sidecar runs (ADR 043): a
//!   one-shot process would exit at startup and never map live traffic.
//!
//! Still future: multi-target dispatch, retention, and the failure
//! modes from ADR 031 §7 (lock backoff, disk-full pause, partial-event
//! flush).

#![forbid(unsafe_code)]

pub mod embellisher;
pub mod sqlite;

// Pure mapper / decoder driver / JSONL reader live in
// `noodle-embellish-core` (ADR 039 §4 carve-out). Re-export verbatim
// so existing callers keep their import paths.
pub use noodle_embellish_core::{
    DecodedPair, FileReadSource, FileTailSource, RoundTripView, TapEntryView, TelemetryRow,
    decode_pair, decoded, map_decoded_pair, map_pair, mapper, read_roundtrips_jsonl,
    read_tap_jsonl, reader, wire_source,
};

pub use embellisher::{Embellisher, EmbellisherStats};
pub use sqlite::{SCHEMA_ID, SCHEMA_VERSION, SqliteWriter};

/// Version of the processor itself (recorded in
/// `processor_version` on every emitted row per ADR 031 §5.2).
pub const PROCESSOR_VERSION: &str = concat!("noodle-embellish/", env!("CARGO_PKG_VERSION"));
