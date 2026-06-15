//! `AnthropicMessagesRequestCodec` — the per-domain L5 **request**
//! codec for the documented Anthropic Messages API (ADR 018
//! slice 18.3). The endpoint the Claude Code **CLI**, SDK, and
//! `OpenWhispr` hit (desktop apps use `claude.ai` instead —
//! proven by the captures).
//!
//! Wire envelope (from `claude-code-cli-multi-turn-capture.mitm`):
//! `POST api.anthropic.com/v1/messages` with JSON
//! `{"model","messages":[…],"system":[{"type":"text","text":…}],
//! "tools":[…],"stream","max_tokens", …}`. **Stateless**: the
//! client resends the full `messages[]` + `system` every turn, so
//! the enhancer writes `system` on every request (idempotent —
//! ADR 018 §6).
//!
//! Key fact the documented spec would not have pinned: `system`
//! is a **list of typed blocks** `[{"type":"text","text":…}]`,
//! not a bare string. The attribution directive is therefore
//! **appended as a new `{type:text,text}` block**; every existing
//! block (including the CLI's `x-anthropic-billing-header` block)
//! is preserved untouched.
//!
//! Byte-fidelity is the same `EventSource` pattern as 18.4
//! (ADR 018 §8): retain raw bytes + parsed `Value`; un-enhanced
//! `encode` replays raw **verbatim**; enhanced `encode` mutates
//! only the retained `Value`'s `system` array.

use bytes::Bytes;
use noodle_core::endpoint::{EndpointMatcher, HostMatch, PathMatch};
use noodle_core::event::Role;
use noodle_core::layered::{Codec, CodecInstance, CodecProbe};
use noodle_core::request::{NormalizedRequest, RequestMessage, SystemDirective};

/// Factory. Stateless; routing predicate is a static
/// [`EndpointMatcher`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicMessagesRequestCodec;

impl AnthropicMessagesRequestCodec {
    pub const NAME: &'static str = "anthropic.messages_request";

    fn matcher() -> EndpointMatcher {
        EndpointMatcher::new(
            HostMatch::Exact("api.anthropic.com".to_owned()),
            // prefix + suffix both `/v1/messages` ⇒ exact path:
            // excludes `/v1/messages/batches` (a different API).
            PathMatch {
                prefix: Some("/v1/messages".to_owned()),
                contains: None,
                suffix: Some("/v1/messages".to_owned()),
            },
        )
        .with_method(http::Method::POST)
    }
}

impl Codec for AnthropicMessagesRequestCodec {
    type Input = Bytes;
    type Output = NormalizedRequest;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn matches(&self, probe: &CodecProbe<'_>) -> bool {
        Self::matcher().matches(probe)
    }

    fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = NormalizedRequest>> {
        Box::new(AnthropicMessagesRequestInstance::default())
    }
}

/// Maps an Anthropic role string to [`Role`].
fn role_of(s: &str) -> Role {
    match s {
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

/// Flatten message `content` (string or block list) to the plain
/// text view transforms see. Non-text blocks (`tool_use`,
/// `tool_result`, `thinking`) are omitted from the *view* — wire
/// fidelity is preserved via the retained raw/`Value`, never
/// reconstructed from this.
fn content_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    arr.iter()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Join the wire `system` (string or block list) into the
/// abstract existing-system text for [`SystemDirective`].
fn system_text(system: &serde_json::Value) -> Option<String> {
    if let Some(s) = system.as_str() {
        return Some(s.to_owned());
    }
    let arr = system.as_array()?;
    let joined = arr
        .iter()
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    Some(joined)
}

#[derive(Default)]
pub struct AnthropicMessagesRequestInstance {
    retained: Option<(Bytes, serde_json::Value)>,
}

impl CodecInstance for AnthropicMessagesRequestInstance {
    type Input = Bytes;
    type Output = NormalizedRequest;

    fn decode(&mut self, item: Bytes) -> Vec<NormalizedRequest> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&item) else {
            return Vec::new();
        };
        // The endpoint signature: a `messages` array. Decline
        // anything else (§16 empty-on-error, not a guess).
        let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) else {
            return Vec::new();
        };
        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let msgs = messages
            .iter()
            .map(|m| {
                let role = role_of(
                    m.get("role")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("user"),
                );
                let content = m.get("content").map(content_text).unwrap_or_default();
                RequestMessage::new(role, content)
            })
            .collect();
        let existing = value.get("system").and_then(system_text);

        let req = NormalizedRequest::new(model, msgs, SystemDirective::from_wire(existing));
        self.retained = Some((item, value));
        vec![req]
    }

    fn encode(&mut self, item: NormalizedRequest) -> Vec<Bytes> {
        let Some((raw, value)) = &self.retained else {
            return Vec::new();
        };

        // ADR 018 §8: un-enhanced → byte-identical replay.
        if !item.system.is_directive_set() {
            return vec![raw.clone()];
        }
        let Some(directive) = item.system.directive() else {
            return vec![raw.clone()];
        };

        // Enhanced → append one `{type:text,text:directive}` block
        // to the retained `system` array. Existing blocks (incl.
        // the CLI billing-header block) are preserved verbatim
        // because we mutate the retained value, not a rebuild.
        let mut value = value.clone();
        let block = serde_json::json!({ "type": "text", "text": directive });
        match value.get_mut("system") {
            Some(serde_json::Value::Array(arr)) => arr.push(block),
            Some(serde_json::Value::String(s)) => {
                // Spec also allows a bare string; normalise to the
                // block-list form, original text first.
                let prev = std::mem::take(s);
                value["system"] = serde_json::json!([
                    { "type": "text", "text": prev },
                    block,
                ]);
            }
            _ => {
                value["system"] = serde_json::json!([block]);
            }
        }
        match serde_json::to_vec(&value) {
            Ok(bytes) => vec![Bytes::from(bytes)],
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method};

    // Faithful minimal CLI shape: system as a block list with the
    // billing-header block; mixed string/block message content;
    // multi-turn (stateless full history).
    const BODY: &str = r#"{"model":"claude-opus-4-7","max_tokens":64000,"stream":true,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli;"},{"type":"text","text":"You are Claude Code."}],"messages":[{"role":"user","content":[{"type":"text","text":"what is this project?"}]},{"role":"assistant","content":[{"type":"text","text":"It is noodle."}]},{"role":"user","content":"and now?"}],"tools":[],"metadata":{}}"#;

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
    fn matches_messages_not_batches_or_other_hosts() {
        let c = AnthropicMessagesRequestCodec;
        let h = HeaderMap::new();
        let post = Method::POST;
        assert!(c.matches(&probe("api.anthropic.com", "/v1/messages", &post, &h)));
        assert!(!c.matches(&probe(
            "api.anthropic.com",
            "/v1/messages/batches",
            &post,
            &h,
        )));
        assert!(!c.matches(&probe("claude.ai", "/v1/messages", &post, &h)));
    }

    #[test]
    fn decode_extracts_model_multiturn_messages_and_system() {
        let mut inst = AnthropicMessagesRequestInstance::default();
        let r = inst
            .decode(Bytes::from_static(BODY.as_bytes()))
            .pop()
            .unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(r.messages.len(), 3, "full history (stateless)");
        assert_eq!(r.messages[0].role, Role::User);
        assert_eq!(r.messages[0].content, "what is this project?");
        assert_eq!(r.messages[1].role, Role::Assistant);
        assert_eq!(r.messages[2].content, "and now?");
        // existing system = joined block text
        let sys = r.system.existing().unwrap();
        assert!(sys.contains("x-anthropic-billing-header"));
        assert!(sys.contains("You are Claude Code."));
    }

    #[test]
    fn un_enhanced_round_trip_is_byte_identical() {
        let raw = Bytes::from_static(BODY.as_bytes());
        let mut inst = AnthropicMessagesRequestInstance::default();
        let req = inst.decode(raw.clone()).pop().unwrap();
        let out = inst.encode(req);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], raw, "un-enhanced must be byte-exact");
    }

    #[test]
    fn enhanced_appends_system_block_preserving_existing_and_messages() {
        let raw = Bytes::from_static(BODY.as_bytes());
        let mut inst = AnthropicMessagesRequestInstance::default();
        let mut req = inst.decode(raw).pop().unwrap();
        req.system.set_directive("Emit <noodle:work_type> first.");
        let out = inst.encode(req);
        let v: serde_json::Value = serde_json::from_slice(&out[0]).expect("valid JSON");

        let sys = v["system"].as_array().expect("system is a block list");
        assert_eq!(sys.len(), 3, "two original blocks + one appended");
        // Original blocks preserved verbatim, in order.
        assert!(
            sys[0]["text"]
                .as_str()
                .unwrap()
                .contains("x-anthropic-billing-header")
        );
        assert_eq!(sys[1]["text"], "You are Claude Code.");
        // Directive is the appended block.
        assert_eq!(sys[2]["type"], "text");
        assert_eq!(sys[2]["text"], "Emit <noodle:work_type> first.");
        // Conversation untouched.
        assert_eq!(v["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            v["messages"][0]["content"][0]["text"],
            "what is this project?"
        );
        assert_eq!(v["model"], "claude-opus-4-7");
        // Directive must NOT be in any message.
        let blob = serde_json::to_string(&v["messages"]).unwrap();
        assert!(!blob.contains("noodle:work_type"));
    }

    #[test]
    fn bare_string_system_normalised_to_block_list_on_enhance() {
        let body =
            br#"{"model":"m","messages":[{"role":"user","content":"hi"}],"system":"You are X."}"#;
        let mut inst = AnthropicMessagesRequestInstance::default();
        let mut req = inst.decode(Bytes::from_static(body)).pop().unwrap();
        assert_eq!(req.system.existing(), Some("You are X."));
        req.system.set_directive("DIRECTIVE");
        let v: serde_json::Value = serde_json::from_slice(&inst.encode(req)[0]).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys[0]["text"], "You are X.");
        assert_eq!(sys[1]["text"], "DIRECTIVE");
    }

    #[test]
    fn non_messages_body_declined() {
        let mut inst = AnthropicMessagesRequestInstance::default();
        assert!(inst.decode(Bytes::from_static(b"nope")).is_empty());
        assert!(
            inst.decode(Bytes::from_static(br#"{"prompt":"x"}"#))
                .is_empty()
        );
    }
}
