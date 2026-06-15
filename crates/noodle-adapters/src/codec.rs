#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! `ProviderCodec` and `CodecRegistry` driven adapters.
//!
//! Real codec impls (`OpenAiCodec`, `AnthropicCodec`) land per the
//! build plan. Today we ship the no-op registry so the engine can be
//! wired without yet having a concrete codec.

use std::sync::Arc;

use noodle_core::{CodecRegistry, ProviderCodec, RequestProbe};

/// Registry that never matches — the engine falls through to
/// pass-through behaviour for every request.
pub struct NoOpCodecRegistry;

impl NoOpCodecRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NoOpCodecRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CodecRegistry for NoOpCodecRegistry {
    fn select(&self, _probe: &RequestProbe<'_>) -> Option<Arc<dyn ProviderCodec>> {
        None
    }
}

/// Ordered list of codecs; first whose `matches` fires wins.
/// Pattern: Factory.
#[deprecated(
    since = "0.0.1",
    note = "use `noodle_core::layered::CodecRegistry` — the layered codec stack has its own per-layer typed registry. Removal tracked under A.8.b."
)]
pub struct OrderedCodecRegistry {
    codecs: Vec<Arc<dyn ProviderCodec>>,
}

impl OrderedCodecRegistry {
    #[must_use]
    pub fn new(codecs: Vec<Arc<dyn ProviderCodec>>) -> Self {
        Self { codecs }
    }
}

impl CodecRegistry for OrderedCodecRegistry {
    fn select(&self, probe: &RequestProbe<'_>) -> Option<Arc<dyn ProviderCodec>> {
        self.codecs.iter().find(|c| c.matches(probe)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, Method, Uri};
    use noodle_core::{BodyStream, EventStream, ResponseShape};

    use super::*;

    struct AlwaysMatch {
        name: &'static str,
    }
    impl ProviderCodec for AlwaysMatch {
        fn name(&self) -> &'static str {
            self.name
        }
        fn matches(&self, _probe: &RequestProbe<'_>) -> bool {
            true
        }
        fn decode(&self, _parts: &ResponseShape, body: BodyStream) -> EventStream {
            // Not exercised in these tests.
            let _ = body;
            unimplemented!()
        }
        fn encode(&self, _parts: &ResponseShape, events: EventStream) -> BodyStream {
            let _ = events;
            unimplemented!()
        }
    }

    struct NeverMatch;
    impl ProviderCodec for NeverMatch {
        fn name(&self) -> &'static str {
            "never"
        }
        fn matches(&self, _probe: &RequestProbe<'_>) -> bool {
            false
        }
        fn decode(&self, _parts: &ResponseShape, body: BodyStream) -> EventStream {
            let _ = body;
            unimplemented!()
        }
        fn encode(&self, _parts: &ResponseShape, events: EventStream) -> BodyStream {
            let _ = events;
            unimplemented!()
        }
    }

    fn probe<'a>(method: &'a Method, uri: &'a Uri, headers: &'a HeaderMap) -> RequestProbe<'a> {
        RequestProbe {
            method,
            uri,
            headers,
        }
    }

    #[test]
    fn noop_registry_returns_none() {
        let m = Method::GET;
        let u: Uri = "http://x/".parse().unwrap();
        let h = HeaderMap::new();
        let p = probe(&m, &u, &h);
        assert!(NoOpCodecRegistry::new().select(&p).is_none());
    }

    #[test]
    fn ordered_registry_first_match_wins() {
        let m = Method::GET;
        let u: Uri = "http://x/".parse().unwrap();
        let h = HeaderMap::new();
        let p = probe(&m, &u, &h);

        let reg = OrderedCodecRegistry::new(vec![
            Arc::new(NeverMatch),
            Arc::new(AlwaysMatch { name: "first" }),
            Arc::new(AlwaysMatch { name: "second" }),
        ]);
        let selected = reg.select(&p).expect("should match");
        assert_eq!(selected.name(), "first");
    }

    #[test]
    fn ordered_registry_no_match_returns_none() {
        let m = Method::GET;
        let u: Uri = "http://x/".parse().unwrap();
        let h = HeaderMap::new();
        let p = probe(&m, &u, &h);
        let reg = OrderedCodecRegistry::new(vec![Arc::new(NeverMatch), Arc::new(NeverMatch)]);
        assert!(reg.select(&p).is_none());
    }
}
