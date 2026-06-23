//! `ContextEnhancer` driven adapters.
//!
//! - `NoOpEnhancer` — pass-through.
//! - `ConfiguredAnthropicEnhancer` — applies every configured
//!   `[[context.enhancements]]` entry (verbatim `text` at the
//!   configured `as` placement) to Anthropic-shape request bodies,
//!   on every round trip, idempotently (ADR 048 gap review §6.R3 /
//!   G0: the client rebuilds its history each round trip and never
//!   carries our wire-only mutation, so enhancement must recur).
//! - `OpenAiAttributionEnhancer` — prepends a system message containing
//!   the attribution directive to OpenAI-shape request bodies. Gated
//!   on body shape rather than provider name (provider classification
//!   will move into the proxy when `CodecRegistry` is on the request
//!   hot path).

use bytes::Bytes;
use noodle_core::config::context::Enhancement;
use noodle_core::{BoxError, ContextEnhancer, DiscoverContext, EnhanceContext, FieldWriter};

use crate::marking::frame_signals;
use crate::transform::placement;

// ── NoOp ────────────────────────────────────────────────────────────

/// Pass-through enhancer. Body unchanged on enhance; nothing extracted.
pub struct NoOpEnhancer;

impl NoOpEnhancer {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for NoOpEnhancer {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextEnhancer for NoOpEnhancer {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn enhance(&self, _ctx: &EnhanceContext<'_>, body: Bytes) -> Result<Bytes, BoxError> {
        Ok(body)
    }

    fn discover(
        &self,
        _ctx: &DiscoverContext<'_>,
        _text: &str,
        _fields: &mut dyn FieldWriter,
    ) -> Result<(), BoxError> {
        Ok(())
    }
}

// ── Configured Anthropic enhancement (ADR 048 §5 / gap review R3) ─────

/// Applies the operator's `[[context.enhancements]]` entries —
/// verbatim `text` at the configured `as` placement — to
/// Anthropic-shape request bodies at the raw-body seam.
///
/// **Every round trip, idempotently.** The client rebuilds its
/// `messages[]` from its own transcript each round trip; a
/// wire-only mutation never comes back (G0, proven by the 9-turn
/// capture). Idempotence is content-based: a body that already
/// carries the first enhancement's text verbatim is left untouched.
///
/// **Gates** (any failing → forward the original unchanged):
/// 1. Body parses as JSON and is Anthropic-shaped
///    ([`is_anthropic_shaped`] — top-level `system` or
///    Anthropic-only block types).
/// 2. A **genuine new user-text prompt** — not a side-call (quota
///    probe `max_tokens == 1`, security monitor, title/topic
///    classifier, compaction recap; [`frame_signals::request_signals`])
///    and not a tool-result continuation
///    ([`placement::is_genuine_user_text_turn`]). Injecting into a
///    side-call burns its token budget and degrades the agent.
/// 3. Directive not already present (idempotence).
///
/// Every placement is fail-soft (§5.3): a placement whose
/// structural precondition fails leaves the body untouched; a
/// re-serialization failure forwards the original. The worst
/// outcome is "we learned nothing this turn."
pub struct ConfiguredAnthropicEnhancer {
    enhancements: Vec<Enhancement>,
}

impl ConfiguredAnthropicEnhancer {
    #[must_use]
    pub fn new(enhancements: Vec<Enhancement>) -> Self {
        Self { enhancements }
    }
}

impl ContextEnhancer for ConfiguredAnthropicEnhancer {
    fn name(&self) -> &'static str {
        "configured_anthropic"
    }

    fn enhance(&self, _ctx: &EnhanceContext<'_>, body: Bytes) -> Result<Bytes, BoxError> {
        if self.enhancements.is_empty() {
            return Ok(body);
        }
        let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Ok(body),
        };
        if !is_anthropic_shaped(&parsed) {
            return Ok(body);
        }
        // Inject only into a genuine new user-text prompt. Two classes
        // pass through untouched, both verified against captured
        // traffic (`captures/max/*.mitm`):
        //   - **Side-calls** — the agent's internal calls: quota probe
        //     (`max_tokens == 1`), security monitor / title / topic
        //     classifiers (non-streaming, tiny budget, `<transcript>` /
        //     `<session>` wrapper text), and the compaction recap.
        //     Injecting our directive makes them spend their budget
        //     emitting our markers and terminate on `max_tokens`
        //     instead of their `stop_sequence` — we degrade the agent.
        //   - **Tool-result continuations** — a user turn answering a
        //     prior `tool_use` is not a new prompt; a trailing
        //     `role: "system"` message is skipped when locating the
        //     turn (see [`placement::is_genuine_user_text_turn`]).
        if frame_signals::request_signals(&body).side_call
            || !placement::is_genuine_user_text_turn(&parsed)
        {
            return Ok(body);
        }
        // Idempotence: the operator's verbatim text appearing
        // anywhere in the body means this pass (or a replay)
        // already enhanced. Content-based, not session-state-based
        // — survives restarts and replicas for free.
        if let Some(first) = self.enhancements.first()
            && twoway_contains(&body, first.text.as_bytes())
        {
            return Ok(body);
        }
        let mut mutated = false;
        for enhancement in &self.enhancements {
            mutated |= placement::apply(enhancement.r#as, &mut parsed, &enhancement.text);
        }
        if !mutated {
            return Ok(body);
        }
        match serde_json::to_vec(&parsed) {
            Ok(bytes) => Ok(bytes.into()),
            // Fail-soft: never emit bytes we couldn't faithfully
            // produce — forward the original.
            Err(_) => Ok(body),
        }
    }

    fn discover(
        &self,
        _ctx: &DiscoverContext<'_>,
        _text: &str,
        _fields: &mut dyn FieldWriter,
    ) -> Result<(), BoxError> {
        Ok(())
    }
}

/// Naive substring search over raw bytes. Bodies are ≤ a few
/// hundred KB and the needle ~½ KB; this runs once per request and
/// is dwarfed by the JSON parse above.
fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// ── OpenAI attribution ──────────────────────────────────────────────

/// Prepends a `role: "system"` message containing the attribution
/// directive to OpenAI-shape request bodies (JSON with a top-level
/// `messages` array).
///
/// Body-shape gated, not provider-name gated: the enhancer parses the
/// JSON, looks for `messages: [...]`, and bails out if it's not there.
/// Bodies that aren't JSON, or are JSON without `messages`, pass
/// through untouched. This keeps the demo runnable against a local
/// echo upstream without DNS games to make the request URL look like
/// `api.openai.com`.
///
/// **Anthropic exclusion.** `OpenAI` Chat Completions carries the system
/// prompt as a `{"role":"system"}` entry inside `messages`; Anthropic's
/// Messages API forbids that role in `messages` and carries the system
/// prompt in a top-level `system` block list instead. Because the gate
/// above is shape-based, an Anthropic body (which also has a `messages`
/// array) would otherwise get a malformed `{"role":"system"}` shoved
/// into its conversation. So we additionally **decline Anthropic-shaped
/// bodies** (see [`is_anthropic_shaped`]) and let the per-domain
/// `AnthropicMessagesRequestCodec` own `system` enhancement for that
/// provider. Declining does not burn the per-session idempotency flag.
///
/// **Stateless, idempotent per body** (ADR 048 gap review G0/G4a):
/// the client rebuilds its `messages[]` from its own transcript on
/// every round trip — a wire-only mutation never comes back — so
/// the directive must be (re)applied on every request. Idempotence
/// is content-based: a body whose `messages` already lead with our
/// exact system directive is left untouched. The old
/// once-per-`Session` gate (`Session::directive_enhanced`) silently
/// dropped the directive from round trip 2 onward and is gone.
pub struct OpenAiAttributionEnhancer {
    directive: String,
}

impl OpenAiAttributionEnhancer {
    #[must_use]
    pub fn new(directive: impl Into<String>) -> Self {
        Self {
            directive: directive.into(),
        }
    }
}

/// True when a parsed request body looks like the Anthropic Messages
/// API rather than `OpenAI` Chat Completions. Two tells, either sufficient:
///
/// 1. A **top-level `system`** field — Anthropic carries the system
///    prompt here (string or block list); `OpenAI` never does (its system
///    prompt is a `{"role":"system"}` entry inside `messages`). Claude
///    Code always sends one.
/// 2. Any message `content` block whose `type` is Anthropic-only
///    (`thinking`, `tool_use`, `tool_result`) — covers the rare
///    Anthropic turn that omits `system` (e.g. a tool-result follow-up).
///
/// Both checks are conservative: a false positive only means we decline
/// to enhance (the engine's Anthropic codec still does), never a corrupt
/// body. `OpenAI` bodies match neither tell.
fn is_anthropic_shaped(body: &serde_json::Value) -> bool {
    const ANTHROPIC_BLOCK_TYPES: [&str; 3] = ["thinking", "tool_use", "tool_result"];
    if body.get("system").is_some() {
        return true;
    }
    body.get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|block| block.get("type").and_then(serde_json::Value::as_str))
        .any(|t| ANTHROPIC_BLOCK_TYPES.contains(&t))
}

impl ContextEnhancer for OpenAiAttributionEnhancer {
    fn name(&self) -> &'static str {
        "openai_attribution"
    }

    fn enhance(&self, _ctx: &EnhanceContext<'_>, body: Bytes) -> Result<Bytes, BoxError> {
        // Body shape gate. Anything that doesn't parse to JSON, or
        // doesn't have a `messages` array, passes through untouched
        // — and does NOT burn the per-session idempotency flag.
        let mut parsed: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Ok(body),
        };
        // Shape gate + provider exclusion (both immutable). Requires a
        // `messages` array and declines Anthropic-shaped bodies, which
        // also have one but reject `role:"system"` inside it — enhancing
        // there would corrupt the request. Declining here leaves the
        // engine's AnthropicMessagesRequestCodec to enhance into the
        // top-level `system` block list, and does NOT burn the
        // per-session idempotency flag.
        if parsed
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .is_none()
            || is_anthropic_shaped(&parsed)
        {
            return Ok(body);
        }

        // Idempotence gate — content-based (G0/G4a): skip when the
        // first message is already exactly our system directive.
        // Stateless, so it survives restarts, replicas, and replays
        // — and re-applies on every fresh client-built request,
        // which is required for the directive to be visible on the
        // turn's final round trip.
        let already_present = parsed
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .and_then(|m| m.first())
            .is_some_and(|first| {
                first.get("role").and_then(serde_json::Value::as_str) == Some("system")
                    && first.get("content").and_then(serde_json::Value::as_str)
                        == Some(self.directive.as_str())
            });
        if already_present {
            return Ok(body);
        }

        let messages = parsed
            .get_mut("messages")
            .and_then(|v| v.as_array_mut())
            .expect("messages array present — checked above");
        messages.insert(
            0,
            serde_json::json!({
                "role": "system",
                "content": self.directive,
            }),
        );

        Ok(serde_json::to_vec(&parsed)?.into())
    }

    fn discover(
        &self,
        _ctx: &DiscoverContext<'_>,
        _text: &str,
        _fields: &mut dyn FieldWriter,
    ) -> Result<(), BoxError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use noodle_core::{DiscardFieldWriter, Session, SessionKey};

    use super::*;

    fn session() -> Session {
        Session::new(
            SessionKey {
                auth_header: b"a",
                session_header: b"b",
            }
            .id(),
        )
    }

    fn ctx(session: &Session) -> EnhanceContext<'_> {
        EnhanceContext {
            provider: "openai",
            path: "/v1/chat/completions",
            session,
        }
    }

    fn parse_messages(body: &Bytes) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        v["messages"].as_array().cloned().unwrap_or_default()
    }

    // ── NoOp ────────────────────────────────────────────────────────

    #[test]
    fn noop_enhancer_returns_body_unchanged() {
        let s = session();
        let inj = NoOpEnhancer::new();
        let body = Bytes::from_static(b"{\"hi\":1}");
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn noop_enhancer_extracts_nothing() {
        let s = session();
        let ectx = DiscoverContext {
            provider: "openai",
            session: &s,
        };
        let inj = NoOpEnhancer::new();
        let mut fields = DiscardFieldWriter;
        inj.discover(&ectx, "anything", &mut fields).unwrap();
    }

    // ── OpenAI attribution ──────────────────────────────────────────

    #[test]
    fn enhances_system_message_at_position_zero() {
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from(br#"{"messages":[{"role":"user","content":"hi"}]}"#.as_slice());
        let out = inj.enhance(&ctx(&s), body).unwrap();
        let messages = parse_messages(&out);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "DIRECTIVE");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn second_call_in_same_session_is_a_noop() {
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from(br#"{"messages":[{"role":"user","content":"hi"}]}"#.as_slice());
        let first = inj.enhance(&ctx(&s), body.clone()).unwrap();
        let second = inj.enhance(&ctx(&s), first.clone()).unwrap();
        // Second call should NOT re-enhance the directive.
        let messages = parse_messages(&second);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "DIRECTIVE");
        // But more importantly — `second == first` byte-for-byte.
        assert_eq!(first, second);
    }

    #[test]
    fn distinct_sessions_each_enhance_once() {
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from(br#"{"messages":[{"role":"user","content":"hi"}]}"#.as_slice());
        let s1 = session();
        let s2 = Session::new(
            SessionKey {
                auth_header: b"x",
                session_header: b"y",
            }
            .id(),
        );
        let o1 = inj.enhance(&ctx(&s1), body.clone()).unwrap();
        let o2 = inj.enhance(&ctx(&s2), body.clone()).unwrap();
        for out in [&o1, &o2] {
            let messages = parse_messages(out);
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0]["role"], "system");
        }
    }

    #[test]
    fn non_json_body_passes_through() {
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from_static(b"plain text, not json");
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body);
        // And the per-session flag was NOT burned — a follow-up
        // OpenAI-shape request can still enhance.
        assert!(
            !s.directive_enhanced
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn anthropic_body_with_top_level_system_passes_through_untouched() {
        // Regression: the enhancer is shape-gated on `messages`, which
        // Anthropic also has. It must NOT enhance a `{"role":"system"}`
        // entry into an Anthropic conversation (Anthropic rejects that
        // role in `messages`); the engine's AnthropicMessagesRequestCodec
        // owns `system` enhancement for that path.
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from(
            br#"{"model":"claude-opus-4","system":[{"type":"text","text":"You are Claude Code."}],"messages":[{"role":"user","content":"hi"}]}"#
                .as_slice(),
        );
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body, "Anthropic body must be forwarded verbatim");
        // And the per-session flag was NOT burned — a later OpenAI-shape
        // request on the same session can still enhance.
        assert!(
            !s.directive_enhanced
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn anthropic_body_without_system_detected_by_content_blocks() {
        // The rarer Anthropic turn that omits `system` (e.g. a
        // tool-result follow-up) is still recognised via its
        // Anthropic-only content block types.
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from(
            br#"{"model":"claude-opus-4","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"...","signature":"abc"}]}]}"#
                .as_slice(),
        );
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body, "Anthropic tool/thinking turn forwarded verbatim");
        assert!(
            !s.directive_enhanced
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn json_without_messages_passes_through() {
        let s = session();
        let inj = OpenAiAttributionEnhancer::new("DIRECTIVE");
        let body = Bytes::from_static(br#"{"prompt":"hi","max_tokens":10}"#);
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body);
    }

    // ── ConfiguredAnthropicEnhancer (ADR 048 gap review R3) ─────────

    fn anthropic_enhancer(
        placement: noodle_core::config::context::Placement,
    ) -> ConfiguredAnthropicEnhancer {
        ConfiguredAnthropicEnhancer::new(vec![noodle_core::config::context::Enhancement {
            r#as: placement,
            text: "<system-reminder>VERBATIM OPERATOR TEXT</system-reminder>".into(),
            tags: Vec::new(),
        }])
    }

    fn anthropic_body() -> Bytes {
        Bytes::from_static(
            br#"{"model":"claude-opus-4-7","max_tokens":4096,"system":[{"type":"text","text":"agent"}],"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"future_field":{"keep":true}}"#,
        )
    }

    #[test]
    fn configured_enhancer_applies_verbatim_text_at_placement() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        let out = inj.enhance(&ctx(&s), anthropic_body()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["messages"][0]["content"][0]["text"],
            "<system-reminder>VERBATIM OPERATOR TEXT</system-reminder>",
            "operator text must land verbatim at the configured placement"
        );
        assert_eq!(v["future_field"]["keep"], true, "unknown fields survive");
    }

    #[test]
    fn configured_enhancer_is_content_idempotent() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        let once = inj.enhance(&ctx(&s), anthropic_body()).unwrap();
        let twice = inj.enhance(&ctx(&s), once.clone()).unwrap();
        assert_eq!(once, twice, "second pass must be byte-identical");
    }

    #[test]
    fn configured_enhancer_skips_quota_probe() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        let probe = Bytes::from_static(
            br#"{"model":"claude-haiku-4-5","max_tokens":1,"system":"x","messages":[{"role":"user","content":"quota"}]}"#,
        );
        let out = inj.enhance(&ctx(&s), probe.clone()).unwrap();
        assert_eq!(
            out, probe,
            "max_tokens == 1 preflight must pass through untouched"
        );
    }

    #[test]
    fn configured_enhancer_skips_monitor_side_call() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        // Security monitor / classifier: non-streaming, 64-token budget,
        // `<transcript>` wrapper text (captures/max/*.mitm). Injecting
        // here makes it terminate on max_tokens instead of stop_sequence.
        let monitor = Bytes::from_static(
            br#"{"model":"claude-haiku-4-5","max_tokens":64,"stop_sequences":["</block>"],"system":[{"type":"text","text":"agent"}],"messages":[{"role":"user","content":[{"type":"text","text":"<transcript>\nBash find ..."}]}]}"#,
        );
        let out = inj.enhance(&ctx(&s), monitor.clone()).unwrap();
        assert_eq!(
            out, monitor,
            "monitor side-call must pass through untouched"
        );
    }

    #[test]
    fn configured_enhancer_skips_tool_result_continuation() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        // The agent answering a prior tool_use is a continuation, not a
        // new prompt — never enhance (captures show this is the bulk of
        // mis-injected traffic).
        let cont = Bytes::from_static(
            br#"{"model":"claude-opus-4-7","max_tokens":64000,"system":[{"type":"text","text":"agent"}],"messages":[{"role":"user","content":[{"type":"text","text":"go"}]},{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#,
        );
        let out = inj.enhance(&ctx(&s), cont.clone()).unwrap();
        assert_eq!(
            out, cont,
            "tool-result continuation must pass through untouched"
        );
    }

    #[test]
    fn configured_enhancer_enhances_genuine_prompt_past_trailing_system() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        // A trailing role:system follows the user turn — ignored; the
        // genuine user-text prompt still gets the directive.
        let body = Bytes::from_static(
            br#"{"model":"claude-opus-4-7","max_tokens":32000,"system":[{"type":"text","text":"agent"}],"messages":[{"role":"user","content":[{"type":"text","text":"real prompt"}]},{"role":"system","content":"ctx"}]}"#,
        );
        let out = inj.enhance(&ctx(&s), body).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["messages"][0]["content"][0]["text"],
            "<system-reminder>VERBATIM OPERATOR TEXT</system-reminder>",
            "genuine prompt enhanced despite the trailing system message"
        );
    }

    #[test]
    fn configured_enhancer_declines_openai_shape() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        let openai = Bytes::from_static(
            br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let out = inj.enhance(&ctx(&s), openai.clone()).unwrap();
        assert_eq!(out, openai, "non-anthropic shapes pass through");
    }

    #[test]
    fn configured_enhancer_non_json_passes_through() {
        use noodle_core::config::context::Placement;
        let s = session();
        let inj = anthropic_enhancer(Placement::UserPrepend);
        let body = Bytes::from_static(b"not json at all");
        let out = inj.enhance(&ctx(&s), body.clone()).unwrap();
        assert_eq!(out, body);
    }
}
