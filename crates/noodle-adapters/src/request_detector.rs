//! Layered `RequestDetector` adapters (ADR 021).
//!
//! Header-level read-only inspection of an incoming request at
//! flow-open time. Each detector here implements
//! [`noodle_core::layered::RequestDetector`] and emits zero or
//! more [`Hint`][noodle_core::layered::Hint]s (or other side
//! effects) into the engine's per-flow side channel.
//!
//! Currently shipped:
//!
//! - [`UserAgentDetector`] — substring match over a small
//!   known-tool table; emits a `tool` [`Hint`] with a
//!   per-needle confidence. Replaces the v1 inline
//!   `user_agent_hint` function that previously lived in
//!   `noodle-proxy::wirelog`.
//!
//! Future detectors (auth-header shape, custom `X-Tool-*`
//! headers, etc.) drop in alongside without changing the trait
//! surface — engine wiring is identical for any new
//! `RequestDetector`.

use noodle_core::layered::{CodecProbe, Hint, RequestDetector, SideChannelTx};
use smol_str::SmolStr;

/// Maps the `User-Agent` HTTP header to a `tool` [`Hint`] using
/// a small substring table. Replaces the v1 inline
/// `user_agent_hint` function that previously lived in
/// `noodle-proxy::wirelog` — this is the proper layered
/// `RequestDetector` form per ADR 021.
///
/// The substring table is intentionally tiny and substring-
/// based; this is a heuristic detector, not a parser. When a
/// real production deployment needs richer matching (regex,
/// version-aware, exact-prefix-only) the table moves into
/// config via the configurable-detector story (related: story
/// 034 for marker / enhancement-prompt config).
///
/// Confidence values per ADR 004's ranking. Substring `contains`
/// over a list, first-match-wins; needle order matters.
#[derive(Clone, Debug)]
pub struct UserAgentDetector {
    table: &'static [(&'static str, &'static str, f32)],
}

impl UserAgentDetector {
    pub const NAME: &'static str = "user_agent";

    /// The hint `source` value emitted by this detector. Stable
    /// for downstream consumers that key on it.
    pub const SOURCE: &'static str = "user_agent";

    /// Default table covering the agents we've validated against
    /// in `captures/`. Ordering is meaningful — `Claude-Code`
    /// must precede the broader `claude-cli` needle so the more
    /// specific match wins.
    ///
    /// `(needle, canonical_value, confidence)`.
    pub const DEFAULT_TABLE: &'static [(&'static str, &'static str, f32)] = &[
        ("Claude-Code", "Claude Code", 0.95),
        ("claude-cli", "Claude Code", 0.90),
        ("Claude-Desktop", "Claude Desktop", 0.95),
        ("Cursor", "Cursor", 0.90),
        ("OpenCode", "OpenCode", 0.90),
    ];

    /// Construct with the default table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            table: Self::DEFAULT_TABLE,
        }
    }

    /// Construct with a caller-supplied substring table. Useful
    /// for tests and (later) for config-driven deployments.
    #[must_use]
    pub const fn with_table(table: &'static [(&'static str, &'static str, f32)]) -> Self {
        Self { table }
    }
}

impl Default for UserAgentDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestDetector for UserAgentDetector {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn detect(&self, probe: &CodecProbe<'_>, side: &mut SideChannelTx<'_>) {
        // Pull the UA header. Missing or non-UTF-8 → silent
        // skip per the ADR 021 empty-on-error posture.
        let Some(ua_value) = probe.request_headers.get(http::header::USER_AGENT) else {
            return;
        };
        let Ok(ua) = ua_value.to_str() else {
            return;
        };

        // First-match-wins substring scan. Confidence and
        // canonical value come from the table entry.
        for (needle, canonical, confidence) in self.table {
            if ua.contains(needle) {
                side.emit_hint(Hint {
                    category: SmolStr::new_static("tool"),
                    value: SmolStr::new(*canonical),
                    confidence: *confidence,
                    source: SmolStr::new_static(Self::SOURCE),
                    correlation: None,
                });
                return;
            }
        }
        // No match → no emission. The Resolver treats absence as
        // "no opinion from this detector" — exactly what we want.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method};
    use noodle_core::layered::{CodecProbe, SideEffect};

    fn probe_with_ua(ua: &'static str) -> (HeaderMap, Method) {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::USER_AGENT, HeaderValue::from_static(ua));
        (headers, Method::POST)
    }

    fn make_probe<'a>(headers: &'a HeaderMap, method: &'a Method) -> CodecProbe<'a> {
        CodecProbe {
            host: "api.anthropic.com",
            path: "/v1/messages",
            method,
            request_headers: headers,
            response_status: None,
            response_content_type: None,
        }
    }

    fn run_detector(headers: &HeaderMap, method: &Method) -> Vec<SideEffect> {
        let probe = make_probe(headers, method);
        let mut buf = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);
        UserAgentDetector::new().detect(&probe, &mut side);
        buf
    }

    fn extract_hint(effects: &[SideEffect]) -> &Hint {
        match effects.first().expect("expected one hint") {
            SideEffect::Hint(h) => h,
            other => panic!("expected Hint, got {other:?}"),
        }
    }

    #[test]
    fn detector_is_object_safe() {
        let _b: Box<dyn RequestDetector> = Box::new(UserAgentDetector::new());
    }

    #[test]
    fn claude_code_full_ua_emits_canonical_hint() {
        let (h, m) = probe_with_ua("claude-cli/2.1.143 (external, cli) Claude-Code/2.1.143");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.category.as_str(), "tool");
        assert_eq!(hint.value.as_str(), "Claude Code");
        assert!((hint.confidence - 0.95).abs() < 1e-6);
        assert_eq!(hint.source.as_str(), UserAgentDetector::SOURCE);
    }

    #[test]
    fn claude_cli_only_ua_falls_back_to_lower_confidence() {
        let (h, m) = probe_with_ua("claude-cli/0.42 (linux)");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.value.as_str(), "Claude Code");
        assert!((hint.confidence - 0.90).abs() < 1e-6);
    }

    #[test]
    fn cursor_ua_maps_to_cursor() {
        let (h, m) = probe_with_ua("Cursor/0.42");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.value.as_str(), "Cursor");
        assert!((hint.confidence - 0.90).abs() < 1e-6);
    }

    #[test]
    fn opencode_ua_maps_to_opencode() {
        let (h, m) = probe_with_ua("OpenCode/0.42");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.value.as_str(), "OpenCode");
    }

    #[test]
    fn claude_desktop_ua_maps_to_claude_desktop() {
        let (h, m) = probe_with_ua("Claude-Desktop/1.5.0 (electron) anthropic");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.value.as_str(), "Claude Desktop");
    }

    #[test]
    fn unknown_ua_emits_nothing() {
        let (h, m) = probe_with_ua("curl/8.4.0");
        assert!(run_detector(&h, &m).is_empty());
        let (h, m) = probe_with_ua("python-requests/2.32");
        assert!(run_detector(&h, &m).is_empty());
    }

    #[test]
    fn missing_ua_header_emits_nothing() {
        let headers = HeaderMap::new();
        let method = Method::POST;
        assert!(run_detector(&headers, &method).is_empty());
    }

    #[test]
    fn first_match_wins_when_ua_contains_multiple_needles() {
        // Belt-and-braces: a string with both "Claude-Code" and
        // "Cursor" should match the earlier-listed needle.
        let (h, m) = probe_with_ua("Claude-Code/1.0 (also Cursor)");
        let out = run_detector(&h, &m);
        let hint = extract_hint(&out);
        assert_eq!(hint.value.as_str(), "Claude Code");
    }

    #[test]
    fn custom_table_overrides_defaults() {
        const CUSTOM: &[(&str, &str, f32)] = &[("MyTool", "My Tool", 0.99)];
        let detector = UserAgentDetector::with_table(CUSTOM);
        let (h, m) = probe_with_ua("MyTool/1.0");
        let mut buf = Vec::new();
        let probe = make_probe(&h, &m);
        {
            let mut side = SideChannelTx::new(&mut buf, 0, 0);
            detector.detect(&probe, &mut side);
        }
        let hint = extract_hint(&buf);
        assert_eq!(hint.value.as_str(), "My Tool");
        assert!((hint.confidence - 0.99).abs() < 1e-6);
    }
}
