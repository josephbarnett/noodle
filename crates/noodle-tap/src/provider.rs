//! Host → provider name mapping.
//!
//! TAP groups events by provider in the viewer. Adding a new provider is
//! one line in [`provider_from_url`] plus a test below.

/// Derive the canonical provider name from a request URL or `Host`
/// header. Returns `"unknown"` for hosts we don't recognize.
///
/// Match rules: a provider claims its **eTLD+1**. So
/// `api.anthropic.com`, `mcp-proxy.anthropic.com`, and
/// `downloads.claude.ai` all classify as "anthropic" — they're
/// hosts the same vendor operates and the viewer should colour them
/// the same. Matching is suffix-based (`host == suffix` or
/// `host.ends_with(".{suffix}")`) so a host like
/// `evil-anthropic.com.attacker.net` does NOT match.
/// `(domain suffix, provider name)` table — first match wins.
/// Vendor-owned eTLD+1s live here; subdomains pick up automatically
/// via [`provider_from_url`]'s suffix-match.
const PROVIDER_SUFFIXES: &[(&str, &str)] = &[
    ("anthropic.com", "anthropic"),
    ("claude.ai", "anthropic"),
    ("openai.com", "openai"),
    ("oaistatic.com", "openai"),
    ("generativelanguage.googleapis.com", "google"),
    ("cohere.com", "cohere"),
    ("cohere.ai", "cohere"),
    ("mistral.ai", "mistral"),
];

#[must_use]
pub fn provider_from_url(url_or_host: &str) -> &'static str {
    // Accept either "https://api.anthropic.com/..." or "api.anthropic.com".
    let host = url_or_host
        .split("://")
        .nth(1)
        .unwrap_or(url_or_host)
        .split('/')
        .next()
        .unwrap_or(url_or_host);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();

    for &(suffix, provider) in PROVIDER_SUFFIXES {
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return provider;
        }
    }
    "unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_matches_full_url() {
        assert_eq!(
            provider_from_url("https://api.anthropic.com/v1/messages"),
            "anthropic"
        );
    }

    #[test]
    fn anthropic_matches_bare_host() {
        assert_eq!(provider_from_url("api.anthropic.com"), "anthropic");
    }

    #[test]
    fn openai_matches() {
        assert_eq!(
            provider_from_url("https://api.openai.com/v1/chat/completions"),
            "openai"
        );
    }

    #[test]
    fn host_with_port_strips_port() {
        assert_eq!(
            provider_from_url("https://api.anthropic.com:443/v1/messages"),
            "anthropic"
        );
    }

    #[test]
    fn case_insensitive_host() {
        assert_eq!(
            provider_from_url("https://API.Anthropic.COM/v1/messages"),
            "anthropic"
        );
    }

    #[test]
    fn unknown_host_returns_unknown() {
        assert_eq!(provider_from_url("https://example.com/foo"), "unknown");
        assert_eq!(provider_from_url("not-a-url"), "unknown");
    }

    #[test]
    fn anthropic_subdomains_are_anthropic() {
        // mcp-proxy.anthropic.com showed up unknown in the wild
        // (see screenshot in PR feedback). Pin both subdomain
        // patterns the vendor uses.
        assert_eq!(
            provider_from_url("https://mcp-proxy.anthropic.com/v1/mcp/foo"),
            "anthropic"
        );
        assert_eq!(
            provider_from_url("https://downloads.claude.ai/blob"),
            "anthropic"
        );
        assert_eq!(provider_from_url("https://claude.ai/"), "anthropic");
    }

    #[test]
    fn openai_subdomains_are_openai() {
        assert_eq!(provider_from_url("https://chat.openai.com/"), "openai");
        assert_eq!(
            provider_from_url("https://cdn.oaistatic.com/asset"),
            "openai"
        );
    }

    #[test]
    fn suffix_match_is_anchored_to_a_dot() {
        // `evil-anthropic.com.attacker.net` is NOT anthropic — the
        // suffix has to be a true ancestor in the DNS hierarchy.
        assert_eq!(
            provider_from_url("https://evil-anthropic.com.attacker.net/"),
            "unknown"
        );
        assert_eq!(provider_from_url("https://notanthropic.com/"), "unknown");
    }
}
