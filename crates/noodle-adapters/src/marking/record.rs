//! ADR 052 §5 — stateless capture-side record (request side).
//!
//! Content-free per-round-trip signals built from the frame-identity headers
//! and the request body in isolation: no cross-request state, no
//! message-history hashing. Frame identity is read straight off the wire
//! (`x-claude-code-agent-id` for Claude Code, `x-session-id` /
//! `x-parent-session-id` for `OpenCode`), which is what lets §6 correlation run
//! server-side and replaces the `extends_root` / `message_sig` chain.
//!
//! This lands beside [`super::frame_tree`]; the stateful detector is unchanged.
//! All outputs are ids / fingerprints / enums — no prompt or response text is
//! retained.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The agent client that produced a round trip — selects the per-client
/// reading of the frame-identity headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureClient {
    ClaudeCode,
    OpenCode,
    Unknown,
}

/// Frame-identity headers, lifted off the request by the caller (the record
/// module never sees the raw `HeaderMap`). All optional: which are present is
/// exactly what distinguishes main vs sub-agent vs client.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameHeaders {
    /// `x-claude-code-session-id`
    pub cc_session_id: Option<String>,
    /// `x-claude-code-agent-id` — present only on a Claude Code sub-agent
    pub cc_agent_id: Option<String>,
    /// `x-session-id` — `OpenCode` frame id (its frame *is* a session)
    pub oc_session_id: Option<String>,
    /// `x-parent-session-id` — `OpenCode` parent frame, absent at the root
    pub oc_parent_session_id: Option<String>,
}

/// The request-side §5 record. Joined with the response-side signals
/// (`stop_reason`, `this_message_id`, `spawn_fps`, `tokens`) to form the full
/// content-free record the server correlates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    pub client: CaptureClient,
    /// Top-level grouping. `None` for `OpenCode` at the edge — the server derives
    /// it from the root frame of the parent chain (§5).
    pub session_id: Option<String>,
    /// The agent/thread this RT belongs to. `MAIN` for the Claude Code main
    /// frame; the agent id for a sub-agent; the session id for `OpenCode`.
    pub frame_id: String,
    /// The declared parent frame. `MAIN` for a CC sub-agent (the CC wire can't
    /// express deeper nesting); the parent session for `OpenCode`; `None` at root.
    pub parent_frame_id: Option<String>,
    /// Intra-frame chain link to the prior RT (`diagnostics.previous_message_id`).
    pub prev_message_id: Option<String>,
    /// Fingerprint of this RT's opening prompt; `None` on a continuation
    /// (a round trip whose last user message is a `tool_result`).
    pub open_fp: Option<String>,
    /// A round trip driven by no user prompt (quota / title-gen / monitor /
    /// compaction recap) — off-tree, belongs to no turn (§2).
    pub side_call: bool,
}

/// The Claude Code main frame id, and a CC sub-agent's declared parent.
pub const MAIN: &str = "MAIN";

fn sha12(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..12].to_string()
}

/// Leading user text of the last user message; `None` when that message is a
/// `tool_result` (a continuation) or carries no text block.
fn last_user_text(v: &Value) -> Option<String> {
    let msgs = v.get("messages")?.as_array()?;
    let content = msgs
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))?
        .get("content")?;
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => arr
            .iter()
            .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|b| b.get("text").and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

/// Harness wrappers + the compaction recap whose presence marks a side call.
fn is_side_call_text(t: &str) -> bool {
    let s = t.trim_start();
    s.starts_with("<transcript>")
        || s.starts_with("[SUGGESTION MODE")
        || s.starts_with("<session>")
        || s.starts_with("The user stepped away and is coming back. Recap")
}

/// Build the request-side §5 record from the frame headers and the request body.
/// Pure and stateless: depends only on this one request.
#[must_use]
pub fn request_record(h: &FrameHeaders, body: &[u8]) -> RequestRecord {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);

    let client = if h.cc_session_id.is_some() {
        CaptureClient::ClaudeCode
    } else if h.oc_session_id.is_some() {
        CaptureClient::OpenCode
    } else {
        CaptureClient::Unknown
    };

    let (session_id, frame_id, parent_frame_id) = match client {
        CaptureClient::ClaudeCode => {
            // agent id present ⟹ sub-agent (parent is main); absent ⟹ MAIN.
            let frame = h.cc_agent_id.clone().unwrap_or_else(|| MAIN.to_string());
            let parent = h.cc_agent_id.as_ref().map(|_| MAIN.to_string());
            (h.cc_session_id.clone(), frame, parent)
        }
        CaptureClient::OpenCode => {
            // frame id *is* the session id; the server fills session from the
            // root of the parent chain, so the edge leaves it null.
            let frame = h.oc_session_id.clone().unwrap_or_else(|| MAIN.to_string());
            (None, frame, h.oc_parent_session_id.clone())
        }
        CaptureClient::Unknown => (None, MAIN.to_string(), None),
    };

    let prev_message_id = v
        .get("diagnostics")
        .and_then(|d| d.get("previous_message_id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let trailing = last_user_text(&v);
    let open_fp = trailing
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(sha12);

    let max_tokens = v.get("max_tokens").and_then(Value::as_u64);
    let side_call = matches!(max_tokens, Some(mt) if mt <= 1)
        || trailing.as_deref().is_some_and(is_side_call_text);

    RequestRecord {
        client,
        session_id,
        frame_id,
        parent_frame_id,
        prev_message_id,
        open_fp,
        side_call,
    }
}

/// Per-round-trip usage, decomposed so the carried context (`cache_read`) is
/// distinguishable from the marginal new prompt (`input`) — ADR 056.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

/// The response-side §5 signals, reassembled from the SSE stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseRecord {
    /// `end_turn` closes a turn; `tool_use` continues it. `None` when the
    /// response dies before a terminal delta (treated as non-terminal, §4).
    pub stop_reason: Option<String>,
    /// Chain target — what the next RT's `prev_message_id` points at.
    pub this_message_id: Option<String>,
    /// Fingerprint per sub-agent prompt this RT spawned (name-free: any
    /// `tool_use` whose input carries a string `prompt`).
    pub spawn_fps: Vec<String>,
    pub tokens: Tokens,
}

fn merge_usage(t: &mut Tokens, u: &Value) {
    let get = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    // SSE splits usage across message_start (input/cache) and message_delta
    // (output); take the max per field so neither overwrites the other with 0.
    t.input = t.input.max(get("input_tokens"));
    t.output = t.output.max(get("output_tokens"));
    t.cache_read = t.cache_read.max(get("cache_read_input_tokens"));
    t.cache_creation = t.cache_creation.max(get("cache_creation_input_tokens"));
}

/// Reassemble the response-side §5 signals from a raw Anthropic SSE stream
/// (`event:`/`data:` lines). Pure and stateless; reads no prompt or response
/// text beyond fingerprinting spawned prompts.
#[must_use]
pub fn response_record(sse: &[u8]) -> ResponseRecord {
    let text = String::from_utf8_lossy(sse);
    let mut out = ResponseRecord::default();
    // Per content-block index: whether it is a tool_use, plus any prompt found
    // inline at start, and the accumulated `input_json_delta` fragments.
    let mut tool_idx: BTreeMap<u64, Option<String>> = BTreeMap::new();
    let mut partial: BTreeMap<u64, String> = BTreeMap::new();

    for line in text.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(m) = v.get("message") {
                    if out.this_message_id.is_none() {
                        out.this_message_id =
                            m.get("id").and_then(Value::as_str).map(str::to_string);
                    }
                    if let Some(u) = m.get("usage") {
                        merge_usage(&mut out.tokens, u);
                    }
                }
            }
            Some("message_delta") => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    out.stop_reason = Some(sr.to_string());
                }
                if let Some(u) = v.get("usage") {
                    merge_usage(&mut out.tokens, u);
                }
            }
            Some("content_block_start") => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let cb = v.get("content_block");
                if cb.and_then(|c| c.get("type")).and_then(Value::as_str) == Some("tool_use") {
                    let inline = cb
                        .and_then(|c| c.get("input"))
                        .and_then(|i| i.get("prompt"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    tool_idx.insert(idx, inline);
                }
            }
            Some("content_block_delta")
                if v.get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str)
                    == Some("input_json_delta") =>
            {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let frag = v
                    .get("delta")
                    .and_then(|d| d.get("partial_json"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                partial.entry(idx).or_default().push_str(frag);
            }
            _ => {}
        }
    }

    for (idx, inline) in tool_idx {
        let from_delta = partial
            .get(&idx)
            .filter(|s| !s.is_empty())
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|v| v.get("prompt").and_then(Value::as_str).map(str::to_string));
        if let Some(prompt) = from_delta.or(inline) {
            out.spawn_fps.push(sha12(&prompt));
        }
    }
    out
}

/// The full content-free §5 record for one round trip — request side joined
/// with response side. This is what the server (§6) correlates into the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRecord {
    pub client: CaptureClient,
    pub session_id: Option<String>,
    pub frame_id: String,
    pub parent_frame_id: Option<String>,
    pub prev_message_id: Option<String>,
    pub this_message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub open_fp: Option<String>,
    pub spawn_fps: Vec<String>,
    pub side_call: bool,
    pub tokens: Tokens,
}

impl CaptureRecord {
    /// Join the request- and response-side signals into one record.
    #[must_use]
    pub fn assemble(req: RequestRecord, resp: ResponseRecord) -> Self {
        Self {
            client: req.client,
            session_id: req.session_id,
            frame_id: req.frame_id,
            parent_frame_id: req.parent_frame_id,
            prev_message_id: req.prev_message_id,
            this_message_id: resp.this_message_id,
            stop_reason: resp.stop_reason,
            open_fp: req.open_fp,
            spawn_fps: resp.spawn_fps,
            side_call: req.side_call,
            tokens: resp.tokens,
        }
    }

    /// Number of sub-agents this round trip spawned (marks the spawning RT).
    #[must_use]
    pub fn n_spawn(&self) -> usize {
        self.spawn_fps.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_main_frame_has_no_agent_id() {
        let h = FrameHeaders {
            cc_session_id: Some("sess-1".into()),
            ..Default::default()
        };
        let body = br#"{"messages":[{"role":"user","content":"do a thing"}]}"#;
        let r = request_record(&h, body);
        assert_eq!(r.client, CaptureClient::ClaudeCode);
        assert_eq!(r.frame_id, "MAIN");
        assert_eq!(r.parent_frame_id, None);
        assert_eq!(r.session_id.as_deref(), Some("sess-1"));
        assert!(r.open_fp.is_some());
        assert!(!r.side_call);
    }

    #[test]
    fn claude_subagent_frame_is_the_agent_id() {
        let h = FrameHeaders {
            cc_session_id: Some("sess-1".into()),
            cc_agent_id: Some("agent-abc".into()),
            ..Default::default()
        };
        let body = br#"{"messages":[{"role":"user","content":"sub task"}]}"#;
        let r = request_record(&h, body);
        assert_eq!(r.frame_id, "agent-abc");
        assert_eq!(r.parent_frame_id.as_deref(), Some("MAIN"));
    }

    #[test]
    fn continuation_has_no_open_fp() {
        let h = FrameHeaders {
            cc_session_id: Some("sess-1".into()),
            ..Default::default()
        };
        let body = br#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1"}]}]}"#;
        let r = request_record(&h, body);
        assert_eq!(r.open_fp, None);
    }

    #[test]
    fn quota_probe_and_wrappers_are_side_calls() {
        let h = FrameHeaders {
            cc_session_id: Some("s".into()),
            ..Default::default()
        };
        assert!(
            request_record(
                &h,
                br#"{"max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#
            )
            .side_call
        );
        assert!(
            request_record(
                &h,
                br#"{"messages":[{"role":"user","content":"<transcript>\nhi"}]}"#
            )
            .side_call
        );
        assert!(request_record(&h, br#"{"messages":[{"role":"user","content":"The user stepped away and is coming back. Recap"}]}"#).side_call);
    }

    #[test]
    fn opencode_frame_is_session_with_explicit_parent() {
        let root = FrameHeaders {
            oc_session_id: Some("ses_root".into()),
            ..Default::default()
        };
        let r = request_record(&root, br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(r.client, CaptureClient::OpenCode);
        assert_eq!(r.frame_id, "ses_root");
        assert_eq!(r.parent_frame_id, None);
        assert_eq!(r.session_id, None);

        let child = FrameHeaders {
            oc_session_id: Some("ses_child".into()),
            oc_parent_session_id: Some("ses_root".into()),
            ..Default::default()
        };
        let c = request_record(&child, br#"{"messages":[{"role":"user","content":"sub"}]}"#);
        assert_eq!(c.frame_id, "ses_child");
        assert_eq!(c.parent_frame_id.as_deref(), Some("ses_root"));
    }

    #[test]
    fn response_record_reads_stop_spawns_and_split_usage() {
        let sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_x","usage":{"input_tokens":100,"cache_read_input_tokens":50,"cache_creation_input_tokens":9}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"Task","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"prompt\":\"do x\"}"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}

event: message_stop
data: {"type":"message_stop"}
"#;
        let r = response_record(sse.as_bytes());
        assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(r.this_message_id.as_deref(), Some("msg_x"));
        assert_eq!(r.spawn_fps, vec![sha12("do x")]);
        // usage is split across message_start (input/cache) and message_delta (output)
        assert_eq!(r.tokens.input, 100);
        assert_eq!(r.tokens.output, 20);
        assert_eq!(r.tokens.cache_read, 50);
        assert_eq!(r.tokens.cache_creation, 9);
    }

    #[test]
    fn assemble_joins_request_and_response() {
        let req = request_record(
            &FrameHeaders {
                cc_session_id: Some("s".into()),
                ..Default::default()
            },
            br#"{"messages":[{"role":"user","content":"go"}]}"#,
        );
        let resp = ResponseRecord {
            stop_reason: Some("end_turn".into()),
            this_message_id: Some("msg_1".into()),
            spawn_fps: vec!["fp1".into(), "fp2".into()],
            tokens: Tokens {
                input: 5,
                output: 3,
                ..Default::default()
            },
        };
        let rec = CaptureRecord::assemble(req, resp);
        assert_eq!(rec.frame_id, "MAIN");
        assert_eq!(rec.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(rec.this_message_id.as_deref(), Some("msg_1"));
        assert_eq!(rec.n_spawn(), 2);
        assert_eq!(rec.tokens.input, 5);
    }
}
