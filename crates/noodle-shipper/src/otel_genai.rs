//! ADR 052 §10 — map a correlated round trip onto OpenTelemetry GenAI semantic
//! conventions. Option (a): the semconv **vocabulary + attribute keys** on the
//! shipped record.
//!
//! The trace tree is our §6 reconstruction, named in GenAI terms:
//! - turn → one trace (`trace_id`)
//! - frame → a span; the agent frame is an `invoke_agent` span, parented by
//!   `parent_frame_id`
//! - round trip → a child `chat` span (the leaf LLM call)
//! - usage → `gen_ai.usage.*` attributes, rolled up by trace then `session.id`
//!
//! Per the §10 caveat we **mint** the grouping from §6 ids (we do not read W3C
//! `traceparent` off the wire); native 16-byte OTLP ids are option (b),
//! deferred. Side-calls are off-tree (§2) and are not mapped here.
//!
//! This module is a pure transform with no I/O; wiring it into the OTLP export
//! path is a separate step.

use std::collections::BTreeSet;

/// The GenAI SemConv version these attribute keys track (§10 version pinning).
pub const GENAI_SEMCONV_VERSION: &str = "1.37";

/// The §6 role of a round trip, as it maps onto a GenAI span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Depth-0 main agent.
    Main,
    /// A sub-agent frame.
    SubAgent,
}

/// The correlated round trip the shipper hands to the GenAI mapper. Carries the
/// §6 marks plus the usage/provider facts needed for the attribute set.
#[derive(Clone, Debug)]
pub struct CorrelatedRoundTrip {
    pub session_id: String,
    /// The turn — becomes the `trace_id`.
    pub turn_id: String,
    /// The agent frame this round trip runs in (`ROOT` for main).
    pub frame_id: String,
    /// The frame that spawned this one; `None` for the root frame.
    pub parent_frame_id: Option<String>,
    pub depth: u32,
    pub role: Role,
    /// This round trip's response message id — the leaf span id.
    pub message_id: String,
    /// Provider name, e.g. `anthropic`.
    pub provider: String,
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// Business-context classification (§5 `activity`), if assigned.
    pub activity: Option<String>,
}

/// A GenAI-shaped span: its place in the trace tree plus the semconv attribute
/// set. `trace_id`/`span_id`/`parent_span_id` are §6 ids used as logical
/// grouping keys (option a), not native OTLP hex (option b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenAiSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    /// `gen_ai.operation.name`.
    pub operation: &'static str,
    /// Semconv-named `(key, value)` attribute pairs.
    pub attributes: Vec<(&'static str, String)>,
}

/// The leaf `chat` span for one round trip — a child of its agent frame span.
#[must_use]
pub fn chat_span(rt: &CorrelatedRoundTrip) -> GenAiSpan {
    let mut attributes = vec![
        ("gen_ai.operation.name", "chat".to_string()),
        ("gen_ai.provider.name", rt.provider.clone()),
        ("session.id", rt.session_id.clone()),
        ("gen_ai.usage.input_tokens", rt.input_tokens.to_string()),
        ("gen_ai.usage.output_tokens", rt.output_tokens.to_string()),
    ];
    if rt.cache_read_tokens > 0 {
        attributes.push((
            "gen_ai.usage.cache_read_input_tokens",
            rt.cache_read_tokens.to_string(),
        ));
    }
    if let Some(m) = &rt.model {
        attributes.push(("gen_ai.request.model", m.clone()));
    }
    if let Some(a) = &rt.activity {
        attributes.push(("noodle.activity", a.clone()));
    }
    GenAiSpan {
        trace_id: rt.turn_id.clone(),
        span_id: rt.message_id.clone(),
        parent_span_id: Some(rt.frame_id.clone()),
        operation: "chat",
        attributes,
    }
}

/// The agent-frame `invoke_agent` span the round-trip spans hang under. One per
/// frame; the root frame (main) has no parent span.
#[must_use]
pub fn agent_span(rt: &CorrelatedRoundTrip) -> GenAiSpan {
    let agent_name = match rt.role {
        Role::Main => "main",
        Role::SubAgent => "sub_agent",
    };
    GenAiSpan {
        trace_id: rt.turn_id.clone(),
        span_id: rt.frame_id.clone(),
        parent_span_id: rt.parent_frame_id.clone(),
        operation: "invoke_agent",
        attributes: vec![
            ("gen_ai.operation.name", "invoke_agent".to_string()),
            ("gen_ai.agent.id", rt.frame_id.clone()),
            ("gen_ai.agent.name", agent_name.to_string()),
            ("session.id", rt.session_id.clone()),
        ],
    }
}

/// All spans for one turn — the GenAI trace. The shipper groups round trips by
/// `turn_id` and hands each group here; the exporter serializes the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    pub trace_id: String,
    pub session_id: String,
    pub spans: Vec<GenAiSpan>,
}

/// Assemble one turn's round trips into its trace: one `invoke_agent` span per
/// distinct frame (in first-seen order), then one `chat` span per round trip.
/// Returns `None` for empty input. Callers pass round trips already grouped by
/// `turn_id`.
#[must_use]
pub fn assemble_trace(rts: &[CorrelatedRoundTrip]) -> Option<Trace> {
    let first = rts.first()?;
    let mut spans = Vec::with_capacity(rts.len() * 2);
    let mut seen = BTreeSet::new();
    for rt in rts {
        if seen.insert(rt.frame_id.as_str()) {
            spans.push(agent_span(rt));
        }
    }
    for rt in rts {
        spans.push(chat_span(rt));
    }
    Some(Trace {
        trace_id: first.turn_id.clone(),
        session_id: first.session_id.clone(),
        spans,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> CorrelatedRoundTrip {
        CorrelatedRoundTrip {
            session_id: "sess-1".into(),
            turn_id: "turn-1".into(),
            frame_id: "ROOT".into(),
            parent_frame_id: None,
            depth: 0,
            role: Role::Main,
            message_id: "msg_1".into(),
            provider: "anthropic".into(),
            model: Some("claude-opus-4-8".into()),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 865,
            activity: Some("feature-development".into()),
        }
    }

    fn attr<'a>(s: &'a GenAiSpan, k: &str) -> Option<&'a str> {
        s.attributes.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn chat_span_is_a_leaf_under_its_frame() {
        let s = chat_span(&rt());
        assert_eq!(s.operation, "chat");
        assert_eq!(s.trace_id, "turn-1"); // turn = trace
        assert_eq!(s.span_id, "msg_1");
        assert_eq!(s.parent_span_id.as_deref(), Some("ROOT"));
        assert_eq!(attr(&s, "session.id"), Some("sess-1"));
        assert_eq!(attr(&s, "gen_ai.provider.name"), Some("anthropic"));
        assert_eq!(attr(&s, "gen_ai.usage.input_tokens"), Some("100"));
        assert_eq!(attr(&s, "gen_ai.usage.output_tokens"), Some("20"));
        assert_eq!(attr(&s, "gen_ai.usage.cache_read_input_tokens"), Some("865"));
        assert_eq!(attr(&s, "gen_ai.request.model"), Some("claude-opus-4-8"));
        assert_eq!(attr(&s, "noodle.activity"), Some("feature-development"));
    }

    #[test]
    fn main_agent_span_is_a_trace_root() {
        let s = agent_span(&rt());
        assert_eq!(s.operation, "invoke_agent");
        assert_eq!(s.span_id, "ROOT");
        assert_eq!(s.parent_span_id, None, "main frame is the trace root");
        assert_eq!(attr(&s, "gen_ai.agent.name"), Some("main"));
    }

    #[test]
    fn subagent_span_parents_to_its_frame() {
        let mut r = rt();
        r.role = Role::SubAgent;
        r.frame_id = "agent-xyz".into();
        r.parent_frame_id = Some("ROOT".into());
        r.depth = 1;
        let span = agent_span(&r);
        assert_eq!(span.span_id, "agent-xyz");
        assert_eq!(span.parent_span_id.as_deref(), Some("ROOT"));
        assert_eq!(attr(&span, "gen_ai.agent.name"), Some("sub_agent"));
        // its chat round-trips hang under the sub-agent frame
        assert_eq!(chat_span(&r).parent_span_id.as_deref(), Some("agent-xyz"));
    }

    #[test]
    fn cache_read_omitted_when_zero() {
        let mut r = rt();
        r.cache_read_tokens = 0;
        assert_eq!(attr(&chat_span(&r), "gen_ai.usage.cache_read_input_tokens"), None);
    }

    #[test]
    fn semconv_version_is_pinned() {
        assert_eq!(GENAI_SEMCONV_VERSION, "1.37");
    }

    #[test]
    fn assemble_trace_builds_one_agent_span_per_frame_and_one_chat_per_rt() {
        let main_rt0 = rt(); // ROOT main, msg_1
        let mut sub = rt();
        sub.role = Role::SubAgent;
        sub.frame_id = "agent-xyz".into();
        sub.parent_frame_id = Some("ROOT".into());
        sub.depth = 1;
        sub.message_id = "msg_2".into();
        let mut main_rt1 = rt();
        main_rt1.message_id = "msg_3".into();

        let trace = assemble_trace(&[main_rt0, sub, main_rt1]).unwrap();
        assert_eq!(trace.trace_id, "turn-1");
        assert_eq!(trace.session_id, "sess-1");
        // 2 distinct frames (ROOT, agent-xyz) → 2 invoke_agent spans; 3 chat spans
        let agents = trace.spans.iter().filter(|s| s.operation == "invoke_agent").count();
        let chats = trace.spans.iter().filter(|s| s.operation == "chat").count();
        assert_eq!(agents, 2, "one invoke_agent span per distinct frame");
        assert_eq!(chats, 3, "one chat span per round trip");
        // the sub-agent frame parents to ROOT
        let sub_span = trace.spans.iter().find(|s| s.span_id == "agent-xyz").unwrap();
        assert_eq!(sub_span.parent_span_id.as_deref(), Some("ROOT"));
    }

    #[test]
    fn assemble_trace_empty_is_none() {
        assert!(assemble_trace(&[]).is_none());
    }
}
