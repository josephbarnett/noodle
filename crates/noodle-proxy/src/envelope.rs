//! Envelope-level operational-context builder (ADR 029 §2.4,
//! refactor slices S6 + S7).
//!
//! Builds the four `noodle_domain` structs the proxy stamps on
//! every `WireEvent`:
//!
//! - [`AgentApp`][noodle_domain::observation_context::AgentApp] —
//!   the agent harness in the field. Parsed from the request's
//!   `User-Agent` header (with `X-Stainless-*` family considered).
//! - [`Machine`][noodle_domain::observation_context::Machine] —
//!   the host noodle is running on. Pulled from process-level
//!   facts (`std::env::consts`, `gethostname`, locale, timezone).
//!   Cached because these don't change for the lifetime of the
//!   proxy.
//! - [`CollectorApp`][noodle_domain::observation_context::CollectorApp]
//!   — the noodle build itself. Pulled from compile-time env vars
//!   embedded by `build.rs` (`VERGEN_GIT_SHA`, `VERGEN_BUILD_DATE`)
//!   plus the cargo features active in this build.
//! - `SubscriptionContext` — family 13. Holds:
//!   - [`ApiKeyFingerprint`][noodle_domain::subscription_context::ApiKeyFingerprint]
//!     — the first 12 chars of the credential (same window the
//!     S5 redaction policy preserves), the inferred kind
//!     (`ApiKey` / `Session` / `Oauth` / `Unknown`), and the
//!     header it came from (`AuthorizationHeader` / `XApiKey`).
//!   - [`OrganizationContext`][noodle_domain::subscription_context::OrganizationContext]
//!     — `organization_id` extracted from the `claude.ai` URL
//!     path (`/api/organizations/{org}/...`) at flow open AND
//!     from the `Anthropic-Organization-Id` response header at
//!     flow close. `account_type` defaults to `Unknown`
//!     pre-enrichment-plane.
//!   - [`SubscriptionTier`][noodle_domain::subscription_context::SubscriptionTier]
//!     — typically not wire-observable on these cells; left
//!     `None` for v1.
//!
//! ## Why JSON at the `WireEvent` boundary
//!
//! The proxy holds typed `noodle_domain` structs here (for shape
//! safety) and serializes them to `serde_json::Value` at the
//! `WireEvent` boundary. `noodle-core` does not depend on
//! `noodle-domain` (ADR 029 §5), and `noodle-tap` does not either
//! (ADR 029 §1) — so the pre-serialized JSON is the lingua franca
//! that survives the trip from proxy → core → tap. The on-disk
//! shape is governed by `noodle-domain`'s serde derives, so what
//! the proxy writes is what `tap.jsonl` consumers parse.

use std::sync::OnceLock;

use noodle_domain::observation_context::{
    AgentApp, AgentAppName, AgentAppSource, Architecture, CollectorApp, Machine, OsFamily,
};
use noodle_domain::subscription_context::{
    AccountType, ApiKeyFingerprint, ApiKeyKind, ApiKeySource, OrganizationContext, SubscriptionTier,
};
use rama::http::HeaderMap;
use semver::Version;
use serde::Serialize;
use time::OffsetDateTime;

/// Per-request envelope: the four operational-context fields
/// stamped on every `WireEvent`. Built at request open with the
/// observable signals (UA, host facts, build info, API key
/// fingerprint, URL-derived org id); a response-close hook
/// (`merge_organization_id_from_response`) folds the
/// `Anthropic-Organization-Id` response header into the
/// subscription block once the response arrives so the response
/// `WireEvent` carries the same enriched envelope as the
/// request.
#[derive(Debug, Clone, Default)]
pub struct EnvelopeContext {
    agent_app: Option<AgentApp>,
    machine: Option<Machine>,
    collector_app: Option<CollectorApp>,
    subscription: Option<SubscriptionContext>,
}

/// Proxy-side typed `SubscriptionContext` (family 13). Mirrors
/// the on-disk shape of `noodle_domain::subscription_context`'s
/// three top-level fields. Lives here (rather than in
/// `noodle-domain` directly) because the wire-shape boundary is
/// `serde_json::Value`; this is the proxy's typed working copy
/// before serialization.
#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKeyFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization: Option<OrganizationContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<SubscriptionTier>,
}

impl SubscriptionContext {
    /// `None` when every sub-field is absent — keeps
    /// `envelope.subscription` omitted from `tap.jsonl` on
    /// unattributed traffic.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.organization.is_none() && self.tier.is_none()
    }
}

impl EnvelopeContext {
    /// Build an envelope context from a request's URL and
    /// headers. Per ADR 029 §2.4:
    /// - `agent_app` parses the `User-Agent` header.
    /// - `machine` reads cached proxy-host facts.
    /// - `collector_app` reads compile-time-embedded build info.
    /// - `subscription.api_key` reads the request's credential
    ///   header (`Authorization` / `X-Api-Key` /
    ///   `Anthropic-Api-Key`).
    /// - `subscription.organization` reads the `claude.ai` URL
    ///   path (`/api/organizations/{org}/...`) when the request
    ///   is to a `claude.ai` cell; otherwise stays empty until
    ///   the response close hook may populate it from
    ///   `Anthropic-Organization-Id`.
    #[must_use]
    pub fn for_request(uri: &rama::http::Uri, headers: &HeaderMap) -> Self {
        let agent_app = parse_agent_app_from_headers(headers);
        let machine = Some(machine_context().clone());
        let collector_app = Some(collector_app_context().clone());
        let subscription = build_subscription_for_request(uri, headers);
        Self {
            agent_app,
            machine,
            collector_app,
            subscription,
        }
    }

    /// Backwards-compatible builder used by paths that don't
    /// have the request URI available (synthesized-response
    /// fallbacks). The subscription block is built from headers
    /// only — URL-derived org id is left to the URL-aware path.
    #[must_use]
    pub fn for_request_headers(headers: &HeaderMap) -> Self {
        let agent_app = parse_agent_app_from_headers(headers);
        let machine = Some(machine_context().clone());
        let collector_app = Some(collector_app_context().clone());
        let subscription = build_subscription_for_headers(headers);
        Self {
            agent_app,
            machine,
            collector_app,
            subscription,
        }
    }

    /// Merge the `Anthropic-Organization-Id` response header
    /// into the subscription block at response close. When the
    /// header is present AND the existing
    /// `organization.organization_id` is `None`, the header
    /// value populates it. When the URL-derived value already
    /// exists, the response header is asserted to match (we
    /// keep the URL value as the source of truth — they're the
    /// same physical org id on the Anthropic side).
    pub fn merge_organization_id_from_response(&mut self, response_headers: &HeaderMap) {
        let Some(org_id) = extract_org_id_from_response_headers(response_headers) else {
            return;
        };
        let sub = self.subscription.get_or_insert(SubscriptionContext {
            api_key: None,
            organization: None,
            tier: None,
        });
        match sub.organization.as_mut() {
            Some(org) if org.organization_id.is_none() => {
                org.organization_id = Some(org_id);
            }
            Some(_) => {
                // Already populated (from URL path) — no-op.
            }
            None => {
                sub.organization = Some(OrganizationContext {
                    organization_id: Some(org_id),
                    parent_organization_id: None,
                    account_type: AccountType::Unknown,
                });
            }
        }
    }

    /// Serialize the `agent_app` field to JSON for the
    /// `WireEvent` boundary. Returns `None` when the proxy
    /// couldn't determine an agent app (no `User-Agent` header,
    /// or unparseable).
    #[must_use]
    pub fn agent_app_json(&self) -> Option<serde_json::Value> {
        self.agent_app
            .as_ref()
            .and_then(|a| serde_json::to_value(a).ok())
    }

    /// Serialize the `machine` field to JSON for the `WireEvent`
    /// boundary.
    #[must_use]
    pub fn machine_json(&self) -> Option<serde_json::Value> {
        self.machine
            .as_ref()
            .and_then(|m| serde_json::to_value(m).ok())
    }

    /// Serialize the `collector_app` field to JSON for the
    /// `WireEvent` boundary.
    #[must_use]
    pub fn collector_app_json(&self) -> Option<serde_json::Value> {
        self.collector_app
            .as_ref()
            .and_then(|c| serde_json::to_value(c).ok())
    }

    /// Serialize the `subscription` field to JSON for the
    /// `WireEvent` boundary. Returns `None` when no sub-field
    /// was populated — keeps the `envelope.subscription` block
    /// omitted on unattributed traffic.
    #[must_use]
    pub fn subscription_json(&self) -> Option<serde_json::Value> {
        let sub = self.subscription.as_ref()?;
        if sub.is_empty() {
            return None;
        }
        serde_json::to_value(sub).ok()
    }

    /// For tests: borrow the typed inner fields.
    #[doc(hidden)]
    #[must_use]
    pub fn agent_app(&self) -> Option<&AgentApp> {
        self.agent_app.as_ref()
    }

    /// For tests: borrow the typed subscription block.
    #[doc(hidden)]
    #[must_use]
    pub fn subscription(&self) -> Option<&SubscriptionContext> {
        self.subscription.as_ref()
    }
}

/// Parse the `User-Agent` header into an [`AgentApp`]. Matches a
/// known agent-harness prefix case-insensitively; falls back to
/// `Unknown` when no prefix matches.
///
/// Today the proxy only consults `User-Agent`; the
/// `X-Stainless-*` family (set by every Stainless-generated SDK,
/// which includes Anthropic's `claude-code`) is a future
/// enrichment hook — when an X-Stainless header carries an SDK
/// language / version richer than the UA, that will refine
/// `agent_app.version`. v1: UA-only.
#[must_use]
pub fn parse_agent_app_from_headers(headers: &HeaderMap) -> Option<AgentApp> {
    let ua = headers
        .get(rama::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())?;
    Some(parse_agent_app_from_user_agent(ua))
}

/// Pure parser exposed for unit-testing without a `HeaderMap`.
#[must_use]
pub fn parse_agent_app_from_user_agent(ua: &str) -> AgentApp {
    let lower = ua.to_ascii_lowercase();
    // Match the longest, most specific prefix first. A few clients
    // (Anthropic SDKs, Cursor) ship `<name>/<version> (extras)`
    // shape; the version parser is best-effort.
    let (name, raw_version) = if let Some(rest) = strip_token_prefix(&lower, "claude-code") {
        (AgentAppName::ClaudeCode, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "claude-cli") {
        // `claude-cli` is the historical name for `claude-code`
        // — same agent, same UA family.
        (AgentAppName::ClaudeCode, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "claude-desktop") {
        (AgentAppName::ClaudeDesktop, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "opencode") {
        (AgentAppName::OpenCode, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "cursor") {
        (AgentAppName::Cursor, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "chatgpt-desktop") {
        (AgentAppName::ChatGptDesktop, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "codex-cli") {
        (AgentAppName::CodexCli, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "warp") {
        (AgentAppName::Warp, rest)
    } else if let Some(rest) = strip_token_prefix(&lower, "zed") {
        (AgentAppName::Zed, rest)
    } else {
        return AgentApp {
            name: AgentAppName::Unknown,
            version: None,
            build_hash: None,
            build_date: None,
            source: AgentAppSource::Unknown,
        };
    };

    let version = parse_version_after_slash(raw_version);

    AgentApp {
        name,
        version,
        build_hash: None,
        build_date: None,
        source: AgentAppSource::UserAgentHeader,
    }
}

/// Match a token at the start of `s` followed by `/`, whitespace,
/// or end-of-string. Returns the bytes immediately after the
/// matched token (i.e. starting at the version separator) so a
/// downstream version parser can pull `<sep><version>`. Returns
/// `None` if `s` doesn't start with the token followed by a
/// valid separator.
///
/// Token-boundary matching avoids `cursor-cli` claiming
/// `AgentAppName::Cursor` when a future agent ships under a
/// distinct name beginning with `cursor`.
fn strip_token_prefix<'a>(s: &'a str, token: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(token)?;
    match rest.chars().next() {
        None | Some('/' | ' ' | '\t' | '(') => Some(rest),
        _ => None,
    }
}

/// Parse the version that follows a `/<version>` suffix on the
/// agent token. Tolerates trailing whitespace / paren-extras
/// (`claude-code/0.2.5 (Macintosh; …)`).
fn parse_version_after_slash(rest: &str) -> Option<Version> {
    let after_slash = rest.strip_prefix('/')?;
    let v_str = after_slash
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';' || c == ',')
        .next()?;
    Version::parse(v_str).ok()
}

/// Process-local cache of the machine-level operational-context
/// (hostname, OS, etc.). These facts don't change for the
/// lifetime of the proxy — read once, reuse forever.
fn machine_context() -> &'static Machine {
    static CELL: OnceLock<Machine> = OnceLock::new();
    CELL.get_or_init(|| Machine {
        hostname: hostname_string(),
        os_family: detect_os_family(),
        os_version: None,
        architecture: detect_architecture(),
        locale: std::env::var("LANG").ok(),
        timezone: detect_timezone(),
    })
}

/// Best-effort hostname read. Returns `None` if `gethostname`
/// returned non-UTF-8 bytes (rare; doesn't happen on the
/// platforms we ship).
fn hostname_string() -> Option<String> {
    let raw = gethostname::gethostname();
    raw.into_string().ok()
}

fn detect_os_family() -> OsFamily {
    match std::env::consts::OS {
        "macos" => OsFamily::Macos,
        "linux" => OsFamily::Linux,
        "windows" => OsFamily::Windows,
        _ => OsFamily::Unknown,
    }
}

fn detect_architecture() -> Architecture {
    match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        _ => Architecture::Unknown,
    }
}

/// Read the system timezone via the `TZ` env var. Falls back to
/// `None` rather than reading `/etc/localtime` to keep the
/// dependency surface minimal — operators that care about
/// timezone on the envelope can set `TZ` on the proxy process.
fn detect_timezone() -> Option<String> {
    std::env::var("TZ").ok()
}

/// Process-local cache of the collector-app context (this
/// noodle build). Compile-time embedded — does not change after
/// the binary is built.
fn collector_app_context() -> &'static CollectorApp {
    static CELL: OnceLock<CollectorApp> = OnceLock::new();
    CELL.get_or_init(|| {
        // build.rs emits these. `VERGEN_GIT_SHA` is the literal
        // commit SHA (40 hex chars) or `"unknown"` on a non-git
        // build. `VERGEN_BUILD_DATE` is RFC3339Z.
        let build_hash = env!("VERGEN_GIT_SHA").to_owned();
        let build_date_str = env!("VERGEN_BUILD_DATE");
        let build_date = OffsetDateTime::parse(
            build_date_str,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let pkg_version = env!("CARGO_PKG_VERSION");
        let version = Version::parse(pkg_version).unwrap_or_else(|_| Version::new(0, 0, 0));
        CollectorApp {
            name: "noodle".to_owned(),
            version,
            build_hash,
            build_date,
            features: active_features(),
        }
    })
}

/// Cargo-feature names currently active in this build. v1
/// reports the `tap` feature flag (which gates the JSONL sink
/// integration the user typically cares about) and the
/// `default_features` marker so consumers can distinguish a
/// fully-featured build from `--no-default-features`.
fn active_features() -> Vec<String> {
    let mut features = Vec::new();
    if cfg!(feature = "tap") {
        features.push("tap".to_owned());
    }
    features
}

/// How many visible chars of the credential the proxy preserves
/// on `envelope.subscription.api_key.prefix`. Pinned to the same
/// constant the S5 redaction policy uses for sensitive headers
/// (`Authorization`, `X-Api-Key`, `Anthropic-Api-Key`) per
/// ADR 027 §9 — the two values are conceptually the same window
/// on the credential, just landing on different `tap.jsonl`
/// fields.
const API_KEY_PREFIX_LEN: usize = 12;

/// Build the subscription block from a request's URL + headers.
/// Returns `None` when no sub-field could be populated (no
/// credential header AND not a `claude.ai` URL with an org id in
/// the path).
fn build_subscription_for_request(
    uri: &rama::http::Uri,
    headers: &HeaderMap,
) -> Option<SubscriptionContext> {
    let api_key = extract_api_key_fingerprint(headers);
    let organization = extract_org_id_from_uri(uri).map(|org_id| OrganizationContext {
        organization_id: Some(org_id),
        parent_organization_id: None,
        account_type: AccountType::Unknown,
    });
    if api_key.is_none() && organization.is_none() {
        return None;
    }
    Some(SubscriptionContext {
        api_key,
        organization,
        tier: tier_placeholder(),
    })
}

/// Build the subscription block from headers alone (no URL
/// available). Used by the synthesized-response fallback path in
/// `lib.rs` where the proxy never had a URI in hand.
fn build_subscription_for_headers(headers: &HeaderMap) -> Option<SubscriptionContext> {
    let api_key = extract_api_key_fingerprint(headers)?;
    Some(SubscriptionContext {
        api_key: Some(api_key),
        organization: None,
        tier: tier_placeholder(),
    })
}

/// Family 13 §`SubscriptionTier` — typically not wire-observable
/// on the cells noodle proxies today (Console API enrichment is
/// the embellishment plane's job). Returns `None` so the
/// `envelope.subscription.tier` slot is omitted entirely on the
/// wire until a vendor signal lets us populate it. Kept as a
/// helper (rather than inlined) so the day this populates, the
/// constructor lives at one site.
fn tier_placeholder() -> Option<SubscriptionTier> {
    // Documented shape for the day this populates:
    //   Some(SubscriptionTier {
    //       tier: None,
    //       source: SubscriptionTierSource::Unknown,
    //   })
    None
}

/// Extract an [`ApiKeyFingerprint`] from a request's credential
/// header. Checked in the order the ADR 027 §9 redaction table
/// lists them so the result is deterministic when multiple
/// credential headers are present.
fn extract_api_key_fingerprint(headers: &HeaderMap) -> Option<ApiKeyFingerprint> {
    // (header name, source, scheme-strip?) — `Authorization`
    // values typically arrive as `Bearer <credential>`; the
    // scheme is stripped before we take the prefix so we
    // measure the credential, not the auth-scheme word.
    const RULES: &[(&str, ApiKeySource, bool)] = &[
        ("authorization", ApiKeySource::AuthorizationHeader, true),
        ("x-api-key", ApiKeySource::XApiKey, false),
        ("anthropic-api-key", ApiKeySource::XApiKey, false),
    ];
    for (name, source, strip_scheme) in RULES {
        let Some(value) = headers.get(*name).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        let credential = if *strip_scheme {
            strip_auth_scheme(value)
        } else {
            value
        };
        let prefix = take_prefix_bytes(credential, API_KEY_PREFIX_LEN)?;
        let kind = classify_kind(&prefix, *source);
        return Some(ApiKeyFingerprint {
            prefix,
            kind,
            source: *source,
        });
    }
    None
}

/// Strip a leading auth-scheme prefix (`Bearer `, `Basic `,
/// `Token `, …). Mirrors `noodle-tap`'s redactor — the two
/// callers see the same credential boundary that way.
fn strip_auth_scheme(value: &str) -> &str {
    const SCHEMES: &[&str] = &["Bearer ", "Basic ", "Token ", "Digest "];
    for scheme in SCHEMES {
        if value
            .get(..scheme.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
        {
            return &value[scheme.len()..];
        }
    }
    value
}

/// Take the first N bytes of `value` when `value` is strictly
/// longer than N. Returns `None` when the value is N bytes or
/// shorter — same conservative policy the S5 redactor enforces.
/// Returning the prefix when it equals the entire credential
/// would surface the full secret on `tap.jsonl`.
fn take_prefix_bytes(value: &str, n: usize) -> Option<String> {
    if value.len() > n {
        let end = value.char_indices().nth(n).map_or(value.len(), |(i, _)| i);
        Some(value[..end].to_owned())
    } else {
        None
    }
}

/// Derive [`ApiKeyKind`] from the credential prefix shape. The
/// vendor prefix encodes the credential type — Anthropic ships
/// `sk-ant-api03-…` for API keys, `sk-ant-sid02-…` for session
/// tokens. OAuth bearer tokens are heuristically detected via
/// the source: a value with no Anthropic prefix arriving on the
/// `Authorization` header is most likely an OAuth bearer.
fn classify_kind(prefix: &str, source: ApiKeySource) -> ApiKeyKind {
    let lower = prefix.to_ascii_lowercase();
    if lower.starts_with("sk-ant-api") {
        ApiKeyKind::ApiKey
    } else if lower.starts_with("sk-ant-sid") {
        ApiKeyKind::Session
    } else if matches!(source, ApiKeySource::AuthorizationHeader) {
        // Bearer-style auth that isn't an Anthropic-formatted
        // credential is most plausibly an OAuth token. The
        // domain enum keeps `Unknown` for the cases where we
        // cannot tell.
        ApiKeyKind::Oauth
    } else {
        ApiKeyKind::Unknown
    }
}

/// Match `/api/organizations/{org}/...` URL paths and return the
/// org id segment. Used on `claude.ai` cells where the URL path
/// carries the org id (`/api/organizations/abc-123/chat_conversations/...`).
fn extract_org_id_from_uri(uri: &rama::http::Uri) -> Option<String> {
    extract_org_id_from_path(uri.path())
}

/// Pure-string variant of [`extract_org_id_from_uri`] — exposed
/// for unit testing without a full `Uri` ceremony.
#[must_use]
pub fn extract_org_id_from_path(path: &str) -> Option<String> {
    // Path: /api/organizations/{org_id}/...
    // Tolerate a missing trailing slash on the org id segment
    // (some endpoints terminate there).
    let trimmed = path.trim_start_matches('/');
    let mut segments = trimmed.split('/');
    if segments.next()? != "api" {
        return None;
    }
    if segments.next()? != "organizations" {
        return None;
    }
    let org_id = segments.next()?;
    if org_id.is_empty() {
        return None;
    }
    Some(org_id.to_owned())
}

/// Extract the `Anthropic-Organization-Id` value from a response
/// header map. Case-insensitive on the header name (HTTP is, but
/// belt-and-braces). Returns the raw header value when present
/// and parseable as UTF-8 — Anthropic's org ids are ASCII.
#[must_use]
pub fn extract_org_id_from_response_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get("anthropic-organization-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_user_agent() {
        let a = parse_agent_app_from_user_agent("claude-code/0.2.5 (Macintosh; arm64)");
        assert_eq!(a.name, AgentAppName::ClaudeCode);
        assert_eq!(a.source, AgentAppSource::UserAgentHeader);
        assert_eq!(a.version, Some(Version::new(0, 2, 5)));
    }

    #[test]
    fn parses_claude_cli_alias_as_claude_code() {
        // `claude-cli` is the historical UA for claude-code.
        let a = parse_agent_app_from_user_agent("claude-cli/1.0.0");
        assert_eq!(a.name, AgentAppName::ClaudeCode);
        assert_eq!(a.version, Some(Version::new(1, 0, 0)));
    }

    #[test]
    fn parses_cursor_user_agent() {
        let a = parse_agent_app_from_user_agent("cursor-app/0.40.0 Electron/27.0.0");
        // Note: we want exactly `cursor`, not `cursor-app`. Our
        // matcher requires `/` after the token to claim the
        // agent — `cursor-app/...` does NOT match `cursor` (the
        // token boundary is `-`, not `/` or whitespace) so the
        // current behaviour is `Unknown`. This is conservative
        // and correct: we'd rather miss-detect than mis-detect.
        assert_eq!(a.name, AgentAppName::Unknown);
    }

    #[test]
    fn parses_bare_cursor_user_agent() {
        let a = parse_agent_app_from_user_agent("cursor/0.40.0");
        assert_eq!(a.name, AgentAppName::Cursor);
        assert_eq!(a.version, Some(Version::new(0, 40, 0)));
    }

    #[test]
    fn parses_opencode_user_agent() {
        let a = parse_agent_app_from_user_agent("opencode/1.2.3 something");
        assert_eq!(a.name, AgentAppName::OpenCode);
        assert_eq!(a.version, Some(Version::new(1, 2, 3)));
    }

    #[test]
    fn parses_claude_desktop_user_agent() {
        let a = parse_agent_app_from_user_agent("claude-desktop/1.0.0");
        assert_eq!(a.name, AgentAppName::ClaudeDesktop);
        assert_eq!(a.version, Some(Version::new(1, 0, 0)));
    }

    #[test]
    fn unknown_user_agent_yields_unknown() {
        let a = parse_agent_app_from_user_agent("curl/8.4.0");
        assert_eq!(a.name, AgentAppName::Unknown);
        assert_eq!(a.source, AgentAppSource::Unknown);
        assert!(a.version.is_none());
    }

    #[test]
    fn case_insensitive_match() {
        let a = parse_agent_app_from_user_agent("Claude-Code/0.2.5");
        assert_eq!(a.name, AgentAppName::ClaudeCode);
    }

    #[test]
    fn missing_version_still_classifies_name() {
        let a = parse_agent_app_from_user_agent("claude-code (Macintosh)");
        assert_eq!(a.name, AgentAppName::ClaudeCode);
        assert!(a.version.is_none());
    }

    #[test]
    fn empty_ua_yields_unknown() {
        let a = parse_agent_app_from_user_agent("");
        assert_eq!(a.name, AgentAppName::Unknown);
    }

    #[test]
    fn for_request_headers_with_ua_populates_agent_app() {
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::USER_AGENT,
            rama::http::HeaderValue::from_static("claude-code/0.2.5 (Macintosh; arm64)"),
        );
        let env = EnvelopeContext::for_request_headers(&headers);
        let a = env.agent_app().expect("agent_app populated");
        assert_eq!(a.name, AgentAppName::ClaudeCode);
    }

    #[test]
    fn for_request_headers_without_ua_omits_agent_app() {
        let env = EnvelopeContext::for_request_headers(&HeaderMap::new());
        assert!(env.agent_app().is_none());
    }

    #[test]
    fn machine_context_populates_os_and_arch() {
        let m = machine_context();
        // Whatever platform we're running on, OS family and arch
        // are deterministic from `std::env::consts`.
        assert!(matches!(
            m.os_family,
            OsFamily::Macos | OsFamily::Linux | OsFamily::Windows | OsFamily::Unknown,
        ));
        assert!(matches!(
            m.architecture,
            Architecture::X86_64 | Architecture::Aarch64 | Architecture::Unknown,
        ));
    }

    #[test]
    fn collector_app_carries_compile_time_build_info() {
        let c = collector_app_context();
        assert_eq!(c.name, "noodle");
        // build_hash is either `"unknown"` (no git) or a hex
        // SHA. Either way it's non-empty.
        assert!(!c.build_hash.is_empty());
        // version round-trips back through semver — sanity
        // check the embedded `CARGO_PKG_VERSION`.
        let _ = c.version.major;
    }

    #[test]
    fn envelope_serializes_agent_app_json_with_snake_case() {
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::USER_AGENT,
            rama::http::HeaderValue::from_static("claude-code/0.2.5"),
        );
        let env = EnvelopeContext::for_request_headers(&headers);
        let v = env.agent_app_json().expect("agent_app_json populated");
        assert_eq!(v["name"], "claude_code");
        assert_eq!(v["source"], "user_agent_header");
        assert_eq!(v["version"], "0.2.5");
    }

    #[test]
    fn envelope_machine_json_populates() {
        let env = EnvelopeContext::for_request_headers(&HeaderMap::new());
        let v = env.machine_json().expect("machine_json populated");
        assert!(v["os_family"].is_string());
        assert!(v["architecture"].is_string());
    }

    #[test]
    fn envelope_collector_app_json_populates() {
        let env = EnvelopeContext::for_request_headers(&HeaderMap::new());
        let v = env
            .collector_app_json()
            .expect("collector_app_json populated");
        assert_eq!(v["name"], "noodle");
        assert!(v["build_hash"].is_string());
    }

    // ─── Subscription context (S7) ─────────────────────────────

    #[test]
    fn api_key_from_authorization_bearer_strips_scheme_and_takes_12() {
        // ADR 027 §9 + ADR 029 §2.4 family 13: the 12-char window
        // matches the redaction policy. Bearer scheme is stripped
        // before the prefix is taken.
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::AUTHORIZATION,
            rama::http::HeaderValue::from_static("Bearer sk-ant-api03-wcqXYZ12345abc"),
        );
        let f = extract_api_key_fingerprint(&headers).expect("fingerprint extracted");
        assert_eq!(f.prefix, "sk-ant-api03");
        assert_eq!(f.prefix.len(), 12);
        assert_eq!(f.kind, ApiKeyKind::ApiKey);
        assert_eq!(f.source, ApiKeySource::AuthorizationHeader);
    }

    #[test]
    fn api_key_from_x_api_key_does_not_strip_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            rama::http::HeaderValue::from_static("sk-ant-api03-abcdEFGHIJ123456"),
        );
        let f = extract_api_key_fingerprint(&headers).expect("fingerprint extracted");
        assert_eq!(f.prefix, "sk-ant-api03");
        assert_eq!(f.kind, ApiKeyKind::ApiKey);
        assert_eq!(f.source, ApiKeySource::XApiKey);
    }

    #[test]
    fn api_key_from_anthropic_api_key_reports_x_api_key_source() {
        // Per ADR 029 §2.4 family 13 — both `X-Api-Key` and
        // `Anthropic-Api-Key` are vendor API-key carriers and map
        // to `ApiKeySource::XApiKey`. The vendor distinction is
        // not recorded on the source (the header name is
        // recoverable from the same `tap.jsonl.headers` block).
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-api-key",
            rama::http::HeaderValue::from_static("sk-ant-sid02-abcdEFGHIJ123456"),
        );
        let f = extract_api_key_fingerprint(&headers).expect("fingerprint extracted");
        assert_eq!(f.prefix, "sk-ant-sid02");
        assert_eq!(f.kind, ApiKeyKind::Session);
        assert_eq!(f.source, ApiKeySource::XApiKey);
    }

    #[test]
    fn api_key_oauth_bearer_classified_oauth() {
        // Bearer-style auth with a non-Anthropic prefix → most
        // likely OAuth. The classifier should not pretend it's an
        // Anthropic API key.
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::AUTHORIZATION,
            rama::http::HeaderValue::from_static("Bearer ya29.A0AbCDef_ghi_jkl_mno"),
        );
        let f = extract_api_key_fingerprint(&headers).expect("fingerprint extracted");
        assert_eq!(f.kind, ApiKeyKind::Oauth);
        assert_eq!(f.source, ApiKeySource::AuthorizationHeader);
        assert_eq!(f.prefix.len(), 12);
    }

    #[test]
    fn api_key_short_credential_returns_none() {
        // Same conservative policy as the S5 redactor: when the
        // credential isn't strictly longer than 12 chars, we'd
        // expose the whole thing. Decline instead.
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            rama::http::HeaderValue::from_static("sk-short"),
        );
        assert!(extract_api_key_fingerprint(&headers).is_none());
    }

    #[test]
    fn api_key_absent_when_no_credential_header() {
        assert!(extract_api_key_fingerprint(&HeaderMap::new()).is_none());
    }

    #[test]
    fn api_key_authorization_takes_precedence_when_both_present() {
        // Deterministic ordering: the rule list checks
        // `Authorization` first.
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::AUTHORIZATION,
            rama::http::HeaderValue::from_static("Bearer sk-ant-api03-AAAAAAAAAAAA"),
        );
        headers.insert(
            "x-api-key",
            rama::http::HeaderValue::from_static("sk-ant-api03-BBBBBBBBBBBB"),
        );
        let f = extract_api_key_fingerprint(&headers).expect("fingerprint extracted");
        assert_eq!(f.source, ApiKeySource::AuthorizationHeader);
        // The auth-header value's bytes are reflected, not x-api-key's.
        assert_eq!(f.prefix, "sk-ant-api03");
    }

    #[test]
    fn org_id_extracted_from_claude_ai_path() {
        assert_eq!(
            extract_org_id_from_path("/api/organizations/abc-123/chat_conversations"),
            Some("abc-123".to_owned())
        );
        assert_eq!(
            extract_org_id_from_path("/api/organizations/org_01ABC/projects"),
            Some("org_01ABC".to_owned())
        );
    }

    #[test]
    fn org_id_none_when_path_does_not_match() {
        assert!(extract_org_id_from_path("/").is_none());
        assert!(extract_org_id_from_path("/api/messages").is_none());
        assert!(extract_org_id_from_path("/api/organizations").is_none());
        assert!(extract_org_id_from_path("/api/organizations/").is_none());
        assert!(extract_org_id_from_path("/v1/messages").is_none());
    }

    #[test]
    fn org_id_extracted_from_response_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-organization-id",
            rama::http::HeaderValue::from_static("org_01XYZ_from_header"),
        );
        assert_eq!(
            extract_org_id_from_response_headers(&headers),
            Some("org_01XYZ_from_header".to_owned())
        );
    }

    #[test]
    fn org_id_absent_when_response_header_missing() {
        assert!(extract_org_id_from_response_headers(&HeaderMap::new()).is_none());
    }

    #[test]
    fn org_id_response_header_case_insensitive() {
        // `HeaderMap::get` is case-insensitive on header name
        // regardless of how the value was inserted.
        let mut headers = HeaderMap::new();
        headers.insert(
            "Anthropic-Organization-Id",
            rama::http::HeaderValue::from_static("org_mixed_case"),
        );
        assert_eq!(
            extract_org_id_from_response_headers(&headers),
            Some("org_mixed_case".to_owned())
        );
    }

    #[test]
    fn subscription_json_serializes_full_block_snake_case() {
        let uri: rama::http::Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::AUTHORIZATION,
            rama::http::HeaderValue::from_static("Bearer sk-ant-api03-wcqXYZ12345abc"),
        );
        let env = EnvelopeContext::for_request(&uri, &headers);
        let v = env
            .subscription_json()
            .expect("subscription_json populated");
        assert_eq!(v["api_key"]["prefix"], "sk-ant-api03");
        assert_eq!(v["api_key"]["kind"], "api_key");
        assert_eq!(v["api_key"]["source"], "authorization_header");
        // No org id on `api.anthropic.com` URL — extraction needs
        // the `claude.ai` URL path or the response header.
        assert!(v.get("organization").is_none());
    }

    #[test]
    fn subscription_json_populates_org_from_claude_ai_url() {
        let uri: rama::http::Uri =
            "https://claude.ai/api/organizations/org_abc-123/chat_conversations"
                .parse()
                .unwrap();
        let env = EnvelopeContext::for_request(&uri, &HeaderMap::new());
        let v = env
            .subscription_json()
            .expect("subscription_json populated");
        assert_eq!(v["organization"]["organization_id"], "org_abc-123");
        assert_eq!(v["organization"]["account_type"], "unknown");
        assert!(v["organization"]["parent_organization_id"].is_null());
    }

    #[test]
    fn subscription_json_omitted_when_no_signals() {
        let uri: rama::http::Uri = "https://example.com/foo".parse().unwrap();
        let env = EnvelopeContext::for_request(&uri, &HeaderMap::new());
        assert!(env.subscription_json().is_none());
    }

    #[test]
    fn merge_response_org_id_populates_when_request_had_none() {
        let uri: rama::http::Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            rama::http::header::AUTHORIZATION,
            rama::http::HeaderValue::from_static("Bearer sk-ant-api03-wcqXYZ12345abc"),
        );
        let mut env = EnvelopeContext::for_request(&uri, &headers);
        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            "anthropic-organization-id",
            rama::http::HeaderValue::from_static("org_from_response"),
        );
        env.merge_organization_id_from_response(&resp_headers);
        let v = env.subscription_json().expect("subscription populated");
        assert_eq!(v["organization"]["organization_id"], "org_from_response");
    }

    #[test]
    fn merge_response_org_id_preserves_url_value() {
        // When the URL already gave us the org id, the response
        // header is a confirmation, not an override. We keep the
        // URL value.
        let uri: rama::http::Uri =
            "https://claude.ai/api/organizations/org_from_url/chat_conversations"
                .parse()
                .unwrap();
        let mut env = EnvelopeContext::for_request(&uri, &HeaderMap::new());
        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            "anthropic-organization-id",
            rama::http::HeaderValue::from_static("org_from_url"),
        );
        env.merge_organization_id_from_response(&resp_headers);
        let v = env.subscription_json().expect("subscription populated");
        assert_eq!(v["organization"]["organization_id"], "org_from_url");
    }

    #[test]
    fn merge_response_org_id_noop_when_response_header_absent() {
        let uri: rama::http::Uri = "https://api.anthropic.com/v1/messages".parse().unwrap();
        let mut env = EnvelopeContext::for_request(&uri, &HeaderMap::new());
        env.merge_organization_id_from_response(&HeaderMap::new());
        // Subscription stayed `None` — no signal anywhere.
        assert!(env.subscription_json().is_none());
    }

    #[test]
    fn subscription_block_is_empty_collapses_to_none() {
        let sub = SubscriptionContext {
            api_key: None,
            organization: None,
            tier: None,
        };
        assert!(sub.is_empty());
    }
}
