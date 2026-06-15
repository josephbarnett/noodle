//! Detector driven adapters.
//!
//! Concrete impls land per the build plan: user-agent classifier,
//! system-prompt-hash, GitHub owner/repo extractor, etc. Today only
//! the no-op is shipped — it lets the engine wire the trait without
//! a real impl until provider-specific detectors arrive.

use noodle_core::{Detector, FlowResolver, HintWriter};

/// Emits no hints. Useful as a placeholder while the real detector
/// stack is being filled in.
pub struct NoOpDetector;

impl NoOpDetector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NoOpDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for NoOpDetector {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn detect(&self, _flow: &dyn FlowResolver, _hints: &mut dyn HintWriter) {}
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::HeaderMap;
    use noodle_core::{Session, SessionKey, VecHintWriter};

    use super::*;

    struct StubFlow {
        session: Session,
        req_body: Bytes,
        headers: HeaderMap,
        host: String,
        provider: String,
    }
    impl FlowResolver for StubFlow {
        fn host(&self) -> &str {
            &self.host
        }
        fn provider(&self) -> Option<&str> {
            Some(&self.provider)
        }
        fn header(&self, _name: &str) -> Option<&str> {
            None
        }
        fn request_headers(&self) -> &HeaderMap {
            &self.headers
        }
        fn response_headers(&self) -> Option<&HeaderMap> {
            None
        }
        fn request_body(&self) -> &Bytes {
            &self.req_body
        }
        fn response_body(&self) -> Option<&Bytes> {
            None
        }
        fn session(&self) -> &Session {
            &self.session
        }
    }

    #[test]
    fn noop_detector_emits_no_hints() {
        let id = SessionKey {
            auth_header: b"x",
            session_header: b"y",
        }
        .id();
        let flow = StubFlow {
            session: Session::new(id),
            req_body: Bytes::new(),
            headers: HeaderMap::new(),
            host: "api.openai.com".into(),
            provider: "openai".into(),
        };
        let mut hints = VecHintWriter::new();
        let det = NoOpDetector::new();
        det.detect(&flow, &mut hints);
        assert!(hints.into_hints().is_empty());
        assert_eq!(det.name(), "noop");
    }
}
