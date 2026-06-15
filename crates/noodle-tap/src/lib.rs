//! `noodle-tap` — TAP-format JSONL debugger sink.
//!
//! ## Why this is a separate crate
//!
//! noodle-tap is **not** part of the engine. It depends only on
//! `noodle-core` (for the `WireSink` trait + `WireEvent` shape). It does
//! not depend on `rama`, `noodle-adapters`, or `noodle-proxy`. The engine
//! cannot accidentally call into tap code; the tap cannot accidentally
//! reach engine internals.
//!
//! The proxy crate gates this dependency behind a `tap` cargo feature.
//! Production builds compiled with `--no-default-features` link none of
//! this code.
//!
//! ## Module map (single-responsibility)
//!
//! - [`sink`] — `TapJsonlLog`: the `WireSink` impl. Hot-path code.
//! - [`writer`] — async writer task + bounded `mpsc` channel.
//! - [`contract`] — the JSONL line shape (`TapEntry`). Pinned by
//!   golden-file tests in `tests/contract.rs` so drift from the
//!   external TAP viewer contract is caught immediately.
//! - [`provider`] — host → provider name mapping.
//! - [`session`] — session-hash extraction (header priority + system
//!   prompt SHA-256 fallback).
//! - [`redact`] — strip / mask sensitive header values.
//! - [`timestamp`] — Unix-ms → `RFC3339Nano` UTC.
//!
//! Each module owns one concern. Adding a new agent's session header is
//! one line in `session.rs` plus a test; adding a new provider is one
//! line in `provider.rs` plus a test. No cross-module coupling.

#![forbid(unsafe_code)]

pub mod contract;
pub mod provider;
pub mod redact;
pub mod session;
pub mod sink;
pub mod source;
pub mod timestamp;
pub mod writer;

pub use contract::TapEntry;
pub use sink::TapJsonlLog;
