//! Pure embellishment library.
//!
//! Carved out of `noodle-embellish` per ADR 039 §4 — the
//! file/CLI/SQLite shell stays in the original binary crate; this
//! crate hosts the pure mapper, decoder driver, and JSONL reader
//! the plugin facade re-exports.
//!
//! Modules:
//!
//! - [`mapper`] — translates `tap.jsonl` records (raw + decoded)
//!   into the `ai-telemetry` v0.0.2 wire shape.
//! - [`decoded`] — drives the per-provider
//!   [`noodle_domain::decoders::ProviderDecoder`] over a paired
//!   tap request/response.
//! - [`reader`] — JSONL view types ([`reader::TapEntryView`],
//!   [`reader::RoundTripView`]) and the `read_tap_jsonl` /
//!   `read_roundtrips_jsonl` batch helpers.
//! - [`wire_source`] — file-backed [`noodle_core::WireSource`] impls:
//!   [`wire_source::FileReadSource`] (S13, batch) and
//!   [`wire_source::FileTailSource`] (S12, tail). The consumption seam
//!   ADR 044's continuous embellish and `ParquetSink` read through.
//! - [`brain`] — ADR 047 rung 1 turn-over-turn diff +
//!   `context_management` directive lift. Pure, stateful per-thread
//!   observer; the caller holds a [`brain::Brain`] across round
//!   trips and merges each [`brain::BrainObservation`] into the
//!   OTLP record (`brain.*` attributes).
//! - [`policy`] — Watchtower D2 observe-mode classifier port (ADR
//!   045 §2.2 / §2.4 / §2.5). Per-pair [`policy::PolicyDecision`]
//!   stamped onto the OTLP record as `policy.*` attributes.

#![forbid(unsafe_code)]

pub mod brain;
pub mod decoded;
pub mod mapper;
pub mod policy;
pub mod reader;
pub mod wire_source;

pub use brain::{Brain, BrainObservation, UTILITY_THREAD_ID};
pub use decoded::{DecodedPair, decode_pair};
pub use mapper::{TelemetryRow, map_decoded_pair, map_pair};
pub use policy::{
    AllowAllClassifier, BashDestructiveClassifier, ChainClassifier, PolicyClassifier,
    PolicyDecision, PolicyMode, PolicySurface, PolicyVerdict,
};
pub use reader::{RoundTripView, TapEntryView, read_roundtrips_jsonl, read_tap_jsonl};
pub use wire_source::{FileReadSource, FileTailSource};
