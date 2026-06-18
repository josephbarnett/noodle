//! Driven-adapter implementations of the marking-detector contract
//! defined in `noodle-core::marking` (ADR 028).
//!
//! - [`in_memory_store`] — `InMemoryMarkingStore`, the single-
//!   process default.
//! - [`anthropic`] — `AnthropicMarkingDetector` implementing the
//!   §5.1 spec for `(api.anthropic.com, /v1/messages,
//!   request→upstream)`.
//!
//! Per ADR 028 §4, the marking contract is per-cell: each cell
//! whose dispatch entry includes a `marking_detector` capability
//! gets its own detector here. `anthropic` is the first; future
//! cells (`claude.ai`, etc.) add their own modules following the
//! same shape.

pub mod anthropic;
pub mod frame_signals;
pub mod frame_tree;
pub mod in_memory_store;
pub mod record;

pub use anthropic::AnthropicMarkingDetector;
pub use frame_tree::{
    FrameMarks, FrameRole, FrameTreeDetector, FrameTreeRegistry, OpenOutcome, RequestSignals,
    ResponseSignals, RoundTripSignals, ToolUse,
};
pub use in_memory_store::InMemoryMarkingStore;
pub use record::{request_record, CaptureClient, FrameHeaders, RequestRecord};
