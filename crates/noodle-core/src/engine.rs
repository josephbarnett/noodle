#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! `InspectionEngine` — the domain core's orchestration object.
//!
//! Holds the driven ports the proxy uses. The driving adapter (rama
//! service stack in `noodle-proxy`) calls into `InspectionPipeline` to
//! process a request.
//!
//! Pattern: Builder. The engine is assembled once at startup; runtime
//! lookups are O(1).
//!
//! Three-role surface (per `docs/adrs/005-trait-refactor.md`):
//!
//! - `codecs` — `CodecRegistry` selecting `ProviderCodec` per request.
//! - `detectors` — group-keyed `Detector`s (`COMMON_GROUP` always
//!   runs; per-host groups run when `flow.provider()` matches).
//! - `enhancers` — `ContextEnhancer`s that mutate outbound bodies and
//!   extract artifacts from responses.
//! - `filters` — `FilterFactory`s that produce per-flow `Filter`s for
//!   streaming text rewrite (e.g. `MarkerStripFilter`).
//! - `categories` — `CategoryConfig` for hint resolution.
//! - `sessions`, `audit` — shared infrastructure.
//! - `wire` — optional debug-only `WireSink`.
//!
//! `NoOp` impls (`NoOpCodecRegistry`, `NoOpDetector`, `NoOpEnhancer`,
//! `PassThroughFilterFactory`) are available in `noodle-adapters` for
//! tests and bootstrap.

use std::collections::HashMap;
use std::sync::Arc;

use smol_str::SmolStr;
use thiserror::Error;

use crate::{
    AuditSink, CategoryConfig, CodecRegistry, ContextEnhancer, ContextHint, Detector,
    FilterFactory, FlowResolver, HintWriter, Resolved, SessionStore, VecHintWriter, WireSink,
    resolve,
};

/// Driving port — what the rama service stack calls into. Defined as a
/// trait so the proxy can be exercised in tests against a fake engine.
pub trait InspectionPipeline: Send + Sync + 'static {
    // The exact request/response signatures live in noodle-proxy because
    // they involve rama Body types; the engine impl is converted into a
    // concrete pipeline there. Keeping this trait empty-but-named here
    // reserves the seam.
}

/// The Common detector group — runs on every flow regardless of host.
pub const COMMON_GROUP: &str = "common";

pub struct InspectionEngine {
    pub codecs: Arc<dyn CodecRegistry>,
    /// Group key → detectors. `COMMON_GROUP` runs on every flow; other
    /// keys run when `flow.provider()` matches.
    pub detectors: HashMap<SmolStr, Vec<Arc<dyn Detector>>>,
    pub enhancers: Vec<Arc<dyn ContextEnhancer>>,
    pub filters: Vec<Arc<dyn FilterFactory>>,
    pub categories: Arc<CategoryConfig>,
    pub sessions: Arc<dyn SessionStore>,
    pub audit: Arc<dyn AuditSink>,
    /// Optional debug-only sink for raw protocol traffic. `None` in
    /// most production builds; `Some(JsonStdoutLog)` for the demo
    /// proxy and tests that want to assert wire output.
    pub wire: Option<Arc<dyn WireSink>>,
}

impl InspectionEngine {
    #[must_use]
    pub fn builder() -> InspectionEngineBuilder {
        InspectionEngineBuilder::default()
    }

    /// Run the read-side of attribution against `flow`: invokes the
    /// `COMMON_GROUP` detectors, then any group whose key matches
    /// `flow.provider()`, accumulates hints, and resolves them against
    /// `categories`.
    ///
    /// Common-first, then provider-specific. Sequential by design — a
    /// fan-out threshold (e.g. parallelize once a group has >3
    /// detectors) is a later refinement.
    #[must_use]
    pub fn detect(&self, flow: &dyn FlowResolver) -> Resolved {
        let mut hints = VecHintWriter::new();
        self.run_group(COMMON_GROUP, flow, &mut hints);
        if let Some(provider) = flow.provider()
            && provider != COMMON_GROUP
        {
            self.run_group(provider, flow, &mut hints);
        }
        resolve(hints.hints(), &self.categories)
    }

    /// Run every detector registered under `group` against `flow`,
    /// emitting their hints into `out`. Silent if the group is empty
    /// or absent.
    fn run_group(&self, group: &str, flow: &dyn FlowResolver, out: &mut dyn HintWriter) {
        let Some(detectors) = self.detectors.get(group) else {
            return;
        };
        for d in detectors {
            d.detect(flow, out);
        }
    }

    /// Direct access to the resolver so callers that have already
    /// accumulated hints (e.g. via `ContextEnhancer::extract`) can reuse the
    /// same resolution logic.
    #[must_use]
    pub fn resolve_hints(&self, hints: &[ContextHint]) -> Resolved {
        resolve(hints, &self.categories)
    }
}

impl InspectionPipeline for InspectionEngine {}

#[derive(Default)]
pub struct InspectionEngineBuilder {
    codecs: Option<Arc<dyn CodecRegistry>>,
    detectors: HashMap<SmolStr, Vec<Arc<dyn Detector>>>,
    enhancers: Vec<Arc<dyn ContextEnhancer>>,
    filters: Vec<Arc<dyn FilterFactory>>,
    categories: Option<Arc<CategoryConfig>>,
    sessions: Option<Arc<dyn SessionStore>>,
    audit: Option<Arc<dyn AuditSink>>,
    wire: Option<Arc<dyn WireSink>>,
}

impl InspectionEngineBuilder {
    #[must_use]
    pub fn codecs(mut self, codecs: Arc<dyn CodecRegistry>) -> Self {
        self.codecs = Some(codecs);
        self
    }

    /// Add one detector to a named group. `COMMON_GROUP` always runs;
    /// other group names match against `flow.provider()`.
    #[must_use]
    pub fn detector(mut self, group: impl Into<SmolStr>, detector: Arc<dyn Detector>) -> Self {
        self.detectors
            .entry(group.into())
            .or_default()
            .push(detector);
        self
    }

    /// Add a detector to the always-run `COMMON_GROUP`.
    #[must_use]
    pub fn common_detector(self, detector: Arc<dyn Detector>) -> Self {
        self.detector(COMMON_GROUP, detector)
    }

    #[must_use]
    pub fn enhancer(mut self, enhancer: Arc<dyn ContextEnhancer>) -> Self {
        self.enhancers.push(enhancer);
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: Arc<dyn FilterFactory>) -> Self {
        self.filters.push(filter);
        self
    }

    #[must_use]
    pub fn categories(mut self, categories: Arc<CategoryConfig>) -> Self {
        self.categories = Some(categories);
        self
    }

    #[must_use]
    pub fn sessions(mut self, sessions: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    #[must_use]
    pub fn audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Attach an optional debug-only `WireSink`.
    #[must_use]
    pub fn wire(mut self, wire: Arc<dyn WireSink>) -> Self {
        self.wire = Some(wire);
        self
    }

    pub fn build(self) -> Result<InspectionEngine, BuildError> {
        Ok(InspectionEngine {
            codecs: self.codecs.ok_or(BuildError::MissingPort("codecs"))?,
            detectors: self.detectors,
            enhancers: self.enhancers,
            filters: self.filters,
            categories: self
                .categories
                .ok_or(BuildError::MissingPort("categories"))?,
            sessions: self.sessions.ok_or(BuildError::MissingPort("sessions"))?,
            audit: self.audit.ok_or(BuildError::MissingPort("audit"))?,
            wire: self.wire,
        })
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("missing required port: {0}")]
    MissingPort(&'static str),
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::HeaderMap;

    use super::*;
    use crate::{AuditEvent, ProviderCodec, RequestProbe, Session, SessionId, SessionKey};

    // ── Stubs for required ports ──────────────────────────────────────

    struct StubCodecs;
    impl CodecRegistry for StubCodecs {
        fn select(&self, _probe: &RequestProbe<'_>) -> Option<Arc<dyn ProviderCodec>> {
            None
        }
    }

    struct StubStore;
    impl SessionStore for StubStore {
        fn get_or_init(&self, id: &SessionId) -> Arc<Session> {
            Arc::new(Session::new(id.clone()))
        }
    }

    struct StubAudit;
    impl AuditSink for StubAudit {
        fn record(&self, _event: AuditEvent) {}
    }

    /// Build the smallest "everything wired" builder. Drop one port and
    /// build to assert `MissingPort`.
    fn complete_builder() -> InspectionEngineBuilder {
        InspectionEngine::builder()
            .codecs(Arc::new(StubCodecs))
            .categories(Arc::new(CategoryConfig::default()))
            .sessions(Arc::new(StubStore))
            .audit(Arc::new(StubAudit))
    }

    // ── Builder validation ────────────────────────────────────────────

    #[test]
    fn complete_builder_succeeds() {
        complete_builder().build().expect("build ok");
    }

    #[test]
    fn missing_codecs_errors() {
        let b = InspectionEngine::builder()
            .categories(Arc::new(CategoryConfig::default()))
            .sessions(Arc::new(StubStore))
            .audit(Arc::new(StubAudit));
        let Err(err) = b.build() else {
            panic!("expected MissingPort error");
        };
        assert!(matches!(err, BuildError::MissingPort("codecs")));
    }

    #[test]
    fn missing_categories_errors() {
        let b = InspectionEngine::builder()
            .codecs(Arc::new(StubCodecs))
            .sessions(Arc::new(StubStore))
            .audit(Arc::new(StubAudit));
        let Err(err) = b.build() else {
            panic!("expected MissingPort error");
        };
        assert!(matches!(err, BuildError::MissingPort("categories")));
    }

    #[test]
    fn missing_sessions_errors() {
        let b = InspectionEngine::builder()
            .codecs(Arc::new(StubCodecs))
            .categories(Arc::new(CategoryConfig::default()))
            .audit(Arc::new(StubAudit));
        let Err(err) = b.build() else {
            panic!("expected MissingPort error");
        };
        assert!(matches!(err, BuildError::MissingPort("sessions")));
    }

    #[test]
    fn missing_audit_errors() {
        let b = InspectionEngine::builder()
            .codecs(Arc::new(StubCodecs))
            .categories(Arc::new(CategoryConfig::default()))
            .sessions(Arc::new(StubStore));
        let Err(err) = b.build() else {
            panic!("expected MissingPort error");
        };
        assert!(matches!(err, BuildError::MissingPort("audit")));
    }

    #[test]
    fn detectors_default_to_empty() {
        let engine = complete_builder().build().expect("build ok");
        assert!(engine.detectors.is_empty());
        assert!(engine.enhancers.is_empty());
        assert!(engine.filters.is_empty());
    }

    #[test]
    fn wire_is_optional() {
        let engine = complete_builder().build().expect("build ok");
        assert!(engine.wire.is_none());
    }

    // ── detect() orchestration (read-side hot path) ───────────────────

    struct StubFlow {
        host: String,
        provider: String,
        headers: HeaderMap,
        body: Bytes,
        session: Session,
    }
    impl FlowResolver for StubFlow {
        fn host(&self) -> &str {
            &self.host
        }
        fn provider(&self) -> Option<&str> {
            Some(&self.provider)
        }
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|v| v.to_str().ok())
        }
        fn request_headers(&self) -> &HeaderMap {
            &self.headers
        }
        fn response_headers(&self) -> Option<&HeaderMap> {
            None
        }
        fn request_body(&self) -> &Bytes {
            &self.body
        }
        fn response_body(&self) -> Option<&Bytes> {
            None
        }
        fn session(&self) -> &Session {
            &self.session
        }
    }

    fn flow(host: &str, provider: &str) -> StubFlow {
        let id = SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id();
        StubFlow {
            host: host.into(),
            provider: provider.into(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            session: Session::new(id),
        }
    }

    struct ConstHint {
        name: &'static str,
        category: &'static str,
        value: &'static str,
        confidence: f32,
    }
    impl Detector for ConstHint {
        fn name(&self) -> &'static str {
            self.name
        }
        fn detect(&self, _flow: &dyn FlowResolver, hints: &mut dyn HintWriter) {
            hints.write(ContextHint {
                category: self.category.into(),
                value: self.value.into(),
                confidence: self.confidence,
                source: self.name.into(),
            });
        }
    }

    #[test]
    fn detect_runs_common_group_only_when_no_provider_match() {
        let engine = complete_builder()
            .common_detector(Arc::new(ConstHint {
                name: "common_d",
                category: "tool",
                value: "Common",
                confidence: 0.5,
            }))
            .detector(
                "anthropic",
                Arc::new(ConstHint {
                    name: "anth_d",
                    category: "tool",
                    value: "Anthropic",
                    confidence: 0.9,
                }),
            )
            .build()
            .expect("build ok");

        let resolved = engine.detect(&flow("api.openai.com", "openai"));
        // openai group has no detectors; only common ran.
        assert_eq!(resolved.get("tool"), Some("Common"));
    }

    #[test]
    fn detect_runs_common_then_provider_specific() {
        let engine = complete_builder()
            .common_detector(Arc::new(ConstHint {
                name: "common_d",
                category: "tool",
                value: "Common",
                confidence: 0.5,
            }))
            .detector(
                "anthropic",
                Arc::new(ConstHint {
                    name: "anth_d",
                    category: "tool",
                    value: "Anthropic",
                    confidence: 0.9,
                }),
            )
            .build()
            .expect("build ok");

        let resolved = engine.detect(&flow("api.anthropic.com", "anthropic"));
        // anthropic detector is higher confidence — it wins.
        assert_eq!(resolved.get("tool"), Some("Anthropic"));
    }

    #[test]
    fn detect_with_no_detectors_returns_empty_resolved() {
        let engine = complete_builder().build().expect("build ok");
        let resolved = engine.detect(&flow("api.openai.com", "openai"));
        assert!(resolved.is_empty());
    }
}
