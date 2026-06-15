//! Anthropic request → `tool_result` block extractor (ADR 030 §4.2,
//! refactor overview §2 S11).
//!
//! Walks the JSON body of an `api.anthropic.com /v1/messages`
//! request and pulls every `tool_use_id` referenced by a
//! `tool_result` block in the message history. Used by the wirelog
//! to look up the originating response record via the proxy's
//! pending-tool-uses back-patch table, stamp
//! `pairing.resolves_tool_use_in_request_id` on the current
//! request record, and emit the matching back-patch record for
//! the prior response per ADR 030 §4.3 / §7.3.
//!
//! ## Wire shape (Anthropic request)
//!
//! ```json
//! {
//!   "model": "claude-haiku-4-5",
//!   "messages": [
//!     { "role": "user", "content": "..." },
//!     { "role": "assistant",
//!       "content": [
//!         { "type": "tool_use", "id": "tu_01ABC", "name": "Read",
//!           "input": { "path": "/x" } }
//!       ]
//!     },
//!     { "role": "user",
//!       "content": [
//!         { "type": "tool_result", "tool_use_id": "tu_01ABC",
//!           "is_error": false,
//!           "content": [{ "type": "text", "text": "..." }]
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Tool-result blocks may be string `content` or array `content` —
//! we don't unpack the nested payload here, only the
//! `tool_use_id` (the identity that links to the prior response).
//!
//! ## Lenient parser
//!
//! The hot path tolerates malformed bytes: a body that isn't
//! valid JSON returns an empty list, no panic. Same goes for
//! malformed message shapes (e.g. `content` as a number, or
//! missing fields). The proxy must never reject a request because
//! the pairing detector tripped — pairing is best-effort metadata
//! per ADR 030 §4.

use serde::Deserialize;

/// One observed `tool_result` block in a request's message
/// history. Carries only the `tool_use_id` (and not the nested
/// content) — that's all the pairing needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultRef {
    /// The `tool_use_id` referenced by this `tool_result`.
    /// Matches the id Anthropic stamped on the originating
    /// `tool_use` block in the prior response.
    pub tool_use_id: String,
}

/// Extract every `tool_result` block's `tool_use_id` from an
/// Anthropic `/v1/messages` request body. Order is preserved
/// (declaration order in the `messages[]` array; per-message
/// content array order within each message).
///
/// Returns an empty `Vec` when:
/// - the body is empty,
/// - the body isn't valid JSON,
/// - the body has no `messages` array,
/// - no message in the array carries a `tool_result` block.
///
/// Always lenient — never returns `Err`, never panics.
#[must_use]
pub fn extract_tool_result_refs(body: &[u8]) -> Vec<ToolResultRef> {
    let parsed: AnthropicRequest = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for msg in parsed.messages.into_iter().flatten() {
        let content = match msg.content {
            Some(MessageContent::Array(items)) => items,
            Some(MessageContent::Other(_)) | None => continue,
        };
        for item in content {
            if matches!(item.block_type.as_deref(), Some("tool_result"))
                && let Some(id) = item.tool_use_id
            {
                out.push(ToolResultRef { tool_use_id: id });
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct AnthropicRequest {
    #[serde(default)]
    messages: Option<Vec<Message>>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<MessageContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    /// Structured content — array of typed blocks. Where the
    /// `tool_result` blocks live. The other shape Anthropic
    /// admits — a simple string for user/free-text turns —
    /// carries no `tool_result` blocks by definition; serde's
    /// untagged dispatch falls through to a default-skip path
    /// for any other variant via `Other`.
    Array(Vec<ContentBlock>),
    /// Simple string content (user turn, free-text), or any
    /// other non-array shape. The field is `serde_json::Value`
    /// so serde accepts both string and other scalar/object
    /// variants without erroring. The data isn't read — its
    /// only role is to absorb shapes that aren't an `Array`.
    /// The `#[allow(dead_code)]` is deliberate: the variant is
    /// load-bearing for serde untagged deserialization, but
    /// we never inspect the contents.
    #[allow(dead_code)]
    Other(serde_json::Value),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type", default)]
    block_type: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    // We deliberately don't decode `content`, `is_error`, etc. —
    // pairing only needs the id. Decoding less is faster on the
    // hot path and resilient to vendor field drift.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_returns_empty_list() {
        assert!(extract_tool_result_refs(b"").is_empty());
        assert!(extract_tool_result_refs(b"{}").is_empty());
    }

    #[test]
    fn body_without_messages_returns_empty_list() {
        let body = br#"{"model":"claude-haiku-4-5"}"#;
        assert!(extract_tool_result_refs(body).is_empty());
    }

    #[test]
    fn malformed_json_returns_empty_list_not_panic() {
        // Critical: the proxy hot path must never panic on a
        // junk body. The enhancer path could in principle emit
        // weird bytes, and a downstream client could be malformed.
        assert!(extract_tool_result_refs(b"not json at all").is_empty());
        assert!(extract_tool_result_refs(br#"{"messages": "wrong shape"}"#).is_empty());
    }

    #[test]
    fn extracts_single_tool_result_in_simple_session() {
        let body = br#"{
          "messages": [
            { "role": "user", "content": "Read /x" },
            { "role": "assistant", "content": [
              { "type": "tool_use", "id": "tu_01ABC", "name": "Read",
                "input": { "path": "/x" } }
            ]},
            { "role": "user", "content": [
              { "type": "tool_result", "tool_use_id": "tu_01ABC",
                "is_error": false,
                "content": [{ "type": "text", "text": "ok" }] }
            ]}
          ]
        }"#;
        let refs = extract_tool_result_refs(body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].tool_use_id, "tu_01ABC");
    }

    #[test]
    fn extracts_multiple_tool_results_across_messages() {
        // Realistic claude-code session: multiple tool calls
        // resolve in sequence. The extractor must catch all of
        // them in declaration order.
        let body = br#"{
          "messages": [
            { "role": "user", "content": "do stuff" },
            { "role": "assistant", "content": [
              { "type": "tool_use", "id": "tu_1", "name": "Read", "input": {} }
            ]},
            { "role": "user", "content": [
              { "type": "tool_result", "tool_use_id": "tu_1",
                "content": "ok" }
            ]},
            { "role": "assistant", "content": [
              { "type": "tool_use", "id": "tu_2", "name": "Bash", "input": {} }
            ]},
            { "role": "user", "content": [
              { "type": "tool_result", "tool_use_id": "tu_2",
                "content": "ok" }
            ]}
          ]
        }"#;
        let refs = extract_tool_result_refs(body);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].tool_use_id, "tu_1");
        assert_eq!(refs[1].tool_use_id, "tu_2");
    }

    #[test]
    fn handles_multiple_tool_results_in_one_message() {
        // Some agents batch many tool_results into a single
        // user turn (parallel tool execution). The extractor
        // must catch every block in the content array.
        let body = br#"{
          "messages": [
            { "role": "user", "content": [
              { "type": "tool_result", "tool_use_id": "tu_a", "content": "a" },
              { "type": "tool_result", "tool_use_id": "tu_b", "content": "b" },
              { "type": "tool_result", "tool_use_id": "tu_c", "content": "c" }
            ]}
          ]
        }"#;
        let refs = extract_tool_result_refs(body);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].tool_use_id, "tu_a");
        assert_eq!(refs[1].tool_use_id, "tu_b");
        assert_eq!(refs[2].tool_use_id, "tu_c");
    }

    #[test]
    fn ignores_tool_use_blocks_only_extracts_tool_results() {
        // `tool_use` blocks in the assistant history must NOT
        // surface — the extractor is specifically for
        // tool_result references on the request side.
        let body = br#"{
          "messages": [
            { "role": "assistant", "content": [
              { "type": "tool_use", "id": "tu_X", "name": "Read", "input": {} },
              { "type": "text", "text": "thinking..." }
            ]}
          ]
        }"#;
        assert!(extract_tool_result_refs(body).is_empty());
    }

    #[test]
    fn skips_string_content_messages() {
        // The simple-string content variant carries no blocks.
        let body = br#"{
          "messages": [
            { "role": "user", "content": "no tools here" }
          ]
        }"#;
        assert!(extract_tool_result_refs(body).is_empty());
    }

    #[test]
    fn tool_result_without_tool_use_id_is_skipped() {
        // Defensive: a malformed tool_result missing the id
        // surfaces no ref (consumers downstream can detect this
        // via missing pairing fields).
        let body = br#"{
          "messages": [
            { "role": "user", "content": [
              { "type": "tool_result", "content": "ok" }
            ]}
          ]
        }"#;
        assert!(extract_tool_result_refs(body).is_empty());
    }
}
