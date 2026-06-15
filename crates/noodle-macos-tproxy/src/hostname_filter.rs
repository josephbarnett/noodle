//! Per-flow predicate that decides whether the sysext should
//! intercept a TCP flow or let it pass through.
//!
//! ## Why this is a coarse port-based filter (not hostname-based)
//!
//! Apple's `NETransparentProxyProvider` hands the per-flow metadata
//! `remote_endpoint` to the extension as a resolved IP address, not
//! the hostname the app dialed. The macOS NE framework's own log
//! shows `name = api.anthropic.com` separately, but the rama
//! `TransparentProxyFlowMeta` only exposes the IP via
//! `remote_endpoint.host` — every flow we see is `Host::Address(...)`,
//! never `Host::Name(...)`.
//!
//! Domain-based filtering in transparent mode therefore requires one
//! of:
//!   - **SNI peek** — read the first ~100 bytes of the TLS handshake
//!     and extract the `server_name` extension (planned for iteration
//!     3b alongside the actual TLS MITM).
//!   - **DNS snooping cache** — observe UDP/53 queries + responses
//!     and build an IP↔hostname map (complex; later).
//!   - **rama patch** — expose `flow.remoteHostname` through the
//!     Swift bindings; would be the cleanest long-term fix.
//!
//! Iteration 3a takes the simpler L4 cut: claim every TCP flow whose
//! remote port is 443 (TLS) and whose remote address is not
//! private/loopback. Iteration 3b adds the SNI-based refinement so
//! we only actually MITM AI provider hostnames; everything else
//! gets tunneled through transparently.

use rama::net::address::{Host, HostWithPort};

/// AI provider hostnames the SNI-peek stage (iteration 3b) will
/// match against once it can read `server_name` from the TLS
/// `ClientHello`. Exported so the 3b filter can reuse the same list.
#[allow(dead_code)] // wired in by iteration 3b
pub const AI_PROVIDER_HOSTNAMES: &[&str] = &[
    "api.anthropic.com",
    // Claude Desktop chat dials `claude.ai` directly over
    // HTTP/2 (confirmed via mitmproxy capture: POST/GET
    // `/api/organizations/<org>/chat_conversations/<conv>/…`).
    // Chromium/Electron auto-falls-back QUIC→TCP for a locally-
    // trusted CA, so this is interceptable like api.anthropic.com
    // once it's on the SNI allowlist (014 §5.1 / 013 §4).
    "claude.ai",
    // Anthropic console / platform surface (same Cloudflare
    // front; same interception story as claude.ai).
    "platform.claude.com",
    "api.openai.com",
    "api.cohere.com",
    "api.cohere.ai",
    "api.mistral.ai",
    "api.together.xyz",
    "api.groq.com",
    "openrouter.ai",
    "generativelanguage.googleapis.com",
];

/// TCP-flow intercept decision used in iteration 3a.
///
/// Returns `true` for every flow whose remote port is **443** and
/// remote address is public (not loopback, not RFC1918-style
/// private). Iteration 3b adds an SNI peek inside the intercept
/// service that narrows actual MITM to [`AI_PROVIDER_HOSTNAMES`] —
/// non-matching flows get a transparent byte tunnel.
#[must_use]
pub fn should_intercept_tcp(remote: Option<&HostWithPort>) -> bool {
    let Some(endpoint) = remote else {
        return false;
    };
    if endpoint.port != 443 {
        return false;
    }
    match &endpoint.host {
        Host::Address(addr) => is_public_address(addr),
        // Hostname-typed (`Host::Name`) and verbatim (`Host::Uninterpreted`)
        // endpoints don't actually happen in transparent mode today (see
        // module comment), but cover the case for forward-proxy paths and
        // future rama changes that might start populating them — treat as a
        // candidate for inspection.
        _ => true,
    }
}

/// Strict subset of `addr.is_unicast_global()` / `is_private` checks
/// that matches the rama tproxy example: anything not loopback and
/// not RFC1918-private is fair game for inspection. Public IPv6 is
/// treated as public.
fn is_public_address(addr: &std::net::IpAddr) -> bool {
    if addr.is_loopback() {
        return false;
    }
    match addr {
        std::net::IpAddr::V4(v4) => !v4.is_private() && !v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            // Unique-local fc00::/7 — treat as private.
            let segments = v6.segments();
            (segments[0] & 0xfe00) != 0xfc00 && !v6.is_unspecified()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(host: &str, port: u16) -> HostWithPort {
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            HostWithPort {
                host: Host::Address(ip),
                port,
            }
        } else {
            HostWithPort {
                host: Host::Name(host.parse().expect("valid domain in test")),
                port,
            }
        }
    }

    // ── Iteration 3a behavior: port-based filter ───────────────────────

    #[test]
    fn intercepts_public_ipv4_on_443() {
        assert!(should_intercept_tcp(Some(&endpoint("160.79.104.10", 443))));
    }

    #[test]
    fn intercepts_public_ipv6_on_443() {
        assert!(should_intercept_tcp(Some(&endpoint("2607:6bc0::10", 443))));
    }

    #[test]
    fn passes_through_non_443_ports() {
        assert!(!should_intercept_tcp(Some(&endpoint("160.79.104.10", 80))));
        assert!(!should_intercept_tcp(Some(&endpoint(
            "160.79.104.10",
            8080
        ))));
        assert!(!should_intercept_tcp(Some(&endpoint("160.79.104.10", 53))));
    }

    #[test]
    fn passes_through_loopback() {
        assert!(!should_intercept_tcp(Some(&endpoint("127.0.0.1", 443))));
        assert!(!should_intercept_tcp(Some(&endpoint("::1", 443))));
    }

    #[test]
    fn passes_through_rfc1918_ipv4() {
        assert!(!should_intercept_tcp(Some(&endpoint("192.168.1.1", 443))));
        assert!(!should_intercept_tcp(Some(&endpoint("10.0.0.1", 443))));
        assert!(!should_intercept_tcp(Some(&endpoint("172.16.0.1", 443))));
    }

    #[test]
    fn passes_through_link_local() {
        assert!(!should_intercept_tcp(Some(&endpoint("169.254.0.1", 443))));
    }

    #[test]
    fn passes_through_ipv6_unique_local() {
        assert!(!should_intercept_tcp(Some(&endpoint("fc00::1", 443))));
        assert!(!should_intercept_tcp(Some(&endpoint("fd12::1", 443))));
    }

    #[test]
    fn passes_through_missing_endpoint() {
        assert!(!should_intercept_tcp(None));
    }

    // ── Carryover: still intercept when hostname IS present ────────────
    // (Forward-proxy mode + any future change that exposes hostnames.)

    #[test]
    fn intercepts_when_hostname_is_present_on_443() {
        assert!(should_intercept_tcp(Some(&endpoint(
            "api.anthropic.com",
            443
        ))));
    }

    #[test]
    fn passes_through_hostname_on_non_443() {
        assert!(!should_intercept_tcp(Some(&endpoint(
            "api.anthropic.com",
            80
        ))));
    }

    // ── Allowlist invariants (used by iteration 3b SNI matcher) ────────

    #[test]
    fn ai_provider_allowlist_has_no_dupes() {
        for &host in AI_PROVIDER_HOSTNAMES {
            assert_eq!(
                AI_PROVIDER_HOSTNAMES.iter().filter(|h| **h == host).count(),
                1,
                "{host} appears more than once in the allowlist"
            );
        }
    }
}
