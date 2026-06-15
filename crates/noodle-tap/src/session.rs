//! Session-hash extraction.
//!
//! TAP groups events into sessions by `session_hash`. Two sources, in
//! priority order:
//!
//! 1. A known agent session header (e.g. `X-Claude-Code-Session-Id`).
//! 2. SHA-256 of the request body's `system` field, first 12 hex
//!    characters — the consumer-side fallback when no session header
//!    is present.
//!
//! Adding support for a new agent's session header is one entry in
//! [`SESSION_HEADERS`] plus a test below.

use noodle_core::HeaderPair;
use sha2::{Digest, Sha256};

/// Headers that carry a stable per-session identity, checked in order.
pub const SESSION_HEADERS: &[&str] = &[
    "X-Claude-Code-Session-Id", // Claude Code / Claude CLI
    "X-Cursor-Session-Id",      // Cursor (anticipated)
];

/// Extract the session hash for an event.
///
/// - If any of [`SESSION_HEADERS`] is present (case-insensitive), return
///   its value.
/// - Else, if the request body parses as JSON with a `system` field,
///   return the SHA-256 prefix (12 hex chars) of the serialized `system`
///   field.
/// - Else, return `None`.
#[must_use]
pub fn session_hash(headers: &[HeaderPair], body: &[u8]) -> Option<String> {
    if let Some(v) = first_session_header(headers) {
        return Some(v);
    }
    system_prompt_hash(body)
}

fn first_session_header(headers: &[HeaderPair]) -> Option<String> {
    for wanted in SESSION_HEADERS {
        for h in headers {
            if h.name.eq_ignore_ascii_case(wanted) && !h.value.is_empty() {
                return Some(h.value.clone());
            }
        }
    }
    None
}

fn system_prompt_hash(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let system = v.get("system")?;
    // Re-serialize to canonicalize formatting (whitespace, key order
    // within objects). Matches the Go side, which hashes the
    // `json.RawMessage` it received — i.e., whatever bytes the agent
    // sent, not a re-marshal — but our entry point is post-parse so
    // this is the closest equivalent.
    let canon = serde_json::to_vec(system).ok()?;
    let digest = Sha256::digest(&canon);
    let mut s = String::with_capacity(12);
    for b in digest.iter().take(6) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    Some(s)
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

    #[test]
    fn header_takes_precedence_over_body_hash() {
        let headers = vec![h("X-Claude-Code-Session-Id", "abc-123")];
        let body = br#"{"system":"you are helpful"}"#;
        assert_eq!(session_hash(&headers, body).as_deref(), Some("abc-123"));
    }

    #[test]
    fn header_match_is_case_insensitive() {
        let headers = vec![h("x-claude-code-session-id", "lower-case-key")];
        assert_eq!(
            session_hash(&headers, b"{}").as_deref(),
            Some("lower-case-key")
        );
    }

    #[test]
    fn empty_header_value_falls_through_to_body() {
        let headers = vec![h("X-Claude-Code-Session-Id", "")];
        let body = br#"{"system":"you are helpful"}"#;
        let h = session_hash(&headers, body).expect("hash");
        assert_eq!(h.len(), 12, "12 hex chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn body_hash_is_stable_across_calls() {
        let body = br#"{"model":"x","system":"prompt"}"#;
        let a = session_hash(&[], body).expect("hash a");
        let b = session_hash(&[], body).expect("hash b");
        assert_eq!(a, b);
    }

    #[test]
    fn body_without_system_field_yields_none() {
        let body = br#"{"messages":[]}"#;
        assert!(session_hash(&[], body).is_none());
    }

    #[test]
    fn non_json_body_yields_none() {
        assert!(session_hash(&[], b"not json").is_none());
    }
}
