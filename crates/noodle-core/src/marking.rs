//! Cross-request marking state (ADR 028).
//!
//! The marking-detector contract turns wire signals into the stable
//! mark identifiers (`session_id`, `turn_id`, `parent_session_id`)
//! that surface in `tap.jsonl`'s marks block. This module defines
//! the typed surfaces the contract operates on:
//!
//! - [`MarkingSessionId`] — the wire-extracted per-cell session
//!   identifier (e.g. `X-Claude-Code-Session-Id` value), distinct
//!   from `noodle-core`'s hash-based [`crate::SessionId`].
//! - [`SessionState`] — the cached per-session state required to
//!   apply the §4.1 decision rule on each round-trip.
//! - [`StopReason`] — normalised turn-termination signal lifted from
//!   the wire across vendors.
//! - [`SystemHash`] — a small content-addressable fingerprint used
//!   to detect sub-agent transitions (system-prompt replacement
//!   within a session).
//! - [`MarkingStore`] — the trait per ADR 028 §3.1: `get` to read,
//!   `put` to write at flow close. Implementations live in
//!   `noodle-adapters` (`InMemoryMarkingStore`).
//! - [`MarkingDetector`] — the per-cell detector surface
//!   (`on_request_open`, `on_response_stop_reason`,
//!   `on_response_close`).
//!
//! Naming note: ADR 028 §3 names this trait `SessionStore`. The
//! existing `crate::SessionStore` carries directive-enhancement /
//! attribution state keyed by hashed auth+session headers — a
//! different concern. To avoid name collision and to signal what
//! this store actually stores, the trait here is `MarkingStore`.
//! Both stores may coexist on the same proxy instance.

use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use smol_str::SmolStr;

use crate::{AgentRunId, TurnId};

/// Wire-extracted per-cell session identifier — e.g. the value of
/// the `X-Claude-Code-Session-Id` request header on
/// `api.anthropic.com`, or the `conversation_uuid` URL segment on
/// `claude.ai`.
///
/// Distinct from [`crate::SessionId`], which is a hash over
/// auth + session-header bytes used for directive-enhancement state.
/// The two namespaces are independent — one session may be present
/// in both stores under different keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkingSessionId(SmolStr);

impl MarkingSessionId {
    #[must_use]
    pub fn new(id: impl Into<SmolStr>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MarkingSessionId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for MarkingSessionId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

/// Normalised turn-termination signal lifted from the wire across
/// vendors. Mapping per ADR 028 §1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopReason {
    /// `end_turn` — the model finished the current turn cleanly.
    /// The next request on this session belongs to a new turn.
    EndTurn,
    /// `max_tokens` — response truncated at the token cap. Boundary
    /// effect identical to `EndTurn` for marking purposes.
    MaxTokens,
    /// `tool_use` — mid-turn pause. The turn continues; the next
    /// request belongs to the same turn.
    ToolUse,
    /// `stop_sequence` — the model emitted a configured stop
    /// sequence. Boundary effect identical to `EndTurn`.
    StopSequence,
    /// `pause_turn` — partial-turn checkpoint emitted while a
    /// long-running server-side tool (e.g. web search) is still in
    /// progress. The turn continues; boundary effect identical to
    /// `ToolUse`. The continuation set is `{tool_use, pause_turn}`
    /// per ADR 048 Appendix A; `noodle-domain` tags the same wire
    /// value as a partial-turn checkpoint (`TAG_STOP_PAUSE_TURN`).
    PauseTurn,
    /// Wire signal observed but not recognised by name; treated as a
    /// turn boundary defensively (next request mints a fresh
    /// `turn_id`).
    Unknown,
}

impl StopReason {
    /// True when this reason closes the current turn — i.e. the next
    /// request mints a new `turn_id`.
    #[must_use]
    pub fn closes_turn(self) -> bool {
        match self {
            Self::EndTurn | Self::MaxTokens | Self::StopSequence | Self::Unknown => true,
            Self::ToolUse | Self::PauseTurn => false,
        }
    }

    /// Parse from a wire `stop_reason` string. Unknown values map to
    /// [`Self::Unknown`] (which closes the turn — safe default).
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "end_turn" => Self::EndTurn,
            "max_tokens" => Self::MaxTokens,
            "tool_use" => Self::ToolUse,
            "stop_sequence" => Self::StopSequence,
            "pause_turn" => Self::PauseTurn,
            _ => Self::Unknown,
        }
    }
}

/// SHA-256 fingerprint of the system-prompt payload, used to detect
/// sub-agent transitions within a session (ADR 028 §1.3,
/// `system` replacement signal).
///
/// Stored as the raw 32-byte hash; helpers compare by reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemHash([u8; 32]);

impl SystemHash {
    /// Hash the bytes of a system prompt. Empty / missing prompts
    /// hash to a stable sentinel — the empty hash — so absence is
    /// distinguishable from "different prompt".
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(b"noodle-system-v1\0");
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
        Self(h.finalize().into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// First 8 hex chars — for log lines.
    #[must_use]
    pub fn prefix(&self) -> SmolStr {
        let mut s = String::with_capacity(8);
        for b in &self.0[..4] {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        SmolStr::from(s)
    }
}

/// Per-agent-run state — the inner slot of a [`SessionState`],
/// keyed by the canonical [`SystemHash`] that identifies one
/// logical agent inside a session.
///
/// **Why per-agent-run, not per-session** (ADR 048 §11 item 0):
/// Claude Code's `Task` tool spawns a sub-agent inside the same
/// Anthropic `session_id`. The sub-agent's request carries a
/// different canonical system prompt, but the wire-visible session
/// is unchanged. A single-slot per-session state shape collapses
/// the parent's in-flight turn into the sub-agent's lifecycle —
/// the sub-agent's `end_turn` overwrites the parent's
/// `last_stop_reason` and the parent's resumption mints a stale
/// fresh turn instead of continuing the original tool-use turn.
/// The per-agent-run map keeps each agent's in-flight state in
/// its own slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunState {
    /// The turn currently in progress for this agent run.
    pub current_turn_id: TurnId,
    /// The agent-run identifier minted at first sighting of this
    /// canonical system prompt (ADR 023 §2.5). Stable for the
    /// lifetime of the slot.
    pub agent_run_id: AgentRunId,
    /// The most recent round-trip's normalised `stop_reason` for
    /// this agent run. `None` before the first response is
    /// observed.
    pub last_stop_reason: Option<StopReason>,
    /// Lineage — the parent agent run that spawned this one via a
    /// `Task` / `Agent` tool call. `None` for top-level (root)
    /// agent runs. Set by the marking detector when a
    /// `NewAgentRun` decision pops a pending-child marker off the
    /// per-session stack (ADR 048 §11 item 0).
    pub lineage: Option<ParentRunRef>,
}

/// Reference to a parent agent run, recorded in
/// [`AgentRunState::lineage`] when a sub-agent's first round-trip
/// is observed. The `tool_use_id` lets downstream consumers
/// correlate the parent's `tool_use` content block with the
/// sub-agent's full lifecycle even when the wire `session_id` is
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentRunRef {
    /// The parent's wire session id. Typically equals the child's
    /// session id (Claude Code propagates the parent header), but
    /// modelled explicitly so future per-session sub-agents work
    /// without a special case.
    pub session_id: MarkingSessionId,
    /// The parent's `current_turn_id` at the moment the parent
    /// emitted the `tool_use` that spawned this run.
    pub turn_id: TurnId,
    /// The parent's `agent_run_id` (lifetime of the parent agent).
    pub agent_run_id: AgentRunId,
    /// The Anthropic `tool_use.id` of the parent's spawning
    /// `Task` / `Agent` block (e.g. `"toolu_01ABCD…"`). Lets the
    /// viewer link the parent's `tool_use` row directly to this
    /// sub-agent's run.
    pub tool_use_id: SmolStr,
    /// Fingerprint of the spawn's `input.prompt` text. The
    /// sub-agent's first request carries the same text verbatim as
    /// a text block of its first user message (wire fact verified
    /// against `captures/max/parent-task-subagent.mitm` — see
    /// ADR 048 gap review §6.R2), so a pending child is popped
    /// only by the request that carries its prompt. Interposed
    /// side-calls (title-gen, quota probes) never match and can
    /// never steal lineage. `None` when the spawn carried no
    /// `prompt` string — such an entry is unmatchable and the
    /// child degrades to unattributed.
    pub child_prompt_hash: Option<SystemHash>,
}

/// Per-session cached state required by the marking-detector
/// decision rule (ADR 028 §3.1, §4.1, ADR 048 §11 item 0). Stored
/// in a [`MarkingStore`] keyed by [`MarkingSessionId`].
///
/// One session may host several concurrent agent runs (parent +
/// sub-agents from the `Task` tool). Each gets its own
/// [`AgentRunState`] keyed by the canonical [`SystemHash`] of its
/// system prompt. Hashless turns (e.g. haiku title-gen side-calls
/// with no `system` field) land in the `None` slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionState {
    /// Per-agent-run state, keyed by canonical
    /// [`SystemHash`]. `None` is a real key — used for requests
    /// with no `system` block at all.
    pub runs: HashMap<Option<SystemHash>, AgentRunState>,
    /// Timestamp of the most recent write (Unix milliseconds).
    /// Used for eviction.
    pub last_observed_at_unix_ms: u64,
}

impl SessionState {
    /// Construct the initial state for a never-before-seen session
    /// — seeds a single agent run keyed by `system_hash` with the
    /// supplied `turn_id` / `agent_run_id` and no observed stop
    /// reason yet.
    #[must_use]
    pub fn fresh(
        turn_id: TurnId,
        agent_run_id: AgentRunId,
        system_hash: Option<SystemHash>,
        now_unix_ms: u64,
    ) -> Self {
        let mut runs = HashMap::new();
        runs.insert(
            system_hash,
            AgentRunState {
                current_turn_id: turn_id,
                agent_run_id,
                last_stop_reason: None,
                lineage: None,
            },
        );
        Self {
            runs,
            last_observed_at_unix_ms: now_unix_ms,
        }
    }

    /// Read the [`AgentRunState`] for a given canonical system
    /// hash, if any.
    #[must_use]
    pub fn run(&self, system_hash: Option<&SystemHash>) -> Option<&AgentRunState> {
        self.runs.get(&system_hash.copied())
    }

    /// Test/adapter helper: seed a single agent run with a
    /// supplied `last_stop_reason` so callers don't have to
    /// inline the map literal. Mirrors the pre-refactor
    /// `SessionState { current_turn_id, current_agent_run_id,
    /// last_stop_reason, last_system_hash, ... }` shape via a
    /// single call.
    #[must_use]
    pub fn with_seeded_run(
        turn_id: TurnId,
        agent_run_id: AgentRunId,
        system_hash: Option<SystemHash>,
        last_stop_reason: Option<StopReason>,
        now_unix_ms: u64,
    ) -> Self {
        let mut state = Self::fresh(turn_id, agent_run_id, system_hash, now_unix_ms);
        if let Some(run) = state.runs.get_mut(&system_hash) {
            run.last_stop_reason = last_stop_reason;
        }
        state
    }
}

/// The marking-state store (ADR 028 §3.1). Two operations: `get` to
/// read the cached state at flow open, `put` to write back at flow
/// close. Atomic per-session.
///
/// Implementations live in `noodle-adapters` —
/// `InMemoryMarkingStore` for the single-process default, with
/// Redis / `DynamoDB` / etc. arriving later when cross-process
/// proxies do.
pub trait MarkingStore: Send + Sync + 'static {
    /// Read the cached state for a session. `None` if no round-trip
    /// has been observed for this session.
    fn get(&self, session_id: &MarkingSessionId) -> Option<SessionState>;

    /// Write the cached state at flow close. Atomic per-session.
    fn put(&self, session_id: MarkingSessionId, state: SessionState);
}

/// Decision returned by [`MarkingDetector::on_request_open`] —
/// names the `turn_id` to stamp on the request record and whether
/// it's a freshly minted turn or a continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkingDecision {
    /// The turn id to stamp on this round-trip's record (ADR 028 §4.1).
    pub turn_id: TurnId,
    /// The agent run id to stamp on this round-trip's record
    /// (ADR 023 §2.5). Minted when the canonical system prompt
    /// changes; reused otherwise.
    pub agent_run_id: AgentRunId,
    /// The `turn_id` minting outcome.
    pub kind: MarkingDecisionKind,
    /// The `agent_run_id` minting outcome — `true` when the
    /// detector minted a fresh id (boundary transition), `false`
    /// when it reused the cached id.
    pub agent_run_kind: AgentRunDecisionKind,
    /// Lineage — the parent agent run that spawned this one. Set
    /// on `NewAgentRun` decisions when the detector pops a
    /// pending child off the per-session stack (a parent emitted
    /// `tool_use(Task|Agent)` upstream). `None` for top-level
    /// runs and for continuations. ADR 048 §11 item 0.
    pub lineage: Option<ParentRunRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkingDecisionKind {
    /// First round-trip for this session — no cached state at
    /// all. Fresh `turn_id`.
    FreshSession,
    /// Cached session exists but this canonical system hash has
    /// not been seen in it before — a sub-agent (or distinct
    /// agent run) opening its first turn inside an existing
    /// session. Fresh `turn_id` under a fresh agent-run slot.
    /// New in ADR 048 §11 item 0.
    NewAgentRun,
    /// Cached state for this hash exists and the prior turn
    /// closed (`end_turn` / `max_tokens` / unknown) — the
    /// detector minted a fresh `turn_id` and overwrote the slot's
    /// cached turn.
    NewTurn,
    /// Cached state for this hash exists, prior stop was
    /// `tool_use` — the detector reused the slot's cached
    /// `current_turn_id`.
    Continuation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRunDecisionKind {
    /// First round-trip for this session — the detector minted a
    /// fresh `agent_run_id` alongside the fresh `turn_id`.
    FreshSession,
    /// The canonical system-prompt hash of this round-trip differs
    /// from the cached `last_system_hash` — the detector minted a
    /// fresh `agent_run_id` and overwrote the cached entry.
    /// ADR 023 §2.5 boundary.
    NewAgentRun,
    /// The canonical system-prompt hash is unchanged from the prior
    /// round-trip — the detector reused the cached
    /// `current_agent_run_id`.
    Continuation,
}

/// Per-cell marking-detector surface (ADR 028 §4). Each cell
/// (`(domain, endpoint)` pair) with a marking capability implements
/// this contract against its wire facts (§5 per-cell specs).
///
/// The trait's three methods correspond to the three steps of the
/// §4 contract — request open, response stream, response close.
pub trait MarkingDetector: Send + Sync + 'static {
    /// Step 1 (§4.1) — read the cached state for `session_id` and
    /// decide the `turn_id` to stamp on this request record.
    /// Implementations typically build the [`MarkingDecision`] from
    /// the §4.1 decision table.
    ///
    /// `first_user_text_hashes` carries a fingerprint per text
    /// block of the request's **first** user message. On a
    /// `NewAgentRun` decision, a pending child is popped only when
    /// its spawn-prompt fingerprint matches one of these — the
    /// sub-agent's first request carries the spawn's
    /// `input.prompt` verbatim as a first-user text block, while
    /// interposed side-calls (title-gen, quota probes) never do
    /// (ADR 048 gap review §6.R2). Empty slice = no fingerprints
    /// available; no pending child can match.
    fn on_request_open(
        &self,
        session_id: &MarkingSessionId,
        request_system_hash: Option<&SystemHash>,
        first_user_text_hashes: &[SystemHash],
        now_unix_ms: u64,
    ) -> MarkingDecision;

    /// Step 2 (§4.2) — invoked when the response stream's
    /// `message_delta.delta.stop_reason` is observed. Implementations
    /// typically just remember the value for the close step.
    fn on_response_stop_reason(&self, session_id: &MarkingSessionId, stop: StopReason);

    /// Step 2′ (ADR 048 §11 item 0) — invoked when the response
    /// stream emits a `content_block_start` of type `tool_use`.
    /// Detectors interested in sub-agent lineage push a pending
    /// child onto a per-session stack when `tool_use_name` is
    /// `"Task"` / `"Agent"`; the next request whose first-user
    /// text fingerprints contain `prompt_hash` pops that entry and
    /// stamps lineage on the new [`AgentRunState`].
    ///
    /// `prompt_hash` fingerprints the spawn's `input.prompt`
    /// string; `None` when the input carried no prompt (the entry
    /// is then unmatchable and its child degrades to
    /// unattributed).
    ///
    /// Default impl is a no-op so detectors that don't care about
    /// lineage (cells that never spawn sub-agents) don't need to
    /// override.
    fn on_response_tool_use(
        &self,
        _session_id: &MarkingSessionId,
        _tool_use_name: &str,
        _tool_use_id: &str,
        _prompt_hash: Option<SystemHash>,
    ) {
    }

    /// Step 3 (§4.3) — invoked when the flow closes. The detector
    /// writes the updated [`SessionState`] back to the store. The
    /// `request_system_hash` is the hash from `on_request_open`
    /// (passed back so detectors don't have to cache it themselves).
    fn on_response_close(
        &self,
        session_id: MarkingSessionId,
        decision: &MarkingDecision,
        request_system_hash: Option<SystemHash>,
        now_unix_ms: u64,
    );
}

/// Type-erased pointer to a [`MarkingStore`], for trait-object
/// passing through the engine without parameterising every consumer.
pub type SharedMarkingStore = Arc<dyn MarkingStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_from_wire_canonical_values() {
        assert_eq!(StopReason::from_wire("end_turn"), StopReason::EndTurn);
        assert_eq!(StopReason::from_wire("max_tokens"), StopReason::MaxTokens);
        assert_eq!(StopReason::from_wire("tool_use"), StopReason::ToolUse);
        assert_eq!(
            StopReason::from_wire("stop_sequence"),
            StopReason::StopSequence
        );
        // ADR 048 Appendix A: pause_turn is the server-tool
        // partial-turn checkpoint — a continuation, not a boundary.
        assert_eq!(StopReason::from_wire("pause_turn"), StopReason::PauseTurn);
    }

    #[test]
    fn stop_reason_unknown_defaults_to_unknown() {
        assert_eq!(StopReason::from_wire("refusal"), StopReason::Unknown);
        assert_eq!(StopReason::from_wire(""), StopReason::Unknown);
    }

    #[test]
    fn closes_turn_matches_adr_028_table() {
        // §1.1 + §4.1: end_turn, max_tokens close. tool_use and
        // pause_turn do not (ADR 048 Appendix A continuation set).
        assert!(StopReason::EndTurn.closes_turn());
        assert!(StopReason::MaxTokens.closes_turn());
        assert!(StopReason::StopSequence.closes_turn());
        assert!(StopReason::Unknown.closes_turn(), "defensive default");
        assert!(!StopReason::ToolUse.closes_turn(), "mid-turn pause");
        assert!(
            !StopReason::PauseTurn.closes_turn(),
            "server-tool checkpoint continues the turn"
        );
    }

    #[test]
    fn system_hash_same_input_same_hash() {
        let a = SystemHash::from_bytes(b"You are a helpful assistant.");
        let b = SystemHash::from_bytes(b"You are a helpful assistant.");
        assert_eq!(a, b);
    }

    #[test]
    fn system_hash_different_input_different_hash() {
        let a = SystemHash::from_bytes(b"prompt A");
        let b = SystemHash::from_bytes(b"prompt B");
        assert_ne!(a, b);
    }

    #[test]
    fn system_hash_empty_distinct_from_absent() {
        // The empty bytes hash is a real, stable value — distinct
        // from "no hash recorded" (Option::None). Tests pin the
        // empty hash is deterministic.
        let empty1 = SystemHash::from_bytes(b"");
        let empty2 = SystemHash::from_bytes(b"");
        let nonempty = SystemHash::from_bytes(b"x");
        assert_eq!(empty1, empty2);
        assert_ne!(empty1, nonempty);
    }

    #[test]
    fn marking_session_id_from_string() {
        let id: MarkingSessionId = "uuid-1234".into();
        assert_eq!(id.as_str(), "uuid-1234");
        assert_eq!(id, MarkingSessionId::new("uuid-1234"));
    }

    #[test]
    fn fresh_session_state_seeds_run_for_hash() {
        let t = TurnId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9R");
        let a = AgentRunId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9X");
        let h = SystemHash::from_bytes(b"parent-system-prompt");
        let s = SessionState::fresh(t.clone(), a.clone(), Some(h), 1_000);
        let run = s.run(Some(&h)).expect("run for the seeded hash");
        assert_eq!(run.current_turn_id, t);
        assert_eq!(run.agent_run_id, a);
        assert!(run.last_stop_reason.is_none());
        assert_eq!(s.last_observed_at_unix_ms, 1_000);
        assert!(s.run(None).is_none(), "no hashless slot seeded");
    }

    #[test]
    fn session_state_holds_independent_runs_per_hash() {
        let t1 = TurnId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9A");
        let a1 = AgentRunId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9B");
        let h1 = SystemHash::from_bytes(b"parent");
        let mut s = SessionState::fresh(t1.clone(), a1.clone(), Some(h1), 1_000);

        let t2 = TurnId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9C");
        let a2 = AgentRunId::mint("01HV5GH8X8WJ6E0CMQ8Q3Z4N9D");
        let h2 = SystemHash::from_bytes(b"sub-agent");
        s.runs.insert(
            Some(h2),
            AgentRunState {
                current_turn_id: t2.clone(),
                agent_run_id: a2.clone(),
                last_stop_reason: Some(StopReason::EndTurn),
                lineage: None,
            },
        );

        assert_eq!(s.run(Some(&h1)).unwrap().current_turn_id, t1);
        assert_eq!(s.run(Some(&h2)).unwrap().current_turn_id, t2);
        assert_eq!(s.run(Some(&h1)).unwrap().agent_run_id, a1);
        assert_eq!(s.run(Some(&h2)).unwrap().agent_run_id, a2);
        assert!(s.run(Some(&h1)).unwrap().last_stop_reason.is_none());
        assert_eq!(
            s.run(Some(&h2)).unwrap().last_stop_reason,
            Some(StopReason::EndTurn)
        );
    }
}
