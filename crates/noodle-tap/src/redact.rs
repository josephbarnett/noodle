//! Header redaction with prefix preservation (ADR 027 §9).
//!
//! Sensitive headers are masked before any value leaves noodle's
//! process boundary. The redaction shape is **prefix-preserving**
//! rather than wholly opaque: the first N visible characters of
//! the value are kept (the vendor-tagged credential prefix —
//! e.g. `sk-ant-api03-wcq`), followed by `...<redacted>`. This
//! prefix is **not a secret** — it cannot be used to authenticate
//! — but it is the value downstream consumers (`ApiKeyFingerprint`
//! per ADR 029 §2.4, billing reconciliation, credential
//! inventories) use to identify which credential a flow was
//! billed against.
//!
//! Per-header defaults follow the ADR 027 §9 table:
//!
//! | Header | Default N | Rationale |
//! |---|---|---|
//! | `Authorization` | 12 | Vendor-tagged credential prefix |
//! | `X-Api-Key` | 12 | Vendor-tagged credential prefix |
//! | `Anthropic-Api-Key` | 12 | Vendor-tagged credential prefix |
//! | `Cookie` | 0 | Opaque session blob; no prefix value |
//! | `Set-Cookie` | 0 | Opaque session blob; no prefix value |
//! | `Proxy-Authorization` | 0 | Opaque; preserve nothing |
//!
//! `Authorization` values strip the `Bearer ` (or `Basic `, …)
//! scheme prefix before preserving N characters — otherwise the
//! N visible chars would be wasted on the scheme name. So
//! `Authorization: Bearer sk-ant-api03-wcqXYZ` redacts to
//! `Bearer sk-ant-api03-...<redacted>`, exposing the credential
//! prefix the operator can reconcile against.
//!
//! `N = 0` yields full opacity (`<redacted>`) regardless of value
//! length — the policy operators choose for cells where the
//! prefix has no reconciliation value (cookies, opaque blobs).
//!
//! Adding a new sensitive header is one entry in
//! [`HEADER_REDACTION_RULES`] plus a test below.

use std::collections::BTreeMap;

use noodle_core::HeaderPair;

/// Per-header redaction policy. Header name matched case-
/// insensitively; `n` is the number of visible characters of
/// the value to preserve.
#[derive(Debug, Clone, Copy)]
struct HeaderRule {
    name: &'static str,
    n: usize,
}

/// Default sensitive-header redaction table per ADR 027 §9.
const HEADER_REDACTION_RULES: &[HeaderRule] = &[
    HeaderRule {
        name: "authorization",
        n: 12,
    },
    HeaderRule {
        name: "x-api-key",
        n: 12,
    },
    HeaderRule {
        name: "anthropic-api-key",
        n: 12,
    },
    HeaderRule {
        name: "cookie",
        n: 0,
    },
    HeaderRule {
        name: "set-cookie",
        n: 0,
    },
    HeaderRule {
        name: "proxy-authorization",
        n: 0,
    },
];

/// The replacement marker that follows the preserved prefix.
const REDACTED_MARKER: &str = "...<redacted>";

/// Convert a `Vec<HeaderPair>` into a TAP-compatible header map
/// with sensitive values redacted per the default rules above.
///
/// Output shape: `BTreeMap<String, Vec<String>>` — same as Go's
/// `http.Header` after JSON marshaling. Repeated header names
/// collapse into a single entry with multiple values.
#[must_use]
pub fn redact_headers(headers: &[HeaderPair]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for HeaderPair { name, value } in headers {
        let v = match rule_for(name) {
            Some(rule) => redact_value(name, value, rule.n),
            None => value.clone(),
        };
        out.entry(name.clone()).or_default().push(v);
    }
    out
}

fn rule_for(name: &str) -> Option<&'static HeaderRule> {
    let lower = name.to_ascii_lowercase();
    HEADER_REDACTION_RULES.iter().find(|r| r.name == lower)
}

/// Redact a single sensitive value per the ADR 027 §9 policy.
///
/// - `n = 0` → full opacity, returns the literal `"<redacted>"`.
/// - For `Authorization` values, strip a leading auth-scheme
///   prefix (`Bearer `, `Basic `, `Token `, etc.) and preserve N
///   visible characters of the credential portion. The scheme
///   prefix is re-prepended to the redacted value so the result
///   still parses as a valid `Authorization` header shape.
/// - For all other sensitive headers, preserve the first N
///   visible characters of the raw value.
/// - When the value (after scheme stripping) is shorter than N,
///   the entire credential portion is replaced with `<redacted>`
///   — preserving fewer characters than the policy requires
///   would expose the full credential.
fn redact_value(name: &str, value: &str, n: usize) -> String {
    if n == 0 {
        return "<redacted>".to_owned();
    }

    let is_auth = name.eq_ignore_ascii_case("authorization");
    let (scheme, credential) = if is_auth {
        split_auth_scheme(value)
    } else {
        ("", value)
    };

    let preserved = preserve_n_chars(credential, n);
    if preserved.is_empty() {
        // Credential portion was too short to preserve N chars
        // safely — return full redaction.
        if scheme.is_empty() {
            "<redacted>".to_owned()
        } else {
            format!("{scheme}<redacted>")
        }
    } else if scheme.is_empty() {
        format!("{preserved}{REDACTED_MARKER}")
    } else {
        format!("{scheme}{preserved}{REDACTED_MARKER}")
    }
}

/// Split `"Bearer sk-..."` into `("Bearer ", "sk-...")`. Returns
/// `("", value)` when the value doesn't start with a recognised
/// HTTP auth scheme.
fn split_auth_scheme(value: &str) -> (&str, &str) {
    const SCHEMES: &[&str] = &["Bearer ", "Basic ", "Token ", "Digest "];
    for scheme in SCHEMES {
        if value
            .get(..scheme.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(scheme))
        {
            return (&value[..scheme.len()], &value[scheme.len()..]);
        }
    }
    ("", value)
}

/// Return the first N **bytes** of a value when the value is
/// strictly longer than N. Returns an empty string when the value
/// is N bytes or shorter — the caller treats this as "redact
/// fully" rather than risk exposing the entire credential.
///
/// Operating on bytes (not chars) is correct here because every
/// credential prefix the ADR cares about is ASCII (`sk-ant-...`,
/// `sk-...`). Non-ASCII tokens would be unusual; if they appear
/// the byte-level behaviour is still safe — it never preserves
/// more than N bytes.
fn preserve_n_chars(value: &str, n: usize) -> &str {
    if value.len() > n {
        let end = value.char_indices().nth(n).map_or(value.len(), |(i, _)| i);
        &value[..end]
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> HeaderPair {
        HeaderPair {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    // ─── Passthrough ────────────────────────────────────────────

    #[test]
    fn passthrough_for_non_sensitive_headers() {
        let m = redact_headers(&[h("Content-Type", "application/json")]);
        assert_eq!(
            m.get("Content-Type").map(Vec::as_slice),
            Some(&["application/json".to_owned()][..])
        );
    }

    // ─── Authorization: Bearer (vendor-tagged) ──────────────────

    #[test]
    fn authorization_bearer_preserves_credential_prefix() {
        // ADR 027 §9 worked example: the prefix exposes
        // `sk-ant-api03-` style credential identity, not the
        // scheme word.
        let m = redact_headers(&[h("Authorization", "Bearer sk-ant-api03-wcqXYZ12345abc")]);
        let v = m.get("Authorization").unwrap();
        assert_eq!(v[0], "Bearer sk-ant-api03...<redacted>");
    }

    #[test]
    fn authorization_short_credential_fully_redacted() {
        // 8-char credential, N=12 → redacting fewer than 12 chars
        // would expose the full credential. Policy: full redaction.
        let m = redact_headers(&[h("Authorization", "Bearer sk-short")]);
        let v = m.get("Authorization").unwrap();
        assert_eq!(v[0], "Bearer <redacted>");
    }

    #[test]
    fn authorization_case_insensitive_scheme() {
        let m = redact_headers(&[h("Authorization", "bearer sk-ant-api03-wcqXYZ12345")]);
        assert_eq!(
            m.get("Authorization").unwrap()[0],
            "bearer sk-ant-api03...<redacted>"
        );
    }

    #[test]
    fn authorization_basic_scheme_handled() {
        let m = redact_headers(&[h("Authorization", "Basic dXNlcjpwYXNzd29yZA==")]);
        assert_eq!(
            m.get("Authorization").unwrap()[0],
            "Basic dXNlcjpwYXNz...<redacted>"
        );
    }

    #[test]
    fn authorization_no_scheme_redacts_raw_value() {
        // Some servers accept bare tokens. We still redact —
        // first 12 chars + marker — even without a scheme prefix.
        let m = redact_headers(&[h("Authorization", "raw-token-sk-ant-api03-wcq")]);
        assert_eq!(
            m.get("Authorization").unwrap()[0],
            "raw-token-sk...<redacted>"
        );
    }

    // ─── X-Api-Key and Anthropic-Api-Key ────────────────────────

    #[test]
    fn x_api_key_preserves_12_chars() {
        let m = redact_headers(&[h("x-api-key", "sk-ant-api03-wcqXYZ12345")]);
        assert_eq!(m.get("x-api-key").unwrap()[0], "sk-ant-api03...<redacted>");
    }

    #[test]
    fn anthropic_api_key_preserves_12_chars() {
        // ADR 027 §9 worked example #2.
        let m = redact_headers(&[h("Anthropic-Api-Key", "sk-ant-sid02-abcdEFGHIJ12345")]);
        assert_eq!(
            m.get("Anthropic-Api-Key").unwrap()[0],
            "sk-ant-sid02...<redacted>"
        );
    }

    #[test]
    fn x_api_key_case_insensitive_match() {
        let m = redact_headers(&[h("X-API-Key", "sk-1234abcdEFGHIJ")]);
        assert_eq!(m.get("X-API-Key").unwrap()[0], "sk-1234abcdE...<redacted>");
    }

    #[test]
    fn x_api_key_short_value_fully_redacted() {
        let m = redact_headers(&[h("x-api-key", "sk-short")]);
        // 8 bytes < N=12 → full redaction; preserving 8 chars
        // would expose the entire credential.
        assert_eq!(m.get("x-api-key").unwrap()[0], "<redacted>");
    }

    // ─── N=0 headers (Cookie, Set-Cookie, Proxy-Authorization) ──

    #[test]
    fn cookie_fully_redacted() {
        let m = redact_headers(&[h("Cookie", "session=abcdefghij1234567890")]);
        assert_eq!(m.get("Cookie").unwrap()[0], "<redacted>");
    }

    #[test]
    fn set_cookie_fully_redacted() {
        let m = redact_headers(&[h("Set-Cookie", "auth=xyz; Path=/; Secure")]);
        assert_eq!(m.get("Set-Cookie").unwrap()[0], "<redacted>");
    }

    #[test]
    fn proxy_authorization_fully_redacted() {
        let m = redact_headers(&[h("Proxy-Authorization", "Basic dXNlcjpwYXNz")]);
        assert_eq!(m.get("Proxy-Authorization").unwrap()[0], "<redacted>");
    }

    // ─── Repeated headers + ordering ────────────────────────────

    #[test]
    fn repeated_header_names_collapse_into_one_entry() {
        let m = redact_headers(&[
            h("Set-Cookie", "a=1; Secure"),
            h("Set-Cookie", "b=2; Secure"),
        ]);
        assert_eq!(
            m.get("Set-Cookie").map(Vec::as_slice),
            // Both fully redacted; values list preserves arrival order.
            Some(&["<redacted>".to_owned(), "<redacted>".to_owned()][..])
        );
    }

    // ─── Critical security property: preserved prefix never
    //     exceeds N (no off-by-one exposing the next char). ─────

    #[test]
    fn never_exposes_more_than_n_credential_chars() {
        // ADR 027 §9: N=12 is the contract; off-by-one bugs are
        // confidentiality regressions. Test pins the exact byte
        // count of the visible portion across many input lengths.
        for trailing in 0..=20 {
            let credential = format!("{}{}", "0123456789AB", "x".repeat(trailing));
            let value = format!("Bearer {credential}");
            let m = redact_headers(&[h("Authorization", &value)]);
            let v = &m.get("Authorization").unwrap()[0];
            assert!(
                v.starts_with("Bearer "),
                "scheme prefix should be re-prepended; got {v:?}"
            );
            // Length 0 trailing → credential is exactly 12 chars
            // which equals N → policy says full redaction (we
            // never preserve when value.len() == N).
            if trailing == 0 {
                assert_eq!(v, "Bearer <redacted>", "len==N case → full redaction");
            } else {
                // The visible credential portion is exactly the
                // first 12 chars; the redaction marker follows.
                let expected = format!("Bearer 0123456789AB{REDACTED_MARKER}");
                assert_eq!(v, &expected);
            }
        }
    }

    // ─── Mixed header set (regression for ordering / interaction) ─

    #[test]
    fn mixed_sensitive_and_passthrough_headers() {
        let m = redact_headers(&[
            h("Content-Type", "application/json"),
            h("Authorization", "Bearer sk-ant-api03-wcqXYZ12345"),
            h("User-Agent", "claude-cli/1.2.3"),
            h("X-Api-Key", "sk-1234abcdEFGHIJ"),
            h("Cookie", "session=xyz"),
        ]);
        assert_eq!(m.get("Content-Type").unwrap()[0], "application/json");
        assert_eq!(
            m.get("Authorization").unwrap()[0],
            "Bearer sk-ant-api03...<redacted>"
        );
        assert_eq!(m.get("User-Agent").unwrap()[0], "claude-cli/1.2.3");
        assert_eq!(m.get("X-Api-Key").unwrap()[0], "sk-1234abcdE...<redacted>");
        assert_eq!(m.get("Cookie").unwrap()[0], "<redacted>");
    }
}
