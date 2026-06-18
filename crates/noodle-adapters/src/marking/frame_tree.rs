//! ADR 052 §6 — per-session turn & frame marking, header-driven.
//!
//! Frame identity is read off the wire, never reconstructed from content:
//!
//! - **Frame** — `x-claude-code-agent-id` present ⇒ a sub-agent frame parented
//!   to the main agent (`ROOT`, depth 1); absent ⇒ the main frame (`ROOT`,
//!   depth 0). (`OpenCode`'s session/parent headers map the same way upstream of
//!   this detector.)
//! - **Turn** — opens on the first main round-trip after the previous turn
//!   closed; closes when the main frame stops at a terminal `stop_reason`. A
//!   sub-agent's own `end_turn` is its return, not the turn's end. Sub-agents
//!   inherit the open turn.
//! - **Side-call** — a round-trip driven by no user prompt (quota probe,
//!   harness wrapper, compaction recap); off-tree, no frame, no turn.
//!
//! This replaces the prior content-fingerprint reconstruction
//! (`extends_root` / `message_sig` / spawn-prompt matching), which was fragile
//! under compaction and prompt-wrapping. The request-side inputs are the §5
//! [`crate::marking::record`] signals; the per-round-trip output [`FrameMarks`]
//! and the `on_request_open` / `on_response_close` lifecycle are unchanged, so
//! the sink + viewer contract does not move.
//!
//! State is **per session**: [`FrameTreeDetector`] is the single-session
//! algorithm; [`FrameTreeRegistry`] partitions one detector per `session_id`
//! (ADR 052 §8 — no cross-session turn leakage).

use dashmap::DashMap;
use noodle_core::MarkingSessionId;
use ulid::Ulid;

/// The role a round-trip plays in the session's frame tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRole {
    /// The depth-0 main agent (`frame_id == "ROOT"`).
    Main,
    /// A sub-agent frame (`frame_id == x-claude-code-agent-id`).
    SubAgent,
    /// Off-tree harness call (quota, title-gen, monitor, suggestion, compaction
    /// recap) — no turn, no place in the tree.
    SideCall,
}

impl FrameRole {
    /// Wire spelling used in the §5 marks / goldens.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::SubAgent => "sub_agent",
            Self::SideCall => "side_call",
        }
    }
}

/// The §5 marks produced for one round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameMarks {
    pub role: FrameRole,
    /// `"ROOT"` for the main agent; the agent id for a sub-agent; `None` for a
    /// side-call.
    pub frame_id: Option<String>,
    /// The frame that spawned this one; `None` for ROOT and side-calls.
    pub parent_frame_id: Option<String>,
    /// 0 = main; 1 = sub-agent; `None` for side-calls.
    pub depth: Option<u32>,
    /// The top-level turn this round-trip belongs to; `None` for side-calls.
    pub turn_id: Option<String>,
}

/// One `tool_use` block emitted in a response. Retained on the response-side
/// signals as the §5 spawn record (server-side correlation refinement); the
/// detector's frame decision no longer depends on it.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub id: String,
    /// SHA-256 of a spawn's `input.prompt`, when present.
    pub prompt_sha256: Option<String>,
}

/// Request-side §5 signals, known at request open — the inputs to frame
/// classification. Hashes / ids / enums only, no text.
#[derive(Debug, Clone, Default)]
pub struct RequestSignals {
    pub max_tokens: Option<u64>,
    /// `"session" | "transcript" | "suggestion" | "none"` — trailing-text
    /// harness-wrapper classification (a side-call signal).
    pub trailing_wrapper_kind: String,
    /// `x-claude-code-agent-id` (or the `OpenCode` equivalent), lifted from the
    /// request headers by the proxy. `Some` ⇒ a sub-agent frame; `None` ⇒ main.
    pub agent_id: Option<String>,
    /// A round-trip driven by no user prompt (quota probe / harness wrapper /
    /// compaction recap), computed content-free from the request (§5).
    pub side_call: bool,
}

/// Response-side §5 signals, known at response close — the input to turn-close.
#[derive(Debug, Clone, Default)]
pub struct ResponseSignals {
    /// Wire `stop_reason`, if observed.
    pub stop_reason: Option<String>,
    /// `tool_use` blocks emitted in the response (§5 spawn record).
    pub response_tool_uses: Vec<ToolUse>,
}

/// Outcome of [`FrameTreeDetector::on_request_open`]: the §5 marks for this
/// round-trip, plus whether it is the main frame (drives turn-close at the
/// matching [`FrameTreeDetector::on_response_close`]).
#[derive(Debug, Clone)]
pub struct OpenOutcome {
    /// The §5 marks to stamp on this round-trip.
    pub marks: FrameMarks,
    /// Whether this round-trip is the main (depth-0) frame.
    is_main: bool,
}

/// The full per-round-trip signals (request + response), for whole-capture
/// replay / tests. The proxy uses the [`RequestSignals`] / [`ResponseSignals`]
/// split instead, since it learns the two halves at different lifecycle points.
#[derive(Debug, Clone, Default)]
pub struct RoundTripSignals {
    pub max_tokens: Option<u64>,
    pub trailing_wrapper_kind: String,
    pub agent_id: Option<String>,
    pub side_call: bool,
    pub stop_reason: Option<String>,
    pub response_tool_uses: Vec<ToolUse>,
}

const ROOT: &str = "ROOT";

/// Terminal stop reasons that close a depth-0 turn.
fn is_terminal(stop: Option<&str>) -> bool {
    matches!(stop, Some("end_turn" | "max_tokens" | "stop_sequence"))
}

/// Whether the trailing-wrapper kind marks a harness wrapper (`""`/`"none"`
/// are not wrappers; anything else is).
fn is_wrapper(kind: &str) -> bool {
    !kind.is_empty() && kind != "none"
}

/// Default turn-id mint: a fresh ULID per turn (globally unique across
/// sessions, so two sessions' first turns never collide).
fn ulid_turn_id(_ordinal: u32) -> String {
    Ulid::new().to_string()
}

/// Per-session turn state. One instance per `session_id`. Frame identity is
/// stateless (read off each request's header), so the only state is the open
/// turn.
#[derive(Debug)]
pub struct FrameTreeDetector {
    /// Whether a turn is currently open (closed by the main frame's terminal).
    in_turn: bool,
    /// 1-based ordinal of turns opened in this session (passed to `mint_turn`).
    turn: u32,
    /// The id minted when the current turn opened, reused on every round-trip of
    /// that turn (main + sub-agents). `None` before the first turn.
    current_turn_id: Option<String>,
    /// Turn-id minter, called once per turn open. Defaults to [`ulid_turn_id`];
    /// tests inject a deterministic counter via [`Self::with_turn_mint`].
    mint_turn: fn(u32) -> String,
}

impl Default for FrameTreeDetector {
    fn default() -> Self {
        Self {
            in_turn: false,
            turn: 0,
            current_turn_id: None,
            mint_turn: ulid_turn_id,
        }
    }
}

impl FrameTreeDetector {
    /// Detector minting a fresh ULID per turn (production default).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Detector with a caller-supplied turn-id mint — for deterministic tests
    /// (e.g. `|n| format!("turn-{n}")`). The mint receives the 1-based turn
    /// ordinal so a counter-style id is possible.
    #[must_use]
    pub fn with_turn_mint(mint: fn(u32) -> String) -> Self {
        Self {
            mint_turn: mint,
            ..Self::default()
        }
    }

    /// Classify a round-trip from its **request** signals and produce its §5
    /// marks. Frame identity is the `agent_id` header; the turn is the span
    /// between two main-frame terminals. Pair every call with
    /// [`Self::on_response_close`] using the returned [`OpenOutcome`].
    pub fn on_request_open(&mut self, req: &RequestSignals) -> OpenOutcome {
        // Side-call: no user prompt drives it. Off-tree.
        if req.side_call || req.max_tokens == Some(1) || is_wrapper(&req.trailing_wrapper_kind) {
            return OpenOutcome {
                marks: FrameMarks {
                    role: FrameRole::SideCall,
                    frame_id: None,
                    parent_frame_id: None,
                    depth: None,
                    turn_id: None,
                },
                is_main: false,
            };
        }

        // Frame identity is read off the wire: an agent id ⇒ a sub-agent frame
        // parented to main; its absence ⇒ the main frame (ROOT).
        let (role, frame_id, parent_frame_id, depth) = match req.agent_id.as_deref() {
            Some(agent) => (
                FrameRole::SubAgent,
                agent.to_string(),
                Some(ROOT.to_string()),
                1,
            ),
            None => (FrameRole::Main, ROOT.to_string(), None, 0),
        };

        // A turn opens on the first main round-trip after the previous closed;
        // sub-agents inherit the open turn.
        if role == FrameRole::Main && !self.in_turn {
            self.turn += 1;
            self.current_turn_id = Some((self.mint_turn)(self.turn));
            self.in_turn = true;
        }

        OpenOutcome {
            marks: FrameMarks {
                role,
                frame_id: Some(frame_id),
                parent_frame_id,
                depth: Some(depth),
                turn_id: self.current_turn_id.clone(),
            },
            is_main: role == FrameRole::Main,
        }
    }

    /// Close the turn when the **main** frame stops at a terminal `stop_reason`.
    /// A sub-agent's own terminal is its return, not the turn's end. No-op for a
    /// side-call.
    pub fn on_response_close(&mut self, outcome: &OpenOutcome, resp: &ResponseSignals) {
        if outcome.is_main && is_terminal(resp.stop_reason.as_deref()) {
            self.in_turn = false;
        }
    }

    /// Whole round-trip in one call (request open then response close). For
    /// replay / tests where both halves are known together.
    pub fn on_round_trip(&mut self, rt: &RoundTripSignals) -> FrameMarks {
        let req = RequestSignals {
            max_tokens: rt.max_tokens,
            trailing_wrapper_kind: rt.trailing_wrapper_kind.clone(),
            agent_id: rt.agent_id.clone(),
            side_call: rt.side_call,
        };
        let outcome = self.on_request_open(&req);
        let resp = ResponseSignals {
            stop_reason: rt.stop_reason.clone(),
            response_tool_uses: rt.response_tool_uses.clone(),
        };
        self.on_response_close(&outcome, &resp);
        outcome.marks
    }
}

/// Per-session partitioning over [`FrameTreeDetector`] (ADR 052 §8). Holds one
/// detector per `session_id` so concurrent sessions never share turn state.
/// Thread-safe; clone-free hot path via [`DashMap`].
#[derive(Debug, Default)]
pub struct FrameTreeRegistry {
    sessions: DashMap<MarkingSessionId, FrameTreeDetector>,
}

impl FrameTreeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify a round-trip's request within its session, creating the
    /// session's detector on first sighting. Pair with [`Self::on_response_close`].
    #[must_use]
    pub fn on_request_open(
        &self,
        session_id: &MarkingSessionId,
        req: &RequestSignals,
    ) -> OpenOutcome {
        self.sessions
            .entry(session_id.clone())
            .or_default()
            .on_request_open(req)
    }

    /// Fold a round-trip's response into its session's turn state. No-op if the
    /// session was never opened.
    pub fn on_response_close(
        &self,
        session_id: &MarkingSessionId,
        outcome: &OpenOutcome,
        resp: &ResponseSignals,
    ) {
        if let Some(mut detector) = self.sessions.get_mut(session_id) {
            detector.on_response_close(outcome, resp);
        }
    }

    /// Whole round-trip in one call, isolated per `session_id`.
    #[must_use]
    pub fn on_round_trip(
        &self,
        session_id: &MarkingSessionId,
        rt: &RoundTripSignals,
    ) -> FrameMarks {
        self.sessions
            .entry(session_id.clone())
            .or_default()
            .on_round_trip(rt)
    }

    /// Number of sessions currently tracked (eviction is the caller's concern).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_prompt(stop: &str) -> RoundTripSignals {
        RoundTripSignals {
            max_tokens: Some(64000),
            trailing_wrapper_kind: "none".into(),
            stop_reason: Some(stop.into()),
            ..Default::default()
        }
    }

    #[test]
    fn main_prompt_is_root_main_turn1() {
        let mut d = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
        let m = d.on_round_trip(&main_prompt("end_turn"));
        assert_eq!(m.role, FrameRole::Main);
        assert_eq!(m.frame_id.as_deref(), Some("ROOT"));
        assert_eq!(m.parent_frame_id, None);
        assert_eq!(m.depth, Some(0));
        assert_eq!(m.turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn agent_id_makes_a_subagent_frame_in_the_open_turn() {
        let mut d = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
        // main opens turn-1 (tool_use → stays open, it spawned)
        let main = d.on_round_trip(&main_prompt("tool_use"));
        assert_eq!(main.turn_id.as_deref(), Some("turn-1"));
        // sub-agent round-trip carries the agent id, inherits turn-1
        let sub = d.on_round_trip(&RoundTripSignals {
            agent_id: Some("agent-xyz".into()),
            trailing_wrapper_kind: "none".into(),
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        });
        assert_eq!(sub.role, FrameRole::SubAgent);
        assert_eq!(sub.frame_id.as_deref(), Some("agent-xyz"));
        assert_eq!(sub.parent_frame_id.as_deref(), Some("ROOT"));
        assert_eq!(sub.depth, Some(1));
        assert_eq!(
            sub.turn_id.as_deref(),
            Some("turn-1"),
            "sub-agent inherits the turn"
        );
    }

    #[test]
    fn subagent_end_turn_does_not_close_the_main_turn() {
        let mut d = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
        d.on_round_trip(&main_prompt("tool_use")); // open turn-1, still open
        d.on_round_trip(&RoundTripSignals {
            agent_id: Some("a1".into()),
            trailing_wrapper_kind: "none".into(),
            stop_reason: Some("end_turn".into()), // sub-agent returns
            ..Default::default()
        });
        // main resumes — still turn-1, not a new turn
        let main2 = d.on_round_trip(&main_prompt("tool_use"));
        assert_eq!(main2.turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn turn_closes_on_main_terminal_next_main_opens_turn2() {
        let mut d = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
        let t1 = d.on_round_trip(&main_prompt("end_turn"));
        let t2 = d.on_round_trip(&main_prompt("end_turn"));
        assert_eq!(t1.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(t2.turn_id.as_deref(), Some("turn-2"));
    }

    #[test]
    fn quota_and_wrappers_are_side_calls() {
        let mut d = FrameTreeDetector::new();
        let quota = d.on_round_trip(&RoundTripSignals {
            max_tokens: Some(1),
            trailing_wrapper_kind: "none".into(),
            ..Default::default()
        });
        assert_eq!(quota.role, FrameRole::SideCall);
        assert!(quota.turn_id.is_none());

        let monitor = d.on_round_trip(&RoundTripSignals {
            trailing_wrapper_kind: "transcript".into(),
            ..Default::default()
        });
        assert_eq!(monitor.role, FrameRole::SideCall);

        let recap = d.on_round_trip(&RoundTripSignals {
            trailing_wrapper_kind: "none".into(),
            side_call: true, // recap, flagged by §5 record
            ..Default::default()
        });
        assert_eq!(recap.role, FrameRole::SideCall);
    }

    #[test]
    fn default_turn_id_is_a_unique_ulid() {
        let a = FrameTreeDetector::new().on_round_trip(&main_prompt("end_turn"));
        let b = FrameTreeDetector::new().on_round_trip(&main_prompt("end_turn"));
        let (ta, tb) = (a.turn_id.unwrap(), b.turn_id.unwrap());
        assert_eq!(ta.len(), 26, "ULID is 26 chars");
        assert_ne!(ta, tb, "two sessions' first turns get distinct ids");
    }

    #[test]
    fn registry_isolates_sessions() {
        let reg = FrameTreeRegistry::new();
        let s1 = MarkingSessionId::new("session-1");
        let s2 = MarkingSessionId::new("session-2");
        let m1 = reg.on_round_trip(&s1, &main_prompt("end_turn"));
        let m2 = reg.on_round_trip(&s2, &main_prompt("end_turn"));
        assert_eq!(m1.role, FrameRole::Main);
        assert_eq!(
            m2.role,
            FrameRole::Main,
            "session 2 seeds its own ROOT turn"
        );
        assert_ne!(m1.turn_id, m2.turn_id, "distinct turn ids across sessions");
        assert_eq!(reg.session_count(), 2);
    }
}
