//! `Detector` port — read-only context inference from a flow.
//!
//! Pattern: Strategy (one impl per detection signal). Stateless across
//! requests; cross-request state lives on `Session` or in the resolver.

use bytes::Bytes;
use http::HeaderMap;
use smol_str::SmolStr;

use crate::Session;

/// Read-side port — detectors pull HTTP flow data through this
/// interface without coupling to the rama service stack.
pub trait FlowResolver: Send + Sync {
    fn host(&self) -> &str;
    fn provider(&self) -> Option<&str>;
    fn header(&self, name: &str) -> Option<&str>;
    fn request_headers(&self) -> &HeaderMap;
    fn response_headers(&self) -> Option<&HeaderMap>;
    /// Already-buffered request body. Empty `Bytes` if no body.
    fn request_body(&self) -> &Bytes;
    /// Already-buffered response body. `None` if response is still streaming.
    fn response_body(&self) -> Option<&Bytes>;
    fn session(&self) -> &Session;
}

/// Write-side port for ranked hints. Detectors emit hints; the
/// resolver aggregates and ranks them after all detectors finish.
pub trait HintWriter: Send {
    fn write(&mut self, hint: ContextHint);
}

/// Write-side port for direct event-level fields. Detectors that need
/// to set named fields (provider, model, tokens) bypass the
/// ranked-hint path and use this instead.
pub trait FieldWriter: Send {
    fn set(&mut self, name: &str, value: FieldValue);
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Str(SmolStr),
    LongStr(String),
    I64(i64),
    F64(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextHint {
    pub category: SmolStr,
    pub value: SmolStr,
    /// 0.0..=1.0. Higher beats lower; ties broken by detector priority.
    pub confidence: f32,
    /// The detector's `name()`, for tie-break and debug.
    pub source: SmolStr,
}

/// Read-only inference. Stateless. Reads via `FlowResolver`, emits
/// ranked hints via `HintWriter`. Detectors that need to write
/// event-level fields directly use `FieldDetector` instead.
pub trait Detector: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn detect(&self, flow: &dyn FlowResolver, hints: &mut dyn HintWriter);
}

/// Optional extension. Detectors that need to write event-level
/// fields directly implement this; the engine passes the field
/// writer at dispatch time.
pub trait FieldDetector: Detector {
    fn detect_with_fields(
        &self,
        flow: &dyn FlowResolver,
        hints: &mut dyn HintWriter,
        fields: &mut dyn FieldWriter,
    );
}

/// Convenience hint collector that accumulates into a `Vec`.
/// Useful in tests and as the engine's per-flow hint sink.
#[derive(Default)]
pub struct VecHintWriter {
    hints: Vec<ContextHint>,
}

impl VecHintWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn into_hints(self) -> Vec<ContextHint> {
        self.hints
    }

    #[must_use]
    pub fn hints(&self) -> &[ContextHint] {
        &self.hints
    }
}

impl HintWriter for VecHintWriter {
    fn write(&mut self, hint: ContextHint) {
        self.hints.push(hint);
    }
}

/// Discards every field write. Used when a detector wants to
/// operate purely as a hint emitter (the common case).
pub struct DiscardFieldWriter;

impl FieldWriter for DiscardFieldWriter {
    fn set(&mut self, _name: &str, _value: FieldValue) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn detector_is_object_safe() {
        let _v: Vec<Arc<dyn Detector>> = Vec::new();
    }

    #[test]
    fn flow_resolver_is_object_safe() {
        let _b: Option<Box<dyn FlowResolver>> = None;
    }

    #[test]
    fn hint_writer_is_object_safe() {
        let _b: Option<Box<dyn HintWriter>> = None;
    }

    #[test]
    fn vec_hint_writer_collects() {
        let mut w = VecHintWriter::new();
        w.write(ContextHint {
            category: "tool".into(),
            value: "Claude Code".into(),
            confidence: 0.95,
            source: "user_agent".into(),
        });
        w.write(ContextHint {
            category: "tool".into(),
            value: "Cursor".into(),
            confidence: 0.7,
            source: "system_prompt".into(),
        });
        let hints = w.into_hints();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].source.as_str(), "user_agent");
        assert_eq!(hints[1].source.as_str(), "system_prompt");
    }

    #[test]
    fn discard_field_writer_is_writer() {
        let mut w = DiscardFieldWriter;
        w.set("anything", FieldValue::I64(1)); // no panic, no allocation
    }
}
