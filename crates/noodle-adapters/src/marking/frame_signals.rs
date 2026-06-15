//! Extract ADR 052 §6 [`RequestSignals`] from a `/v1/messages` request body and
//! build response [`ToolUse`] fingerprints from decoded `tool_use` blocks.
//!
//! This is the Rust counterpart of `tools/build_052_fixtures.py`: it must
//! produce, from live wire bytes, the same sanitized signals the Python builder
//! produces from a capture, so the proxy's live marks match the checked-in
//! goldens. All outputs are hashes / ids / enums — no raw text is retained.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::frame_tree::{RequestSignals, ToolUse};

/// Hex SHA-256 of a string (plain sha256 — matches the Python builder's
/// `_sha`; distinct from the domain-separated [`super::SystemHash`]).
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

const WRAPPERS: [&str; 4] = [
    "<session>",
    "<transcript>",
    "[SUGGESTION MODE",
    "<system-reminder>",
];

/// Parse a `/v1/messages` request body into the §6 request-side signals. Returns
/// [`RequestSignals::default`] when the body is empty or unparseable.
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

    // request_tool_result_ids (CHAIN) + message_sig (extends_root).
    let mut request_tool_result_ids = Vec::new();
    let mut message_sig = Vec::new();
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match m.get("content") {
            Some(Value::Array(arr)) => {
                let mut ids = Vec::new();
                for b in arr {
                    match b.get("type").and_then(Value::as_str) {
                        Some("tool_result") => {
                            if let Some(t) = b.get("tool_use_id").and_then(Value::as_str) {
                                request_tool_result_ids.push(t.to_string());
                                ids.push(format!("tr:{t}"));
                            }
                        }
                        Some("tool_use") => {
                            if let Some(id) = b.get("id").and_then(Value::as_str) {
                                ids.push(format!("tu:{id}"));
                            }
                        }
                        Some("text") => {
                            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                            ids.push(format!("tx:{}", &sha256_hex(t)[..12]));
                        }
                        _ => {}
                    }
                }
                message_sig.push(format!("{role}|{}", ids.join(",")));
            }
            Some(other) => {
                let s = other.as_str().unwrap_or("");
                message_sig.push(format!("{role}|tx:{}", &sha256_hex(s)[..12]));
            }
            None => {}
        }
    }

    // first user message text-block hashes (SPAWN match keys).
    let first_user_text_sha256s = msgs
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .map(|m| {
            text_blocks(m.get("content"))
                .iter()
                .map(|t| sha256_hex(t))
                .collect()
        })
        .unwrap_or_default();

    // trailing user message (is_harness_wrapper / genuine_user_text).
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
    let has_genuine_user_text = trailing.iter().any(|t| {
        let s = t.trim();
        !s.is_empty() && !WRAPPERS.iter().any(|w| s.starts_with(w))
    });

    RequestSignals {
        max_tokens,
        request_tool_result_ids,
        first_user_text_sha256s,
        trailing_wrapper_kind,
        has_genuine_user_text,
        message_sig,
    }
}

/// Build a response [`ToolUse`] from a decoded `tool_use` block. `prompt_sha256`
/// is set only for `Task`/`Agent` spawns carrying a string `input.prompt` (the
/// SPAWN fingerprint); every tool (incl. `Bash`/`Read`) is still returned so the
/// detector can register it in `pending_tu` for CHAIN routing.
#[must_use]
pub fn response_tool_use(name: &str, id: &str, input: &Value) -> ToolUse {
    let prompt_sha256 = if matches!(name, "Task" | "Agent") {
        input.get("prompt").and_then(Value::as_str).map(sha256_hex)
    } else {
        None
    };
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
    fn quota_probe_signals() {
        let body = br#"{"max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#;
        let s = request_signals(body);
        assert_eq!(s.max_tokens, Some(1));
        assert_eq!(s.trailing_wrapper_kind, "none");
        assert!(s.has_genuine_user_text); // "quota" is genuine text; mt==1 is the wrapper signal
    }

    #[test]
    fn title_gen_and_chain_and_spawn_keys() {
        // title-gen: trailing <session>
        let title = br#"{"max_tokens":32000,"messages":[{"role":"user","content":[{"type":"text","text":"<session>\nhi"}]}]}"#;
        assert_eq!(request_signals(title).trailing_wrapper_kind, "session");
        assert!(!request_signals(title).has_genuine_user_text);

        // chain: a tool_result id is collected
        let chain = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"go"}]},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_x","name":"Bash"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_x"}]}]}"#;
        let s = request_signals(chain);
        assert_eq!(s.request_tool_result_ids, vec!["toolu_x".to_string()]);
        // first-user text block hash present (spawn match key)
        assert_eq!(s.first_user_text_sha256s.len(), 1);
        assert_eq!(s.first_user_text_sha256s[0], sha256_hex("go"));
    }

    #[test]
    fn spawn_prompt_fingerprint_matches_first_user_hash() {
        // The child's first request carries the spawn prompt verbatim as its
        // first-user text block; the spawn's prompt_sha256 must equal that hash.
        let prompt = "List the contents of crates/";
        let child = format!(
            r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":{prompt:?}}}]}}]}}"#
        );
        let s = request_signals(child.as_bytes());
        let spawn = response_tool_use("Agent", "toolu_a", &serde_json::json!({"prompt": prompt}));
        assert_eq!(
            spawn.prompt_sha256,
            Some(s.first_user_text_sha256s[0].clone())
        );
    }
}
