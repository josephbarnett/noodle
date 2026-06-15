//! `ClaudeAiChatRequestCodec` — the per-domain L5 **request**
//! codec for the Claude Desktop / claude.ai chat-completion
//! endpoint (ADR 018, slice 18.4).
//!
//! Wire envelope (from `claude-desktop-chat-*.mitm`):
//! `POST claude.ai/api/organizations/{org}/chat_conversations/{conv}/completion`
//! with JSON `{"prompt":"<user text>","personalized_styles":[{…,
//! "prompt":"Normal\n",…}],"model":"…","tools":[…],"locale",
//! "timezone", …}`. Conversation state is server-side (no history
//! in the body); `personalized_styles` is client-rebuilt and
//! resent every turn — the steering slot the attribution
//! directive lands in (ADR 018 §2.3).
//!
//! Byte-fidelity (ADR 018 §8 — the `EventSource` pattern applied
//! to the request body): the instance retains the **raw bytes**
//! and the parsed `Value`. `encode` of an **un-enhanced**
//! request replays the raw bytes **verbatim** (byte-identical,
//! 015 §2.1.1); an **enhanced** request re-serialises the
//! retained `Value` with only `personalized_styles[active].prompt`
//! rewritten — every other field preserved because we mutate the
//! retained value, not a reconstruction.
//!
//! `decode` expects the **complete** request body in one call
//! (ADR 018 §8 assumption — request bodies are bounded; chunked
//! assembly is the proxy's job in 18.6).

use bytes::Bytes;
use noodle_core::endpoint::{EndpointMatcher, HostMatch, PathMatch};
use noodle_core::event::Role;
use noodle_core::layered::{Codec, CodecInstance, CodecProbe};
use noodle_core::request::{NormalizedRequest, RequestMessage, SystemDirective};

/// Factory. Stateless; the routing predicate is a static
/// [`EndpointMatcher`] (ADR 019's `(address, endpoint)` cell).
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeAiChatRequestCodec;

impl ClaudeAiChatRequestCodec {
    pub const NAME: &'static str = "claude_ai.chat_request";

    fn matcher() -> EndpointMatcher {
        EndpointMatcher::new(
            HostMatch::Exact("claude.ai".to_owned()),
            PathMatch {
                prefix: Some("/api/organizations/".to_owned()),
                contains: Some("/chat_conversations/".to_owned()),
                suffix: Some("/completion".to_owned()),
            },
        )
        .with_method(http::Method::POST)
    }
}

impl Codec for ClaudeAiChatRequestCodec {
    type Input = Bytes;
    type Output = NormalizedRequest;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn matches(&self, probe: &CodecProbe<'_>) -> bool {
        Self::matcher().matches(probe)
    }

    fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = NormalizedRequest>> {
        Box::new(ClaudeAiChatRequestInstance::default())
    }
}

/// Per-flow instance. Retains the raw bytes + parsed value of the
/// last decoded request so `encode` can replay verbatim
/// (un-enhanced) or splice the directive into the retained value
/// (enhanced) — ADR 018 §8.
#[derive(Default)]
pub struct ClaudeAiChatRequestInstance {
    retained: Option<Retained>,
}

struct Retained {
    raw: Bytes,
    value: serde_json::Value,
    /// Index into `personalized_styles` of the active style whose
    /// `prompt` is the steering slot. `None` when the array is
    /// absent/empty (unobserved for claude.ai, handled defensively).
    active_style: Option<usize>,
}

/// Pick the active personalised style: the `isDefault: true`
/// entry, else the first. Returns its index and current `prompt`.
fn active_style(value: &serde_json::Value) -> Option<(usize, String)> {
    let arr = value.get("personalized_styles")?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let idx = arr
        .iter()
        .position(|s| s.get("isDefault").and_then(serde_json::Value::as_bool) == Some(true))
        .unwrap_or(0);
    let prompt = arr
        .get(idx)
        .and_then(|s| s.get("prompt"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    Some((idx, prompt))
}

impl CodecInstance for ClaudeAiChatRequestInstance {
    type Input = Bytes;
    type Output = NormalizedRequest;

    fn decode(&mut self, item: Bytes) -> Vec<NormalizedRequest> {
        // §16 empty-on-error: a body we can't parse as the
        // expected object is declined, not guessed.
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&item) else {
            return Vec::new();
        };
        let Some(prompt) = value.get("prompt").and_then(serde_json::Value::as_str) else {
            return Vec::new();
        };
        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let style = active_style(&value);
        let existing = style.as_ref().map(|(_, p)| p.clone());

        let req = NormalizedRequest::new(
            model,
            vec![RequestMessage::new(Role::User, prompt)],
            SystemDirective::from_wire(existing),
        );
        self.retained = Some(Retained {
            raw: item,
            value,
            active_style: style.map(|(i, _)| i),
        });
        vec![req]
    }

    fn encode(&mut self, item: NormalizedRequest) -> Vec<Bytes> {
        let Some(retained) = &self.retained else {
            // No decode preceded this encode — nothing to be
            // faithful to. Out of the round-trip contract; §16
            // empty rather than fabricate a body.
            return Vec::new();
        };

        // ADR 018 §8: un-enhanced → replay raw verbatim
        // (byte-identical, 015 §2.1.1).
        if !item.system.is_directive_set() {
            return vec![retained.raw.clone()];
        }

        // Enhanced → re-serialise the retained value with only
        // the active style's `prompt` rewritten to the composed
        // steering text (existing style text + directive).
        let Some(idx) = retained.active_style else {
            // claude.ai always sends personalized_styles; if it
            // didn't we cannot place the directive in the steering
            // slot. Replaying raw would silently drop the
            // directive — surface it instead of failing silently.
            tracing::warn!(
                codec = Self::name_static(),
                "enhanced request has no personalized_styles slot; \
                 directive not placed (unexpected for claude.ai)",
            );
            return vec![retained.raw.clone()];
        };
        let mut value = retained.value.clone();
        if let Some(slot) = value
            .get_mut("personalized_styles")
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|a| a.get_mut(idx))
            .and_then(|s| s.get_mut("prompt"))
        {
            let composed = item.system.composed().unwrap_or_default();
            *slot = serde_json::Value::String(composed);
        }
        match serde_json::to_vec(&value) {
            Ok(bytes) => vec![Bytes::from(bytes)],
            Err(_) => Vec::new(),
        }
    }
}

impl ClaudeAiChatRequestInstance {
    fn name_static() -> &'static str {
        ClaudeAiChatRequestCodec::NAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method};

    const BODY: &str = r#"{"prompt":"what is a man in the middle?","timezone":"America/New_York","personalized_styles":[{"type":"default","key":"Default","name":"Normal","nameKey":"normal_style_name","prompt":"Normal\n","summary":"Default responses from Claude","summaryKey":"normal_style_summary","isDefault":true}],"locale":"en-US","model":"claude-haiku-4-5-20251001","tools":[],"rendering_mode":"messages","parent_message_uuid":"019e32af-43f1-7f86-91d2-63149273edd0"}"#;

    fn probe<'a>(host: &'a str, path: &'a str, m: &'a Method, h: &'a HeaderMap) -> CodecProbe<'a> {
        CodecProbe {
            host,
            path,
            method: m,
            request_headers: h,
            response_status: None,
            response_content_type: None,
        }
    }

    #[test]
    fn matches_only_the_completion_endpoint() {
        let c = ClaudeAiChatRequestCodec;
        let h = HeaderMap::new();
        let post = Method::POST;
        assert!(c.matches(&probe(
            "claude.ai",
            "/api/organizations/abc/chat_conversations/def/completion",
            &post,
            &h,
        )));
        // telemetry on the same host must not match
        assert!(!c.matches(&probe(
            "claude.ai",
            "/api/organizations/abc/sync/settings",
            &post,
            &h,
        )));
    }

    #[test]
    fn decode_extracts_model_prompt_and_existing_style() {
        let mut inst = ClaudeAiChatRequestInstance::default();
        let out = inst.decode(Bytes::from_static(BODY.as_bytes()));
        assert_eq!(out.len(), 1);
        let r = &out[0];
        assert_eq!(r.model.as_deref(), Some("claude-haiku-4-5-20251001"));
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].role, Role::User);
        assert_eq!(r.messages[0].content, "what is a man in the middle?");
        assert_eq!(r.system.existing(), Some("Normal\n"));
        assert!(!r.system.is_directive_set());
    }

    #[test]
    fn un_enhanced_round_trip_is_byte_identical() {
        // ADR 018 §8 / 015 §2.1.1: the gate.
        let raw = Bytes::from_static(BODY.as_bytes());
        let mut inst = ClaudeAiChatRequestInstance::default();
        let decoded = inst.decode(raw.clone());
        let encoded = inst.encode(decoded.into_iter().next().unwrap());
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0], raw, "un-enhanced request must be byte-exact");
    }

    #[test]
    fn enhanced_directive_lands_in_style_prompt_not_user_prompt() {
        let raw = Bytes::from_static(BODY.as_bytes());
        let mut inst = ClaudeAiChatRequestInstance::default();
        let mut req = inst.decode(raw).into_iter().next().unwrap();
        req.system
            .set_directive("Tag your work with <noodle:work_type>.");
        let out = inst.encode(req);
        assert_eq!(out.len(), 1);
        let v: serde_json::Value = serde_json::from_slice(&out[0]).expect("valid JSON");

        // The user's prompt is untouched.
        assert_eq!(v["prompt"], "what is a man in the middle?");
        // Directive is composed into the active style's prompt:
        // original style text preserved + directive appended.
        let style_prompt = v["personalized_styles"][0]["prompt"].as_str().unwrap();
        assert!(
            style_prompt.starts_with("Normal\n"),
            "original style text preserved: {style_prompt:?}",
        );
        assert!(
            style_prompt.contains("Tag your work with <noodle:work_type>."),
            "directive present in steering slot: {style_prompt:?}",
        );
        // Every other field preserved (we mutate the retained
        // value, not a reconstruction).
        assert_eq!(v["model"], "claude-haiku-4-5-20251001");
        assert_eq!(v["timezone"], "America/New_York");
        assert_eq!(v["locale"], "en-US");
        assert_eq!(
            v["parent_message_uuid"],
            "019e32af-43f1-7f86-91d2-63149273edd0",
        );
        assert!(v.get("tools").is_some());
    }

    #[test]
    fn unparseable_body_is_declined() {
        let mut inst = ClaudeAiChatRequestInstance::default();
        assert!(inst.decode(Bytes::from_static(b"not json")).is_empty());
        // a JSON object without "prompt" is not this endpoint
        assert!(inst.decode(Bytes::from_static(br#"{"foo":1}"#)).is_empty());
    }

    #[test]
    fn encode_without_prior_decode_is_empty() {
        let mut inst = ClaudeAiChatRequestInstance::default();
        let req = NormalizedRequest::new(
            Some("m"),
            vec![RequestMessage::new(Role::User, "hi")],
            SystemDirective::none(),
        );
        assert!(inst.encode(req).is_empty());
    }
}
