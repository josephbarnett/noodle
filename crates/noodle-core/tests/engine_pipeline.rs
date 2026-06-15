#![allow(deprecated)]
// A.8.a: integration test exercises the legacy ProviderCodec path. Migration to layered tracked under A.8.b.

//! Functional test: full `InspectionEngine::detect` pipeline.
//!
//! Wires multiple detectors across `COMMON_GROUP` and a provider-specific
//! group, plus a real `CategoryConfig`, then asserts that
//! `engine.detect(flow)` produces the expected `Resolved` map for two
//! different flows. Pure cross-module behaviour; no I/O.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::HeaderMap;
use noodle_core::{
    AuditEvent, AuditSink, CategoryConfig, CategoryDef, CodecRegistry, ContextHint, Detector,
    FlowResolver, HintWriter, InspectionEngine, ProviderCodec, RequestProbe, Session, SessionId,
    SessionKey, SessionStore,
};

// ── Stubs for required ports ────────────────────────────────────────

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

// ── Stub flow ───────────────────────────────────────────────────────

struct StubFlow {
    host: String,
    provider: String,
    headers: HeaderMap,
    body: Bytes,
    session: Session,
}

impl StubFlow {
    fn new(host: &str, provider: &str, header_pairs: &[(&str, &str)]) -> Self {
        let mut headers = HeaderMap::new();
        for (n, v) in header_pairs {
            headers.insert(
                http::HeaderName::from_bytes(n.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        let id = SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id();
        Self {
            host: host.into(),
            provider: provider.into(),
            headers,
            body: Bytes::new(),
            session: Session::new(id),
        }
    }
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

// ── Realistic detectors ─────────────────────────────────────────────

struct UserAgentDetector;
impl Detector for UserAgentDetector {
    fn name(&self) -> &'static str {
        "user_agent"
    }
    fn detect(&self, flow: &dyn FlowResolver, hints: &mut dyn HintWriter) {
        let Some(ua) = flow.header("user-agent") else {
            return;
        };
        let tool = if ua.starts_with("claude-cli/") {
            "Claude Code"
        } else if ua.starts_with("cursor/") {
            "Cursor"
        } else {
            return;
        };
        hints.write(ContextHint {
            category: "tool".into(),
            value: tool.into(),
            confidence: 0.95,
            source: "user_agent".into(),
        });
    }
}

/// Anthropic-specific detector — only registered for the "anthropic"
/// group. Stamps a higher-confidence `tool=Claude Code` from any
/// Anthropic flow regardless of UA, overriding lower-confidence
/// UA-based heuristics.
struct AnthropicAuthoritativeTool;
impl Detector for AnthropicAuthoritativeTool {
    fn name(&self) -> &'static str {
        "anthropic_authoritative_tool"
    }
    fn detect(&self, _flow: &dyn FlowResolver, hints: &mut dyn HintWriter) {
        hints.write(ContextHint {
            category: "tool".into(),
            value: "Claude Code".into(),
            confidence: 0.99,
            source: "anthropic_authoritative_tool".into(),
        });
    }
}

struct TeamHeaderDetector;
impl Detector for TeamHeaderDetector {
    fn name(&self) -> &'static str {
        "team_header"
    }
    fn detect(&self, flow: &dyn FlowResolver, hints: &mut dyn HintWriter) {
        if let Some(team) = flow.header("x-team") {
            hints.write(ContextHint {
                category: "team".into(),
                value: team.into(),
                confidence: 0.7,
                source: "team_header".into(),
            });
        }
    }
}

// ── Fixture builders ────────────────────────────────────────────────

fn categories() -> CategoryConfig {
    CategoryConfig {
        categories: HashMap::from([
            (
                "tool".into(),
                CategoryDef {
                    values: vec!["Claude Code".into(), "Cursor".into()],
                    detectors: vec![],
                    default: Some("unknown".into()),
                },
            ),
            (
                "team".into(),
                CategoryDef {
                    values: vec![],
                    detectors: vec![],
                    default: Some("platform".into()),
                },
            ),
        ]),
    }
}

fn engine() -> InspectionEngine {
    InspectionEngine::builder()
        .codecs(Arc::new(StubCodecs))
        .categories(Arc::new(categories()))
        .sessions(Arc::new(StubStore))
        .audit(Arc::new(StubAudit))
        .common_detector(Arc::new(UserAgentDetector))
        .common_detector(Arc::new(TeamHeaderDetector))
        .detector("anthropic", Arc::new(AnthropicAuthoritativeTool))
        .build()
        .expect("build engine")
}

// ── Tests ───────────────────────────────────────────────────────────

#[test]
fn openai_flow_uses_only_common_detectors() {
    let engine = engine();
    let flow = StubFlow::new(
        "api.openai.com",
        "openai",
        &[("user-agent", "claude-cli/2.1"), ("x-team", "Cirrus")],
    );

    let resolved = engine.detect(&flow);

    // No openai-specific detector ran; common UA detector emitted
    // tool=Claude Code (allow-list canonicalizes); team_header emitted
    // team=Cirrus.
    assert_eq!(resolved.get("tool"), Some("Claude Code"));
    assert_eq!(resolved.get("team"), Some("Cirrus"));
}

#[test]
fn anthropic_flow_overrides_with_authoritative_detector() {
    let engine = engine();
    let flow = StubFlow::new(
        "api.anthropic.com",
        "anthropic",
        &[("user-agent", "vim-plugin/9.0"), ("x-team", "Cirrus")],
    );

    let resolved = engine.detect(&flow);

    // UA didn't match a known tool, so common emits no tool hint.
    // Anthropic-group detector emits tool=Claude Code at 0.99.
    assert_eq!(resolved.get("tool"), Some("Claude Code"));
    assert_eq!(resolved.get("team"), Some("Cirrus"));
}

#[test]
fn anthropic_higher_confidence_beats_common_lower_confidence() {
    // UA matches Cursor at 0.95 (common). Anthropic detector emits
    // Claude Code at 0.99 (specific). Specific should win.
    let engine = engine();
    let flow = StubFlow::new(
        "api.anthropic.com",
        "anthropic",
        &[("user-agent", "cursor/1.0")],
    );
    let resolved = engine.detect(&flow);
    assert_eq!(resolved.get("tool"), Some("Claude Code"));
}

#[test]
fn no_hints_falls_back_to_defaults() {
    let engine = engine();
    let flow = StubFlow::new("api.openai.com", "openai", &[]);
    let resolved = engine.detect(&flow);
    assert_eq!(resolved.get("tool"), Some("unknown"));
    assert_eq!(resolved.get("team"), Some("platform"));
}

#[test]
fn provider_with_no_registered_group_runs_only_common() {
    let engine = engine();
    let flow = StubFlow::new(
        "api.bedrock.amazonaws.com",
        "bedrock",
        &[("user-agent", "claude-cli/2.0")],
    );
    let resolved = engine.detect(&flow);
    // No bedrock group exists; common UA detector still runs.
    assert_eq!(resolved.get("tool"), Some("Claude Code"));
}
