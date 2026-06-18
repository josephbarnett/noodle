//! Extract ADR 052 §5 [`RequestSignals`] from a `/v1/messages` request body and
//! build response [`ToolUse`] fingerprints from decoded `tool_use` blocks.
//!
//! All outputs are hashes / ids / enums — no raw text is retained. Frame
//! identity (`agent_id`) is header-derived and set by the proxy; this module
//! computes the body-derived signals only.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::frame_tree::{RequestSignals, ToolUse};

/// Hex SHA-256 of a string (plain sha256 — matches the Python builder's `_sha`).
fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Text blocks of a message's `content` (string form = one implicit block;
/// array form = each `type == "text"` block, in order).
fn text_blocks(content: Option<&Value>) -> Vec<&str> {
    match content {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// The compaction-recap preamble — a genuine-text round-trip that is not a user
/// turn (the harness asks the model to summarize), so it is a side-call.
const RECAP_PREFIX: &str = "The user stepped away and is coming back. Recap";

/// Parse a `/v1/messages` request body into the §5 request-side signals. Returns
/// [`RequestSignals::default`] when the body is empty or unparseable. `agent_id`
/// is left `None` here — it is header-derived and stamped by the proxy.
#[must_use]
pub fn request_signals(body: &[u8]) -> RequestSignals {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return RequestSignals::default();
    };
    let max_tokens = v.get("max_tokens").and_then(Value::as_u64);
    let empty = Vec::new();
    let msgs = v
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    // Trailing user message — its leading text classifies harness wrappers and
    // the compaction recap, the body-derived side-call signals.
    let trailing = msgs
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| text_blocks(m.get("content")))
        .unwrap_or_default();
    let joined = trailing.concat();
    let joined = joined.trim_start();
    let trailing_wrapper_kind = if joined.starts_with("<session>") {
        "session"
    } else if joined.starts_with("<transcript>") {
        "transcript"
    } else if joined.starts_with("[SUGGESTION MODE") {
        "suggestion"
    } else {
        "none"
    }
    .to_string();

    // A round-trip driven by no user prompt: a quota probe (`max_tokens == 1`),
    // a harness wrapper, or the compaction recap. `<system-reminder>` is NOT a
    // side-call — it wraps genuine user turns.
    let side_call = max_tokens == Some(1)
        || trailing_wrapper_kind != "none"
        || joined.starts_with(RECAP_PREFIX);

    RequestSignals {
        max_tokens,
        trailing_wrapper_kind,
        agent_id: None,
        side_call,
    }
}

/// Build a response [`ToolUse`] from a decoded `tool_use` block. `prompt_sha256`
/// is set for any spawn carrying a string `input.prompt` (name-free — the spawn
/// tool is `Task`/`Agent`/`task` across clients).
#[must_use]
pub fn response_tool_use(name: &str, id: &str, input: &Value) -> ToolUse {
    let prompt_sha256 = input.get("prompt").and_then(Value::as_str).map(sha256_hex);
    ToolUse {
        name: name.to_string(),
        id: id.to_string(),
        prompt_sha256,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_probe_is_side_call() {
        let body = br#"{"max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#;
        let s = request_signals(body);
        assert_eq!(s.max_tokens, Some(1));
        assert!(s.side_call);
        assert_eq!(s.agent_id, None);
    }

    #[test]
    fn wrappers_and_recap_are_side_calls() {
        let title = br#"{"max_tokens":32000,"messages":[{"role":"user","content":[{"type":"text","text":"<session>\nhi"}]}]}"#;
        assert_eq!(request_signals(title).trailing_wrapper_kind, "session");
        assert!(request_signals(title).side_call);

        let monitor = br#"{"messages":[{"role":"user","content":"<transcript>\nx"}]}"#;
        assert!(request_signals(monitor).side_call);

        let recap = br#"{"messages":[{"role":"user","content":"The user stepped away and is coming back. Recap in under 40 words"}]}"#;
        assert!(request_signals(recap).side_call);
    }

    #[test]
    fn genuine_prompt_is_not_a_side_call() {
        let body = br#"{"max_tokens":64000,"messages":[{"role":"user","content":"do the thing"}]}"#;
        let s = request_signals(body);
        assert!(!s.side_call);
        assert_eq!(s.trailing_wrapper_kind, "none");
    }

    #[test]
    fn system_reminder_is_not_a_side_call() {
        // `<system-reminder>` wraps genuine user turns — must NOT be a side-call.
        let body = br#"{"messages":[{"role":"user","content":"<system-reminder>\nthe user sent a new message"}]}"#;
        assert!(!request_signals(body).side_call);
    }

    #[test]
    fn spawn_prompt_fingerprint_is_name_free() {
        let spawn = response_tool_use(
            "Agent",
            "toolu_a",
            &serde_json::json!({"prompt": "List crates/"}),
        );
        assert_eq!(spawn.prompt_sha256, Some(sha256_hex("List crates/")));
        let lower = response_tool_use("task", "toolu_b", &serde_json::json!({"prompt": "x"}));
        assert!(
            lower.prompt_sha256.is_some(),
            "name-free: lowercase `task` too"
        );
    }
}
