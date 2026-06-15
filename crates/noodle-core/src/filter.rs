//! `Filter` port — streaming text rewrite.
//!
//! Stateful per-flow. Holds partial-match state across event
//! boundaries. Modifies the bytes the client sees AND surfaces any
//! markers it captured along the way.
//!
//! Pattern: Strategy + Factory. Filters are stateful, so the engine
//! asks a `FilterFactory` for a fresh `Filter` per streaming response.
//!
//! The canonical concrete impl is `MarkerStripFilter` in
//! `noodle-adapters`, which wraps `MarkerScanner` (see
//! `noodle-core/src/marker.rs`).

use crate::{MarkerHit, Session};

pub struct FilterContext<'a> {
    pub provider: &'a str,
    pub session: &'a Session,
}

/// Output of `Filter::process` or `Filter::flush`. Mirrors
/// `marker::ScanOutput` but with `String` bytes since `Filter`
/// operates on decoded text (UTF-8 by SSE convention).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterOutput {
    /// Bytes to emit to the downstream consumer.
    pub bytes: String,
    /// Markers captured (and removed from `bytes`) in this call.
    pub markers: Vec<MarkerHit>,
}

impl FilterOutput {
    #[must_use]
    pub fn passthrough(chunk: &str) -> Self {
        Self {
            bytes: chunk.to_owned(),
            markers: Vec::new(),
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

pub trait FilterFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Construct a fresh, owned filter for this response.
    fn make(&self, ctx: &FilterContext<'_>) -> Box<dyn Filter>;
}

pub trait Filter: Send + 'static {
    /// Process a chunk of decoded text. Returns the bytes to emit
    /// downstream and any markers captured in this call. Held bytes
    /// (suspect prefix of a marker) are not in `bytes`; they release
    /// later if the suspicion proves wrong.
    fn process(&mut self, chunk: &str) -> FilterOutput;

    /// End-of-stream flush. Releases any held bytes verbatim and any
    /// markers still in flight, so a partial match never silently
    /// swallows trailing input.
    fn flush(&mut self) -> FilterOutput;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn filter_factory_is_object_safe() {
        let _v: Vec<Arc<dyn FilterFactory>> = Vec::new();
    }

    #[test]
    fn filter_box_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Box<dyn Filter>>();
    }

    #[test]
    fn passthrough_helper_keeps_input() {
        let o = FilterOutput::passthrough("hi");
        assert_eq!(o.bytes, "hi");
        assert!(o.markers.is_empty());
    }

    #[test]
    fn empty_helper_is_default() {
        let o = FilterOutput::empty();
        assert_eq!(o, FilterOutput::default());
    }
}
