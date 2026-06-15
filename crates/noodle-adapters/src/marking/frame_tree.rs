//! ADR 052 §6 — the per-session `tool_use` frame-tree reconstruction.
//!
//! This is the corrected marking algorithm: a session is a tree of agent-run
//! **frames** rooted at the main agent (`ROOT`), each frame identified by the
//! spawning `tool_use.id`. Per round-trip the detector classifies, in order:
//!
//! 1. **CHAIN** — the request answers a pending `tool_use` ⇒ the frame that
//!    emitted it (a `Bash` result resumes the same frame; a sub-agent's
//!    `tool_result` resumes the parent).
//! 2. **SPAWN** — the first request of a sub-agent matches a pending
//!    `Task`/`Agent` spawn by prompt fingerprint; consumed on match.
//! 3. **ROOT** — a chain-less, spawn-less round-trip that is **not** a harness
//!    wrapper and either *seeds* ROOT (first turn) or *re-enters* ROOT by
//!    structural thread-extension (turn 2..N).
//! 4. else **side-call** — connected to nothing in the tree.
//!
//! A turn opens at a ROOT round-trip carrying genuine new user input and closes
//! at the depth-0 terminal `stop_reason` with no open children. Frame identity
//! is the `tool_use.id` only — never prompt content (ADR 052 §3, §6).
//!
//! State is **per session**: [`FrameTreeDetector`] is the single-session
//! algorithm; [`FrameTreeRegistry`] partitions a [`FrameTreeDetector`] per
//! `session_id` (ADR 052 §8 — no cross-session frame/turn leakage). Wiring the
//! registry onto the proxy's request/response events and the `WireMarks`
//! contract is the integration step.

use std::collections::HashMap;

use dashmap::DashMap;
use noodle_core::MarkingSessionId;
use ulid::Ulid;

/// The role a round-trip plays in the session's frame tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRole {
    /// The depth-0 main agent (`frame_id == "ROOT"`).
    Main,
    /// A sub-agent frame spawned by a `Task`/`Agent` `tool_use`.
    SubAgent,
    /// Off-tree harness call (quota, title-gen, security-monitor, suggestion,
    /// compactor) — no turn, no place in the tree.
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
    /// The spawning `tool_use.id`; `"ROOT"` for the main agent; `None` for a
    /// side-call.
    pub frame_id: Option<String>,
    /// The frame that spawned this one; `None` for ROOT and side-calls.
    pub parent_frame_id: Option<String>,
    /// 0 = main; 1+ = sub-agent nesting; `None` for side-calls.
    pub depth: Option<u32>,
    /// The top-level turn this round-trip belongs to; `None` for side-calls.
    pub turn_id: Option<String>,
}

/// One `Task`/`Agent`/tool `tool_use` block emitted in a response.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub name: String,
    pub id: String,
    /// SHA-256 of the spawn's `input.prompt`, for `Task`/`Agent` spawns. `None`
    /// for non-spawn tools or spawns without a prompt string.
    pub prompt_sha256: Option<String>,
}

/// Request-side §6 signals, known at request open — the inputs to frame
/// classification (CHAIN / SPAWN / ROOT). All hashes / ids / enums, no text.
#[derive(Debug, Clone, Default)]
pub struct RequestSignals {
    pub max_tokens: Option<u64>,
    /// `tool_use` ids this request answers (CHAIN).
    pub request_tool_result_ids: Vec<String>,
    /// SHA-256 of each text block of the first user message (SPAWN match keys).
    pub first_user_text_sha256s: Vec<String>,
    /// `"session" | "transcript" | "suggestion" | "none"` — trailing-text
    /// harness-wrapper classification.
    pub trailing_wrapper_kind: String,
    /// Any trailing-user text block is non-empty and not a wrapper prefix.
    pub has_genuine_user_text: bool,
    /// Ordered hash-chain of message identities (`extends_root`).
    pub message_sig: Vec<String>,
}

/// Response-side §6 signals, known at response close — the inputs to PUSH /
/// turn-close (steps 6–7).
#[derive(Debug, Clone, Default)]
pub struct ResponseSignals {
    /// Wire `stop_reason`, if observed.
    pub stop_reason: Option<String>,
    /// `tool_use` blocks emitted in the response (PUSH / spawn registration).
    pub response_tool_uses: Vec<ToolUse>,
}

/// Outcome of [`FrameTreeDetector::on_request_open`]: the §5 marks for this
/// round-trip (available at open — classification needs only request signals)
/// plus the opaque state the matching [`FrameTreeDetector::on_response_close`]
/// needs to PUSH this round-trip's response into the tree.
#[derive(Debug, Clone)]
pub struct OpenOutcome {
    /// The §5 marks to stamp on this round-trip.
    pub marks: FrameMarks,
    /// The resolved frame id (`None` for a side-call); the frame the response's
    /// `tool_use`s are credited to at close.
    frame: Option<String>,
    /// The request's `tool_result` ids, cleared from `pending_tu` at close.
    answered: Vec<String>,
}

/// The full per-round-trip signals (request + response), for whole-capture
/// replay / tests. The proxy uses the [`RequestSignals`] / [`ResponseSignals`]
/// split instead, since it learns the two halves at different lifecycle points.
#[derive(Debug, Clone, Default)]
pub struct RoundTripSignals {
    pub max_tokens: Option<u64>,
    pub request_tool_result_ids: Vec<String>,
    pub first_user_text_sha256s: Vec<String>,
    pub trailing_wrapper_kind: String,
    pub has_genuine_user_text: bool,
    pub message_sig: Vec<String>,
    pub stop_reason: Option<String>,
    pub response_tool_uses: Vec<ToolUse>,
}

const ROOT: &str = "ROOT";

/// Terminal stop reasons that close a depth-0 turn (ADR 052 §6 step 7).
fn is_terminal(stop: Option<&str>) -> bool {
    matches!(stop, Some("end_turn" | "max_tokens" | "stop_sequence"))
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Index into the parent-id arena; `None` for ROOT.
    parent: Option<usize>,
    depth: u32,
}

/// Default turn-id mint: a fresh ULID per turn (globally unique across
/// sessions). The ordinal is ignored — production wants opacity + uniqueness,
/// not a session-local counter (`turn-1` collides across sessions).
fn ulid_turn_id(_ordinal: u32) -> String {
    Ulid::new().to_string()
}

/// Per-session §6 reconstruction state. One instance per `session_id`.
#[derive(Debug)]
pub struct FrameTreeDetector {
    /// `frame_id` -> (parent index, depth).
    frames: HashMap<String, Frame>,
    /// Dense arena of frame ids so a frame's parent id is cheap to resolve.
    frame_ids: Vec<String>,
    /// Unanswered `tool_use.id` -> emitting `frame_id`.
    pending_tu: HashMap<String, String>,
    /// Unopened spawns: (`prompt_sha256`, spawning `tool_use.id`, parent
    /// `frame_id`), in emission order.
    pending_spawn: Vec<(String, String, String)>,
    /// Structural signature of the ROOT thread's last request (`extends_root`).
    root_sig: Option<Vec<String>>,
    in_turn: bool,
    /// 1-based ordinal of turns opened in this session (passed to `mint_turn`).
    turn: u32,
    /// The id minted when the current turn opened, reused on every round-trip
    /// of that turn (stamped on the §5 marks). `None` before the first turn.
    current_turn_id: Option<String>,
    /// Turn-id minter, called once per turn open. Defaults to [`ulid_turn_id`];
    /// tests inject a deterministic counter via [`Self::with_turn_mint`]. A
    /// non-capturing fn pointer keeps the detector `Debug` + cheap to clone.
    mint_turn: fn(u32) -> String,
}

impl Default for FrameTreeDetector {
    fn default() -> Self {
        Self {
            frames: HashMap::new(),
            frame_ids: Vec::new(),
            pending_tu: HashMap::new(),
            pending_spawn: Vec::new(),
            root_sig: None,
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

    fn is_harness_wrapper(req: &RequestSignals) -> bool {
        req.max_tokens == Some(1) || req.trailing_wrapper_kind != "none"
    }

    fn extends_root(&self, sig: &[String]) -> bool {
        match &self.root_sig {
            Some(root) => root.len() <= sig.len() && sig[..root.len()] == root[..],
            None => false,
        }
    }

    fn register_frame(&mut self, id: &str, parent: Option<&str>, depth: u32) {
        let parent_idx = parent.map(|p| {
            self.frame_ids
                .iter()
                .position(|x| x == p)
                .unwrap_or_else(|| {
                    self.frame_ids.push(p.to_string());
                    self.frame_ids.len() - 1
                })
        });
        self.frames.insert(
            id.to_string(),
            Frame {
                parent: parent_idx,
                depth,
            },
        );
        if !self.frame_ids.iter().any(|x| x == id) {
            self.frame_ids.push(id.to_string());
        }
    }

    fn parent_id_of(&self, id: &str) -> Option<String> {
        self.frames
            .get(id)
            .and_then(|f| f.parent)
            .map(|idx| self.frame_ids[idx].clone())
    }

    /// Classify a round-trip from its **request** signals and produce its §5
    /// marks (ADR 052 §6 steps 1–5). The frame decision needs only the request
    /// — every response it depends on (the one it CHAINs to, or the spawn it
    /// opens) has causally closed before this request opens — so this is
    /// correct even for interleaved parallel sub-agents. Pair every call with
    /// [`Self::on_response_close`] using the returned [`OpenOutcome`].
    pub fn on_request_open(&mut self, req: &RequestSignals) -> OpenOutcome {
        // 1. CHAIN — answers a pending tree tool_use.
        let mut frame: Option<String> = req
            .request_tool_result_ids
            .iter()
            .find(|t| self.pending_tu.contains_key(*t))
            .map(|t| self.pending_tu[t].clone());

        // 2. SPAWN — open a sub-agent by spawn-prompt fingerprint (consume).
        let spawn_pos = if frame.is_none() {
            self.pending_spawn
                .iter()
                .position(|(ph, _, _)| req.first_user_text_sha256s.contains(ph))
        } else {
            None
        };
        if let Some(pos) = spawn_pos {
            let (_, tu_id, parent) = self.pending_spawn.remove(pos);
            let depth = self.frames.get(&parent).map_or(1, |f| f.depth + 1);
            self.register_frame(&tu_id, Some(&parent), depth);
            frame = Some(tu_id);
        }

        // 3. ROOT — seed or re-enter (not a harness wrapper).
        if frame.is_none() && !Self::is_harness_wrapper(req) {
            if self.root_sig.is_none() && req.has_genuine_user_text {
                self.register_frame(ROOT, None, 0);
                frame = Some(ROOT.to_string());
            } else if self.root_sig.is_some() && self.extends_root(&req.message_sig) {
                frame = Some(ROOT.to_string());
            }
        }

        // 4. SIDE-CALL — connected to nothing; does not touch root_sig.
        let Some(frame) = frame else {
            return OpenOutcome {
                marks: FrameMarks {
                    role: FrameRole::SideCall,
                    frame_id: None,
                    parent_frame_id: None,
                    depth: None,
                    turn_id: None,
                },
                frame: None,
                answered: Vec::new(),
            };
        };

        // 5. ROOT bookkeeping — keep the thread current; open a turn only on
        //    genuine new user input.
        if frame == ROOT {
            self.root_sig = Some(req.message_sig.clone());
            if !self.in_turn && req.has_genuine_user_text {
                self.turn += 1;
                self.current_turn_id = Some((self.mint_turn)(self.turn));
                self.in_turn = true;
            }
        }

        let role = if frame == ROOT {
            FrameRole::Main
        } else {
            FrameRole::SubAgent
        };
        let marks = FrameMarks {
            role,
            frame_id: Some(frame.clone()),
            parent_frame_id: self.parent_id_of(&frame),
            depth: self.frames.get(&frame).map(|f| f.depth),
            // The id minted when this turn opened; every round-trip of the turn
            // (main + all sub-agents) carries it (ADR 052 §5, FR3).
            turn_id: self.current_turn_id.clone(),
        };

        OpenOutcome {
            marks,
            frame: Some(frame),
            answered: req.request_tool_result_ids.clone(),
        }
    }

    /// Fold a round-trip's **response** into the tree (ADR 052 §6 steps 6–7):
    /// register the response's `tool_use`s (and `Task`/`Agent` spawns) under the
    /// round-trip's frame, clear the answered `tool_use`s, and close the turn on
    /// a depth-0 terminal with no open children. No-op for a side-call.
    pub fn on_response_close(&mut self, outcome: &OpenOutcome, resp: &ResponseSignals) {
        let Some(frame) = outcome.frame.as_deref() else {
            return;
        };

        // 6. PUSH — register response tool_uses; Task/Agent register spawns.
        for tu in &resp.response_tool_uses {
            self.pending_tu.insert(tu.id.clone(), frame.to_string());
        }
        for tu in &resp.response_tool_uses {
            let is_spawn = matches!(tu.name.as_str(), "Task" | "Agent");
            if let Some(ph) = tu.prompt_sha256.as_ref().filter(|_| is_spawn) {
                self.pending_spawn
                    .push((ph.clone(), tu.id.clone(), frame.to_string()));
            }
        }
        for t in &outcome.answered {
            self.pending_tu.remove(t);
        }

        // 7. CLOSE — depth-0 terminal with no open children closes the turn.
        if frame == ROOT && is_terminal(resp.stop_reason.as_deref()) {
            let open_children = self
                .pending_tu
                .values()
                .any(|v| v == ROOT || self.frames.contains_key(v));
            if !open_children {
                self.in_turn = false;
            }
        }
    }

    /// Whole round-trip in one call (request open then response close). For
    /// replay / tests where both halves are known together.
    pub fn on_round_trip(&mut self, rt: &RoundTripSignals) -> FrameMarks {
        let req = RequestSignals {
            max_tokens: rt.max_tokens,
            request_tool_result_ids: rt.request_tool_result_ids.clone(),
            first_user_text_sha256s: rt.first_user_text_sha256s.clone(),
            trailing_wrapper_kind: rt.trailing_wrapper_kind.clone(),
            has_genuine_user_text: rt.has_genuine_user_text,
            message_sig: rt.message_sig.clone(),
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
/// detector per `session_id` so concurrent sessions never share frame, spawn,
/// or turn state. Thread-safe; clone-free hot path via [`DashMap`].
///
/// This is the surface the proxy drives: extract the per-round-trip wire
/// signals, call [`FrameTreeRegistry::on_round_trip`] with the session id, and
/// stamp the returned [`FrameMarks`] onto the wire-marks contract.
#[derive(Debug, Default)]
pub struct FrameTreeRegistry {
    sessions: DashMap<MarkingSessionId, FrameTreeDetector>,
}

impl FrameTreeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify a round-trip's request within its session (ADR 052 §6 steps
    /// 1–5), creating the session's detector on first sighting. Pair with
    /// [`Self::on_response_close`] for the same `session_id`.
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

    /// Fold a round-trip's response into its session's tree (ADR 052 §6 steps
    /// 6–7). No-op if the session was never opened.
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

    /// Number of sessions currently tracked (eviction is the caller's concern;
    /// exposed for observability / tests).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(rt: RoundTripSignals) -> RoundTripSignals {
        RoundTripSignals {
            trailing_wrapper_kind: if rt.trailing_wrapper_kind.is_empty() {
                "none".to_string()
            } else {
                rt.trailing_wrapper_kind
            },
            ..rt
        }
    }

    #[test]
    fn pure_prompt_seeds_root_and_one_turn() {
        let mut d = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
        let m = d.on_round_trip(&sig(RoundTripSignals {
            max_tokens: Some(64000),
            has_genuine_user_text: true,
            message_sig: vec!["user|tx:a".into()],
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        }));
        assert_eq!(m.role, FrameRole::Main);
        assert_eq!(m.frame_id.as_deref(), Some("ROOT"));
        assert_eq!(m.depth, Some(0));
        assert_eq!(m.turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn default_turn_id_is_a_unique_ulid() {
        // Production mint: each turn gets a fresh ULID — globally unique, so
        // two sessions' first turns never collide (the `turn-1` bug).
        let prompt = || {
            sig(RoundTripSignals {
                max_tokens: Some(64000),
                has_genuine_user_text: true,
                message_sig: vec!["user|tx:seed".into()],
                stop_reason: Some("end_turn".into()),
                ..Default::default()
            })
        };
        let a = FrameTreeDetector::new().on_round_trip(&prompt());
        let b = FrameTreeDetector::new().on_round_trip(&prompt());
        let (ta, tb) = (a.turn_id.unwrap(), b.turn_id.unwrap());
        assert_eq!(ta.len(), 26, "ULID is 26 chars");
        assert_ne!(ta, tb, "two sessions' first turns get distinct ids");
    }

    #[test]
    fn quota_probe_is_side_call() {
        let mut d = FrameTreeDetector::new();
        let m = d.on_round_trip(&sig(RoundTripSignals {
            max_tokens: Some(1),
            has_genuine_user_text: true,
            ..Default::default()
        }));
        assert_eq!(m.role, FrameRole::SideCall);
        assert!(m.turn_id.is_none());
    }

    #[test]
    fn registry_isolates_sessions() {
        // Two sessions, interleaved. Each must seed its OWN ROOT turn.
        // Without per-session partitioning, the second session's genuine
        // prompt would be blocked by the first session's root_sig (no chain,
        // no spawn, ROOT already opened, sig doesn't extend) and fall through
        // to side_call — the exact cross-session bug ADR 052 §8 flags.
        let reg = FrameTreeRegistry::new();
        let s1 = MarkingSessionId::new("session-1");
        let s2 = MarkingSessionId::new("session-2");
        let prompt = || {
            sig(RoundTripSignals {
                max_tokens: Some(64000),
                has_genuine_user_text: true,
                message_sig: vec!["user|tx:seed".into()],
                stop_reason: Some("end_turn".into()),
                ..Default::default()
            })
        };
        let m1 = reg.on_round_trip(&s1, &prompt());
        let m2 = reg.on_round_trip(&s2, &prompt());
        assert_eq!(m1.role, FrameRole::Main);
        assert_eq!(m1.frame_id.as_deref(), Some("ROOT"));
        assert_eq!(m2.role, FrameRole::Main, "session 2 seeds its own ROOT");
        assert_eq!(m2.frame_id.as_deref(), Some("ROOT"));
        // Each session opens its own turn — and the ULID mints are distinct, so
        // the two first-turns never collide (the cross-session id-collision the
        // per-session counter `turn-1` had).
        assert!(m1.turn_id.is_some() && m2.turn_id.is_some());
        assert_ne!(m1.turn_id, m2.turn_id, "distinct turn ids across sessions");
        assert_eq!(reg.session_count(), 2);
    }
}
