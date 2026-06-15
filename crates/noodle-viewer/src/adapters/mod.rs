//! Adapters — concrete implementations of the ports in `crate::ports`.

pub mod http_debug_proxy;
pub mod side_effects_jsonl_source;
pub mod tap_jsonl_frames_source;
pub mod tap_jsonl_source;

pub use http_debug_proxy::HttpDebugProxy;
pub use side_effects_jsonl_source::SideEffectsJsonlSource;
pub use tap_jsonl_frames_source::TapJsonlFramesSource;
pub use tap_jsonl_source::{DecodedTapJsonlSource, TapJsonlSource};
