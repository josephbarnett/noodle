//! `EndpointMatcher` — the declarative `(address, endpoint)`
//! classifier of ADR 019's dispatch key.
//!
//! Codec/capability selection MUST key on host **and** path
//! **and** (optionally) method / content negotiation — never
//! host alone. The mitm captures proved host-only matching is
//! wrong both ways: it *misses* the model traffic
//! (`claude.ai/api/organizations/{id}/chat_conversations/{id}/completion`,
//! whose host is not `*.anthropic.com`) and *false-positives* on
//! non-model `*.anthropic.com` hosts (`s-cdn.anthropic.com`
//! images, app-update paths, telemetry, MCP, i18n).
//!
//! The matcher is **string-only and owned** so the ADR 019
//! routing table can be expressed as data (config) the deploying
//! operator owns — it never carries executable code. All present
//! constraints must hold (AND); an absent constraint matches
//! anything.

use http::Method;

use crate::layered::CodecProbe;

/// Host constraint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HostMatch {
    /// Matches any host.
    #[default]
    Any,
    /// Exact host (case-insensitive), e.g. `claude.ai`.
    Exact(String),
    /// Host suffix (case-insensitive), e.g. `.anthropic.com`.
    /// Use sparingly — only where the whole suffix is uniform.
    Suffix(String),
}

impl HostMatch {
    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(h) => host.eq_ignore_ascii_case(h),
            Self::Suffix(s) => {
                let h = host.to_ascii_lowercase();
                h.ends_with(&s.to_ascii_lowercase())
            }
        }
    }
}

/// Path constraint. All present sub-constraints must hold, so a
/// specific endpoint is expressible without a regex engine:
/// e.g. completion = `contains "/chat_conversations/"` +
/// `suffix "/completion"`, which excludes
/// `/api/organizations/{id}/sync/settings` and
/// `/api/organizations/{id}/mcp/v2/bootstrap`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PathMatch {
    pub prefix: Option<String>,
    pub contains: Option<String>,
    pub suffix: Option<String>,
}

impl PathMatch {
    fn matches(&self, path: &str) -> bool {
        let p = path.split(['?', '#']).next().unwrap_or(path);
        self.prefix
            .as_ref()
            .is_none_or(|x| p.starts_with(x.as_str()))
            && self
                .contains
                .as_ref()
                .is_none_or(|x| p.contains(x.as_str()))
            && self.suffix.as_ref().is_none_or(|x| p.ends_with(x.as_str()))
    }
}

/// One cell's `(address, endpoint)` predicate over a
/// [`CodecProbe`]. Direction (ADR 019's 4-way axis) is selected
/// by the engine, not this matcher.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EndpointMatcher {
    pub host: HostMatch,
    pub path: PathMatch,
    /// `None` matches any method.
    pub method: Option<Method>,
    /// Substring matched against the response content-type when
    /// present (response side), else the request `accept` header
    /// (request side). `None` matches anything.
    pub content_type: Option<String>,
}

impl EndpointMatcher {
    #[must_use]
    pub fn new(host: HostMatch, path: PathMatch) -> Self {
        Self {
            host,
            path,
            method: None,
            content_type: None,
        }
    }

    #[must_use]
    pub fn with_method(mut self, m: Method) -> Self {
        self.method = Some(m);
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, ct: impl Into<String>) -> Self {
        self.content_type = Some(ct.into());
        self
    }

    /// Whether this cell predicate accepts the probe. All present
    /// constraints must hold.
    #[must_use]
    pub fn matches(&self, probe: &CodecProbe<'_>) -> bool {
        if !self.host.matches(probe.host) {
            return false;
        }
        if !self.path.matches(probe.path) {
            return false;
        }
        if let Some(m) = &self.method
            && probe.method != m
        {
            return false;
        }
        if let Some(want) = &self.content_type {
            let observed = probe.response_content_type.or_else(|| {
                probe
                    .request_headers
                    .get(http::header::ACCEPT)
                    .and_then(|v| v.to_str().ok())
            });
            if !observed.is_some_and(|ct| ct.contains(want.as_str())) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, StatusCode};

    fn probe<'a>(
        host: &'a str,
        path: &'a str,
        method: &'a Method,
        headers: &'a HeaderMap,
        rsp_ct: Option<&'a str>,
    ) -> CodecProbe<'a> {
        CodecProbe {
            host,
            path,
            method,
            request_headers: headers,
            response_status: rsp_ct.map(|_| StatusCode::OK),
            response_content_type: rsp_ct,
        }
    }

    /// The claude.ai chat-completion cell (from the capture):
    /// host `claude.ai` + path contains `chat_conversations` +
    /// ends `/completion`. Must accept the real completion path
    /// and reject every other claude.ai endpoint family observed
    /// in the same capture.
    fn completion_matcher() -> EndpointMatcher {
        EndpointMatcher::new(
            HostMatch::Exact("claude.ai".into()),
            PathMatch {
                prefix: Some("/api/organizations/".into()),
                contains: Some("/chat_conversations/".into()),
                suffix: Some("/completion".into()),
            },
        )
        .with_method(Method::POST)
    }

    #[test]
    fn completion_path_matches() {
        let h = HeaderMap::new();
        let m = Method::POST;
        let p = probe(
            "claude.ai",
            "/api/organizations/e6a79da4-c6bc-444d-b1bf-3c0c5d2b551b/chat_conversations/b1860ba0-e414-4a57-9d5f-2a40ea0add24/completion",
            &m,
            &h,
            Some("text/event-stream; charset=utf-8"),
        );
        assert!(completion_matcher().matches(&p));
    }

    #[test]
    fn claude_ai_non_model_endpoints_do_not_match() {
        // Every one of these is a real path from the chat
        // capture that host-only matching would wrongly grab.
        let h = HeaderMap::new();
        let m = Method::GET;
        let post = Method::POST;
        let cm = completion_matcher();
        for (host, path, meth) in [
            ("claude.ai", "/api/organizations/e6a79da4/sync/settings", &m),
            (
                "claude.ai",
                "/api/organizations/e6a79da4/mcp/v2/bootstrap",
                &m,
            ),
            ("claude.ai", "/api/organizations/e6a79da4/list_styles", &m),
            ("claude.ai", "/i18n/en-US.json", &m),
            ("claude.ai", "/api/event_logging/v2/batch", &post),
            (
                "claude.ai",
                "/api/organizations/e6a79da4/chat_conversations/abc/title",
                &post,
            ),
            ("s-cdn.anthropic.com", "/images/181554.gif", &m),
            (
                "api.anthropic.com",
                "/api/desktop/darwin/universal/squirrel/update",
                &m,
            ),
        ] {
            let p = probe(host, path, meth, &h, Some("application/json"));
            assert!(
                !cm.matches(&p),
                "must NOT match non-model endpoint: {host}{path}",
            );
        }
    }

    #[test]
    fn host_suffix_and_exact_semantics() {
        assert!(HostMatch::Exact("claude.ai".into()).matches("Claude.AI"));
        assert!(!HostMatch::Exact("claude.ai".into()).matches("api.claude.ai"));
        assert!(HostMatch::Suffix(".anthropic.com".into()).matches("s-cdn.anthropic.com"));
        assert!(HostMatch::Any.matches("anything.example"));
    }

    #[test]
    fn method_constraint_rejects_wrong_verb() {
        let h = HeaderMap::new();
        let get = Method::GET;
        let p = probe(
            "claude.ai",
            "/api/organizations/x/chat_conversations/y/completion",
            &get,
            &h,
            Some("text/event-stream"),
        );
        assert!(
            !completion_matcher().matches(&p),
            "POST-only cell rejects GET"
        );
    }

    #[test]
    fn content_type_falls_back_to_request_accept() {
        let mut h = HeaderMap::new();
        h.insert(http::header::ACCEPT, "text/event-stream".parse().unwrap());
        let m = Method::POST;
        // Request side: no response_content_type yet → use accept.
        let p = probe("claude.ai", "/x/completion", &m, &h, None);
        let matcher = EndpointMatcher::new(
            HostMatch::Exact("claude.ai".into()),
            PathMatch {
                prefix: None,
                contains: None,
                suffix: Some("/completion".into()),
            },
        )
        .with_content_type("event-stream");
        assert!(matcher.matches(&p));
    }
}
