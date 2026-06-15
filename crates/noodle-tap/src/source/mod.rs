//! `WireSource` implementations for `noodle-tap`.
//!
//! The read-side dual of [`crate::sink`] (ADR 027 §2.1; refactor
//! overview §2 S12–S13). Each concrete impl pairs with a `WireSink`:
//!
//! - [`FileTail`] (S12) reads the live-tailed `tap.jsonl`
//!   that [`crate::sink::TapJsonlLog`] is writing.
//! - [`FileRead`] (this slice, S13) reads a finished `tap.jsonl` to EOF.
//!
//! Both yield records as [`serde_json::Value`] — the writer-side
//! [`crate::contract::TapEntry`] only derives `Serialize`, so a typed
//! `Deserialize` round-trip isn't available without a coordinated bump.
//! Keeping the read-side untyped also keeps it forward-compatible:
//! new fields on the writer flow through transparently.

pub mod file_read;
pub mod file_tail;

pub use file_read::{FileRead, FileReadError};
pub use file_tail::{CloseHandle, DEFAULT_POLL_INTERVAL, FileTail, FileTailError};
