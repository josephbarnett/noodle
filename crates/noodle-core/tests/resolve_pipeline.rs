//! Functional test: multiple `Detector` impls + `resolve()` working
//! together against a stub `FlowResolver`. Proves the read-side of
//! the attribution pipeline before any provider/policy/codec lands.
//!
//! This sits one tier above unit tests (algorithm correctness lives in
//! `noodle-core/src/resolver.rs::tests`) and one tier below the e2e
//! proxy tests (which need rama and a real network). It exercises
//! cross-module behavior with no I/O.

use std::collections::HashMap;

use bytes::Bytes;
use http::HeaderMap;
use noodle_core::{
    CategoryConfig, CategoryDef, ContextHint, Detector, FlowResolver, HintWriter, Session,
    SessionKey, VecHintWriter, resolve,
};

// ───── Test fixtures ────────────────────────────────────────────────

struct StubFlow {
    headers: HeaderMap,
    host: String,
    provider: String,
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
            auth_header: b"test-auth",
            session_header: b"test-session",
        }
        .id();
        Self {
            headers,
            host: host.into(),
            provider: provider.into(),
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
        static EMPTY: Bytes = Bytes::new();
        &EMPTY
    }
    fn response_body(&self) -> Option<&Bytes> {
        None
    }
    fn session(&self) -> &Session {
        &self.session
    }
}

// ───── Two simple detector impls ────────────────────────────────────

/// Maps `User-Agent` prefixes to a tool name (no config loading).
struct UserAgentDetector {
    patterns: Vec<(&'static str, &'static str)>,
}

impl Detector for UserAgentDetector {
    fn name(&self) -> &'static str {
        "user_agent"
    }
    fn detect(&self, flow: &dyn FlowResolver, hints: &mut dyn HintWriter) {
        let Some(ua) = flow.header("user-agent") else {
            return;
        };
        for (pat, tool) in &self.patterns {
            if ua.starts_with(pat) {
                hints.write(ContextHint {
                    category: "tool".into(),
                    value: (*tool).into(),
                    confidence: 0.95,
                    source: "user_agent".into(),
                });
                return;
            }
        }
    }
}

/// Stamps a fixed team based on a custom header.
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

// ───── Helpers ──────────────────────────────────────────────────────

fn run_detectors(detectors: &[&dyn Detector], flow: &dyn FlowResolver) -> Vec<ContextHint> {
    let mut writer = VecHintWriter::new();
    for d in detectors {
        d.detect(flow, &mut writer);
    }
    writer.into_hints()
}

fn make_config() -> CategoryConfig {
    CategoryConfig {
        categories: HashMap::from([
            (
                "tool".into(),
                CategoryDef {
                    values: vec!["Claude Code".into(), "Cursor".into()],
                    detectors: vec!["user_agent".into()],
                    default: Some("unknown".into()),
                },
            ),
            (
                "team".into(),
                CategoryDef {
                    values: vec![],
                    detectors: vec!["team_header".into()],
                    default: Some("platform".into()),
                },
            ),
        ]),
    }
}

// ───── Tests ────────────────────────────────────────────────────────

#[test]
fn full_pipeline_resolves_known_tool_and_team() {
    let flow = StubFlow::new(
        "api.openai.com",
        "openai",
        &[("user-agent", "claude-cli/2.0.1"), ("x-team", "Cirrus")],
    );
    let ua = UserAgentDetector {
        patterns: vec![("claude-cli/", "Claude Code"), ("cursor/", "Cursor")],
    };
    let team = TeamHeaderDetector;

    let hints = run_detectors(&[&ua, &team], &flow);
    let resolved = resolve(&hints, &make_config());

    assert_eq!(resolved.get("tool"), Some("Claude Code"));
    assert_eq!(resolved.get("team"), Some("Cirrus"));
    assert_eq!(resolved.len(), 2);
}

#[test]
fn full_pipeline_falls_back_to_defaults_when_nothing_matches() {
    let flow = StubFlow::new("api.openai.com", "openai", &[("user-agent", "vim/9.0")]);
    let ua = UserAgentDetector {
        patterns: vec![("claude-cli/", "Claude Code")],
    };
    let team = TeamHeaderDetector;

    let hints = run_detectors(&[&ua, &team], &flow);
    let resolved = resolve(&hints, &make_config());

    // tool: detector fired no hint AND values: closed-list — default applies.
    // Actually no — UA didn't match, so no hint at all. Default applies.
    assert_eq!(resolved.get("tool"), Some("unknown"));
    // team: no x-team header, no hint, default applies.
    assert_eq!(resolved.get("team"), Some("platform"));
}

#[test]
fn full_pipeline_drops_value_outside_allow_list_then_defaults() {
    // UA recognized but the resulting tool name isn't in the closed
    // allow-list — the hint loses to the default.
    let flow = StubFlow::new(
        "api.openai.com",
        "openai",
        &[("user-agent", "weird-tool/1.0")],
    );
    let ua = UserAgentDetector {
        patterns: vec![("weird-tool/", "WeirdTool")],
    };

    let hints = run_detectors(&[&ua], &flow);
    let resolved = resolve(&hints, &make_config());

    assert_eq!(resolved.get("tool"), Some("unknown"));
}

#[test]
fn full_pipeline_open_list_accepts_any_value() {
    // `team` category has empty values: → open list, accept verbatim.
    let flow = StubFlow::new("api.openai.com", "openai", &[("x-team", "Atlas")]);
    let team = TeamHeaderDetector;

    let hints = run_detectors(&[&team], &flow);
    let resolved = resolve(&hints, &make_config());

    assert_eq!(resolved.get("team"), Some("Atlas"));
}

#[test]
fn full_pipeline_canonicalizes_case() {
    let flow = StubFlow::new(
        "api.openai.com",
        "openai",
        &[("user-agent", "claude-cli/2.0")],
    );
    // Detector emits the canonical form already, but verify the
    // resolver still canonicalizes (case-insensitive match).
    let ua = UserAgentDetector {
        patterns: vec![("claude-cli/", "claude code")], // lower-case
    };

    let hints = run_detectors(&[&ua], &flow);
    let resolved = resolve(&hints, &make_config());

    assert_eq!(resolved.get("tool"), Some("Claude Code"));
}

#[test]
fn full_pipeline_handles_empty_detector_list() {
    let flow = StubFlow::new("api.openai.com", "openai", &[]);
    let resolved = resolve(&[], &make_config());

    // Both categories have defaults — they fire.
    assert_eq!(resolved.get("tool"), Some("unknown"));
    assert_eq!(resolved.get("team"), Some("platform"));
    let _ = flow; // unused but documents intent
}
