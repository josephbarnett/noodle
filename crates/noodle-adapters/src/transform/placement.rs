//! Placement realizer — applies a configured directive to a raw
//! Anthropic `/v1/messages` request body at the abstract placement
//! the operator selected (`[[context.enhancements]].as`,
//! ADR 048 §5.1.1).
//!
//! Operates on `serde_json::Value` so every field the realizer does
//! not understand survives the round-trip untouched (§5.3: decode
//! only as far as we must). The JSON re-serialization may reorder
//! object keys; key order carries no semantics on the Anthropic API
//! and does not participate in prompt caching (which keys on
//! content values, not raw bytes).
//!
//! Every placement is fail-soft: when its structural precondition
//! does not hold (no user message, alternation would break, …) it
//! returns `false` and the body is left untouched — the caller
//! forwards the original request unchanged.

use noodle_core::config::context::Placement;
use serde_json::{Value, json};

/// Apply `directive` to `body` at `placement`. Returns `true` when
/// the body was mutated; `false` when the placement's structural
/// precondition failed (body untouched — forward unchanged).
#[must_use]
pub fn apply(placement: Placement, body: &mut Value, directive: &str) -> bool {
    match placement {
        Placement::System | Placement::Raw => apply_system(body, directive),
        Placement::Prompt => apply_to_user_message(body, directive, UserPick::First, usize::MAX),
        Placement::UserPrepend => apply_user_prepend(body, directive),
        Placement::UserAppend | Placement::User => {
            apply_to_user_message(body, directive, UserPick::Last, usize::MAX)
        }
        Placement::UserNew => apply_user_new(body, directive),
        Placement::AssistantPrefill => apply_assistant_prefill(body, directive),
        Placement::Metadata => apply_metadata(body, directive),
    }
}

/// `system` / `raw` — append a text block to the provider's
/// `system` construct. String-form `system` is normalized to the
/// block array; an absent `system` is created.
fn apply_system(body: &mut Value, directive: &str) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let block = text_block(directive);
    match obj.get_mut("system") {
        None => {
            obj.insert("system".into(), json!([block]));
            true
        }
        Some(Value::String(s)) => {
            let existing = json!({ "type": "text", "text": s });
            obj.insert("system".into(), json!([existing, block]));
            true
        }
        Some(Value::Array(arr)) => {
            arr.push(block);
            true
        }
        Some(_) => false,
    }
}

#[derive(Clone, Copy)]
enum UserPick {
    First,
    Last,
}

/// `prompt` (first user message, appended) and `user_append` (last
/// user message, appended) share one mechanism: normalize the
/// picked message's `content` to block form, insert the directive
/// text block at `index` (clamped to the block count).
fn apply_to_user_message(body: &mut Value, directive: &str, pick: UserPick, index: usize) -> bool {
    let Some(blocks) = picked_user_blocks(body, pick) else {
        return false;
    };
    let at = index.min(blocks.len());
    blocks.insert(at, text_block(directive));
    true
}

/// `user_prepend` — insert the directive at the head of the last
/// user message, but **after** the contiguous leading run of
/// `tool_result` blocks. The Messages API requires the user turn
/// answering an assistant `tool_use` to lead with its
/// `tool_result` block(s); any block before them makes the API
/// read the `tool_use` as unanswered (ADR 048 §5.1.2).
fn apply_user_prepend(body: &mut Value, directive: &str) -> bool {
    let Some(blocks) = picked_user_blocks(body, UserPick::Last) else {
        return false;
    };
    let lead = blocks
        .iter()
        .take_while(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .count();
    blocks.insert(lead, text_block(directive));
    true
}

/// `user_new` — a new trailing user message. Only when the last
/// message is an assistant turn (preserves user/assistant
/// alternation); otherwise the body is left untouched.
fn apply_user_new(body: &mut Value, directive: &str) -> bool {
    let Some(messages) = messages_mut(body) else {
        return false;
    };
    let last_is_assistant = messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("assistant");
    if !last_is_assistant {
        return false;
    }
    messages.push(json!({ "role": "user", "content": [text_block(directive)] }));
    true
}

/// `assistant_prefill` — a trailing assistant message beginning
/// with the directive (the model continues from it). Only when the
/// last message is a user turn.
fn apply_assistant_prefill(body: &mut Value, directive: &str) -> bool {
    let Some(messages) = messages_mut(body) else {
        return false;
    };
    let last_is_user = messages
        .last()
        .and_then(|m| m.get("role"))
        .and_then(Value::as_str)
        == Some("user");
    if !last_is_user {
        return false;
    }
    messages.push(json!({ "role": "assistant", "content": [text_block(directive)] }));
    true
}

/// `metadata` — top-level `metadata.noodle_directive`.
/// Experimental: NOT model-visible (Anthropic does not surface
/// request metadata to the model); for out-of-band experiments.
fn apply_metadata(body: &mut Value, directive: &str) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let meta = obj.entry("metadata").or_insert_with(|| json!({}));
    let Some(meta_obj) = meta.as_object_mut() else {
        return false;
    };
    meta_obj.insert("noodle_directive".into(), Value::String(directive.into()));
    true
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn messages_mut(body: &mut Value) -> Option<&mut Vec<Value>> {
    body.get_mut("messages")?.as_array_mut()
}

/// Locate the first/last `role == "user"` message, normalize its
/// `content` to the block-array form the API accepts (a string
/// becomes a single text block), and return the block vec.
fn picked_user_blocks(body: &mut Value, pick: UserPick) -> Option<&mut Vec<Value>> {
    let messages = messages_mut(body)?;
    let pos = match pick {
        UserPick::First => messages
            .iter()
            .position(|m| m.get("role").and_then(Value::as_str) == Some("user"))?,
        UserPick::Last => messages
            .iter()
            .rposition(|m| m.get("role").and_then(Value::as_str) == Some("user"))?,
    };
    let message = messages.get_mut(pos)?;
    let content = message.get_mut("content")?;
    if let Some(s) = content.as_str() {
        let normalized = json!([{ "type": "text", "text": s }]);
        *content = normalized;
    }
    content.as_array_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "<system-reminder>directive</system-reminder>";

    fn anthropic_body() -> Value {
        json!({
            "model": "claude-opus-4-7",
            "max_tokens": 4096,
            "unknown_future_field": { "keep": "me" },
            "system": [{ "type": "text", "text": "you are an agent" }],
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "first ask" }] },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_X", "name": "Bash", "input": {} }
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_X", "content": "ok" },
                    { "type": "tool_result", "tool_use_id": "toolu_Y", "content": "ok2" },
                    { "type": "text", "text": "carry on" }
                ] }
            ]
        })
    }

    fn texts_of(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn system_appends_text_block_to_array() {
        let mut b = anthropic_body();
        assert!(apply(Placement::System, &mut b, D));
        assert_eq!(texts_of(&b["system"]), vec!["you are an agent", D]);
    }

    #[test]
    fn system_normalizes_string_form() {
        let mut b = json!({ "system": "plain string", "messages": [] });
        assert!(apply(Placement::System, &mut b, D));
        assert_eq!(texts_of(&b["system"]), vec!["plain string", D]);
    }

    #[test]
    fn system_created_when_absent() {
        let mut b = json!({ "messages": [] });
        assert!(apply(Placement::Raw, &mut b, D));
        assert_eq!(texts_of(&b["system"]), vec![D]);
    }

    #[test]
    fn prompt_appends_to_first_user_message() {
        let mut b = anthropic_body();
        assert!(apply(Placement::Prompt, &mut b, D));
        let first = &b["messages"][0]["content"];
        assert_eq!(texts_of(first), vec!["first ask", D]);
    }

    #[test]
    fn user_prepend_lands_after_leading_tool_results() {
        let mut b = anthropic_body();
        assert!(apply(Placement::UserPrepend, &mut b, D));
        let last = b["messages"][2]["content"].as_array().unwrap();
        // §5.1.2: blocks 0-1 stay tool_result; directive at 2.
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1]["type"], "tool_result");
        assert_eq!(last[2]["text"], D);
        assert_eq!(last[3]["text"], "carry on");
    }

    #[test]
    fn user_prepend_leads_when_no_tool_results() {
        let mut b = json!({ "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "ask" }] }
        ]});
        assert!(apply(Placement::UserPrepend, &mut b, D));
        let blocks = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], D);
        assert_eq!(blocks[1]["text"], "ask");
    }

    #[test]
    fn user_append_appends_to_last_user_message() {
        let mut b = anthropic_body();
        assert!(apply(Placement::UserAppend, &mut b, D));
        let last = b["messages"][2]["content"].as_array().unwrap();
        assert_eq!(last.last().unwrap()["text"], D);
    }

    #[test]
    fn user_append_normalizes_string_content() {
        let mut b = json!({ "messages": [
            { "role": "user", "content": "plain ask" }
        ]});
        assert!(apply(Placement::User, &mut b, D));
        let blocks = b["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["text"], "plain ask");
        assert_eq!(blocks[1]["text"], D);
    }

    #[test]
    fn user_new_only_after_assistant_turn() {
        // Last message is user → precondition fails, untouched.
        let mut b = anthropic_body();
        let before = b.clone();
        assert!(!apply(Placement::UserNew, &mut b, D));
        assert_eq!(b, before);

        // Last message assistant → new trailing user message.
        let mut b2 = json!({ "messages": [
            { "role": "user", "content": [{ "type": "text", "text": "ask" }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "answer" }] }
        ]});
        assert!(apply(Placement::UserNew, &mut b2, D));
        let msgs = b2["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["text"], D);
    }

    #[test]
    fn assistant_prefill_only_after_user_turn() {
        // Last message is user → prefill appended.
        let mut b = anthropic_body();
        assert!(apply(Placement::AssistantPrefill, &mut b, D));
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs.last().unwrap()["role"], "assistant");
        assert_eq!(msgs.last().unwrap()["content"][0]["text"], D);

        // Last message assistant → untouched.
        let mut b2 = json!({ "messages": [
            { "role": "assistant", "content": [{ "type": "text", "text": "answer" }] }
        ]});
        let before = b2.clone();
        assert!(!apply(Placement::AssistantPrefill, &mut b2, D));
        assert_eq!(b2, before);
    }

    #[test]
    fn metadata_sets_noodle_directive() {
        let mut b = anthropic_body();
        assert!(apply(Placement::Metadata, &mut b, D));
        assert_eq!(b["metadata"]["noodle_directive"], D);
    }

    #[test]
    fn unknown_fields_survive_every_placement() {
        for p in [
            Placement::System,
            Placement::Prompt,
            Placement::UserPrepend,
            Placement::UserAppend,
            Placement::AssistantPrefill,
            Placement::Metadata,
        ] {
            let mut b = anthropic_body();
            assert!(apply(p, &mut b, D), "placement {p:?} should apply");
            assert_eq!(
                b["unknown_future_field"]["keep"], "me",
                "placement {p:?} disturbed an unrelated field"
            );
        }
    }

    #[test]
    fn no_user_message_fails_soft() {
        let mut b = json!({ "messages": [
            { "role": "assistant", "content": [{ "type": "text", "text": "a" }] }
        ]});
        let before = b.clone();
        assert!(!apply(Placement::UserPrepend, &mut b, D));
        assert!(!apply(Placement::UserAppend, &mut b, D));
        assert!(!apply(Placement::Prompt, &mut b, D));
        assert_eq!(b, before);
    }
}
