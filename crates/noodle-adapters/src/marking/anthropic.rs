//! Marking detector for the `(api.anthropic.com, /v1/messages,
//! request→upstream)` cell (ADR 028 §5.1).
//!
//! Wire facts the detector keys on:
//!
//! - **`X-Claude-Code-Session-Id`** request header — per-session
//!   identifier. Used as the [`MarkingSessionId`] for the store.
//! - **`delta.stop_reason`** on the SSE `message_delta` event —
//!   turn-boundary signal. `end_turn` / `max_tokens` / unknown
//!   close the turn; `tool_use` continues.
//! - **`system`** request payload — system-prompt hash, used to
//!   detect sub-agent transitions within a session.
//!
//! ## §4.1 decision table (verbatim)
//!
//! | `SessionStore` entry | `last_stop_reason` | Outcome |
//! |---|---|---|
//! | absent | — | mint fresh `turn_id` |
//! | present | `tool_use` / `pause_turn` | reuse cached `current_turn_id` |
//! | present | `end_turn` / `max_tokens` / unknown | mint fresh `turn_id` |
//!
//! The implementation reads the cached state from a
//! [`MarkingStore`], applies the decision, and (on flow close)
//! writes the new state back.

use std::sync::Arc;

use dashmap::DashMap;
#[cfg(test)]
use noodle_core::SessionState;
use noodle_core::{
    AgentRunDecisionKind, AgentRunId, AgentRunState, MarkingDecision, MarkingDecisionKind,
    MarkingDetector, MarkingSessionId, MarkingStore, ParentRunRef, StopReason, SystemHash, TurnId,
};
use smol_str::SmolStr;
use ulid::Ulid;

/// `AnthropicMarkingDetector` — per ADR 028 §5.1.
///
/// Holds a shared reference to the [`MarkingStore`] (typically
/// [`crate::marking::InMemoryMarkingStore`] in tests / single-
/// process deployments) and an in-memory map of
/// `session_id → observed StopReason` for the in-flight flows
/// between `on_response_stop_reason` and `on_response_close`.
pub struct AnthropicMarkingDetector {
    store: Arc<dyn MarkingStore>,
    /// Per-flow scratch: the `stop_reason` observed during the
    /// response stream, keyed by `session_id`. Drained on
    /// `on_response_close`.
    in_flight_stop: DashMap<MarkingSessionId, StopReason>,
    /// Per-flow scratch: the decision returned by
    /// `on_request_open` for the in-flight request on this
    /// session. Used by `on_response_tool_use` to know which
    /// parent (`turn_id` + `agent_run_id`) to credit when pushing
    /// a pending child onto [`Self::pending_children`]. Drained
    /// on `on_response_close`.
    in_flight_decision: DashMap<MarkingSessionId, MarkingDecision>,
    /// Per-session LIFO stack of sub-agents the parent has
    /// promised via `tool_use(Task|Agent)` but the wire hasn't
    /// yet observed open their first request. Pushed by
    /// `on_response_tool_use` when the name matches; popped by
    /// `on_request_open` on a `NewAgentRun` decision so the new
    /// `AgentRunState.lineage` carries the parent's ids.
    /// ADR 048 §11 item 0.
    pending_children: DashMap<MarkingSessionId, Vec<ParentRunRef>>,
    /// Mint function for turn ids. Defaults to a fresh ULID;
    /// overridable for tests so the resulting marks are
    /// deterministic.
    mint_turn_id: Box<dyn Fn() -> TurnId + Send + Sync>,
    /// Mint function for agent-run ids. Defaults to a fresh ULID;
    /// overridable for tests.
    mint_agent_run_id: Box<dyn Fn() -> AgentRunId + Send + Sync>,
}

impl AnthropicMarkingDetector {
    /// Default constructor — mints turn ids + agent run ids as
    /// fresh ULIDs.
    #[must_use]
    pub fn new(store: Arc<dyn MarkingStore>) -> Self {
        Self {
            store,
            in_flight_stop: DashMap::new(),
            in_flight_decision: DashMap::new(),
            pending_children: DashMap::new(),
            mint_turn_id: Box::new(|| TurnId::mint(Ulid::new().to_string())),
            mint_agent_run_id: Box::new(|| AgentRunId::mint(Ulid::new().to_string())),
        }
    }

    /// Test constructor — mint functions are supplied by the
    /// caller so the resulting marks are deterministic. Each call
    /// to `mint_turn` produces the next id in its sequence; same
    /// for `mint_agent_run`.
    #[cfg(test)]
    pub fn with_mints(
        store: Arc<dyn MarkingStore>,
        mint_turn: impl Fn() -> TurnId + Send + Sync + 'static,
        mint_agent_run: impl Fn() -> AgentRunId + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            in_flight_stop: DashMap::new(),
            in_flight_decision: DashMap::new(),
            pending_children: DashMap::new(),
            mint_turn_id: Box::new(mint_turn),
            mint_agent_run_id: Box::new(mint_agent_run),
        }
    }

    /// Tool-use names that spawn a sub-agent in Claude Code. Both
    /// `Task` (the public tool name on `claude.ai`) and `Agent`
    /// (the CLI-side alias surfaced in `claude -p`'s SSE stream)
    /// appear in practice; treat them interchangeably.
    fn is_sub_agent_spawner(name: &str) -> bool {
        matches!(name, "Task" | "Agent")
    }

    fn mint_turn(&self) -> TurnId {
        (self.mint_turn_id)()
    }

    fn mint_agent_run(&self) -> AgentRunId {
        (self.mint_agent_run_id)()
    }
}

impl MarkingDetector for AnthropicMarkingDetector {
    /// ADR 028 §4.1 (turn-boundary) + ADR 023 §2.5 (agent-run
    /// boundary), per-agent-run scope per ADR 048 §11 item 0.
    ///
    /// State lives in `SessionState.runs`, a map of
    /// `Option<SystemHash> → AgentRunState`. Parent and sub-agent
    /// in the same Anthropic `session_id` end up in distinct slots
    /// (different canonical system prompts → different keys);
    /// their turn boundaries no longer collide.
    ///
    /// **Per-slot turn boundary**:
    ///
    /// | Slot for `request_system_hash` | `last_stop_reason` | Outcome |
    /// |---|---|---|
    /// | absent (whole session cold) | — | mint fresh `turn_id` (`FreshSession`) |
    /// | absent (slot only) | — | mint fresh `turn_id` (`NewAgentRun`) |
    /// | present | `tool_use` / `pause_turn` | reuse slot's `current_turn_id` (`Continuation`) |
    /// | present | `end_turn` / `max_tokens` / unknown | mint fresh `turn_id` (`NewTurn`) |
    ///
    /// **Per-slot agent-run boundary**:
    ///
    /// | Slot for `request_system_hash` | Outcome |
    /// |---|---|
    /// | absent (whole session cold) | mint fresh `agent_run_id` (`FreshSession`) |
    /// | absent (slot only) | mint fresh `agent_run_id` (`NewAgentRun`) |
    /// | present | reuse slot's `agent_run_id` (`Continuation`) |
    fn on_request_open(
        &self,
        session_id: &MarkingSessionId,
        request_system_hash: Option<&SystemHash>,
        first_user_text_hashes: &[SystemHash],
        _now_unix_ms: u64,
    ) -> MarkingDecision {
        let cached = self.store.get(session_id);
        let cached_run = cached.as_ref().and_then(|s| s.run(request_system_hash));

        let (turn_id, kind, agent_run_id, agent_run_kind) = match (&cached, cached_run) {
            // Session totally cold.
            (None, _) => (
                self.mint_turn(),
                MarkingDecisionKind::FreshSession,
                self.mint_agent_run(),
                AgentRunDecisionKind::FreshSession,
            ),
            // Session warm but this hash hasn't been seen yet — a
            // new agent run opening its first turn. Both turn and
            // run are fresh.
            (Some(_), None) => (
                self.mint_turn(),
                MarkingDecisionKind::NewAgentRun,
                self.mint_agent_run(),
                AgentRunDecisionKind::NewAgentRun,
            ),
            // Slot exists — apply the per-slot turn-boundary rule.
            // `tool_use` and `pause_turn` are the continuation set
            // (ADR 048 Appendix A): both mean the model paused
            // mid-turn — a client-side tool round-trip or a
            // server-side tool checkpoint respectively.
            (Some(_), Some(run)) => match run.last_stop_reason {
                Some(StopReason::ToolUse | StopReason::PauseTurn) => (
                    run.current_turn_id.clone(),
                    MarkingDecisionKind::Continuation,
                    run.agent_run_id.clone(),
                    AgentRunDecisionKind::Continuation,
                ),
                Some(_) | None => (
                    self.mint_turn(),
                    MarkingDecisionKind::NewTurn,
                    run.agent_run_id.clone(),
                    AgentRunDecisionKind::Continuation,
                ),
            },
        };

        // ADR 048 §11 item 0 lineage rule (PR-C2 refinement, R2
        // fingerprint match per the ADR 048 gap review §6.R2):
        // `MarkingDecision.lineage` should always reflect the
        // slot's lineage as of this request, so downstream wire
        // consumers (tap.jsonl marks, OTLP attrs) can stamp it on
        // every round-trip — not just the sub-agent's first
        // sighting.
        //
        // - NewAgentRun: pop the pending child whose spawn-prompt
        //   fingerprint matches one of this request's first-user
        //   text blocks. The sub-agent's first request carries the
        //   spawn's `input.prompt` verbatim as a first-user text
        //   block (wire fact verified against
        //   `captures/max/parent-task-subagent.mitm`); interposed
        //   side-calls (title-gen, quota probes, compactor) never
        //   carry it and therefore can never steal a pending
        //   child. No match — side-call, harness-spawned
        //   classifier, or a spawn whose prompt was rewritten en
        //   route — degrades to no lineage, never to
        //   mis-attribution.
        // - Continuation / NewTurn: read the slot's existing
        //   lineage from the cached `AgentRunState`.
        // - FreshSession: no cached state, no lineage.
        let lineage = if matches!(agent_run_kind, AgentRunDecisionKind::NewAgentRun) {
            self.pending_children
                .get_mut(session_id)
                .and_then(|mut stack| {
                    let pos = stack.iter().position(|e| {
                        e.child_prompt_hash
                            .is_some_and(|h| first_user_text_hashes.contains(&h))
                    });
                    pos.map(|p| stack.remove(p))
                })
        } else {
            cached_run.and_then(|r| r.lineage.clone())
        };

        let decision = MarkingDecision {
            turn_id,
            agent_run_id,
            kind,
            agent_run_kind,
            lineage,
        };

        // Cache the decision so `on_response_tool_use` knows which
        // parent ids to credit when it pushes onto the pending
        // stack mid-response.
        self.in_flight_decision
            .insert(session_id.clone(), decision.clone());

        decision
    }

    fn on_response_stop_reason(&self, session_id: &MarkingSessionId, stop: StopReason) {
        self.in_flight_stop.insert(session_id.clone(), stop);
    }

    fn on_response_tool_use(
        &self,
        session_id: &MarkingSessionId,
        tool_use_name: &str,
        tool_use_id: &str,
        prompt_hash: Option<SystemHash>,
    ) {
        if !Self::is_sub_agent_spawner(tool_use_name) {
            return;
        }
        // We need the parent's (turn_id, agent_run_id) to credit
        // — that's whatever `on_request_open` decided for the
        // current in-flight request on this session.
        let Some(parent_decision) = self.in_flight_decision.get(session_id).map(|d| d.clone())
        else {
            // Defensive: tool_use observed without a prior open.
            // Should not happen on a well-formed flow; drop the
            // signal rather than corrupt the stack.
            return;
        };
        let entry = ParentRunRef {
            session_id: session_id.clone(),
            turn_id: parent_decision.turn_id,
            agent_run_id: parent_decision.agent_run_id,
            tool_use_id: SmolStr::new(tool_use_id),
            child_prompt_hash: prompt_hash,
        };
        let mut stack = self.pending_children.entry(session_id.clone()).or_default();
        // Hygiene cap: a spawn whose child never opens a request
        // (interrupted turn, abandoned session) would otherwise
        // accumulate. Oldest-first eviction; 8 comfortably exceeds
        // any observed concurrent-spawn depth.
        if stack.len() >= MAX_PENDING_CHILDREN {
            stack.remove(0);
        }
        stack.push(entry);
    }

    fn on_response_close(
        &self,
        session_id: MarkingSessionId,
        decision: &MarkingDecision,
        request_system_hash: Option<SystemHash>,
        now_unix_ms: u64,
    ) {
        let observed_stop = self.in_flight_stop.remove(&session_id).map(|(_, v)| v);
        self.in_flight_decision.remove(&session_id);

        // Load existing session state (or start a fresh empty one)
        // so we preserve OTHER agents' runs.
        let mut state = self.store.get(&session_id).unwrap_or_default();

        // Lineage write rule:
        // - On NewAgentRun (decision.lineage carries a popped
        //   ParentRunRef), set lineage on the new slot.
        // - On Continuation / NewTurn for an existing slot,
        //   preserve whatever lineage the slot already has.
        // - On FreshSession with no popped parent, lineage = None.
        let lineage = decision.lineage.clone().or_else(|| {
            state
                .runs
                .get(&request_system_hash)
                .and_then(|r| r.lineage.clone())
        });

        state.runs.insert(
            request_system_hash,
            AgentRunState {
                current_turn_id: decision.turn_id.clone(),
                agent_run_id: decision.agent_run_id.clone(),
                last_stop_reason: observed_stop,
                lineage,
            },
        );
        state.last_observed_at_unix_ms = now_unix_ms;
        self.store.put(session_id, state);
    }
}

/// Cap on the per-session pending-children stack. A spawn whose
/// child never opens a request (interrupted turn, abandoned
/// session) would otherwise accumulate; oldest entries evict
/// first. 8 comfortably exceeds any observed concurrent-spawn
/// depth (Claude Code serialises Task dispatch today).
const MAX_PENDING_CHILDREN: usize = 8;

/// Header name carrying the per-session id on `api.anthropic.com`
/// — exposed so callers extracting from `HeaderMap` use the
/// canonical spelling.
pub const SESSION_HEADER: &str = "x-claude-code-session-id";

/// Hash the spawn's `input.prompt` for the pending-children
/// fingerprint, from a decoded `tool_use` input value. The
/// sub-agent's first request carries the same text verbatim as a
/// first-user text block (ADR 048 gap review §6.R2); both sides
/// hash through [`SystemHash::from_bytes`] so equality is
/// byte-equality of the prompt text. `None` when the input has no
/// string `prompt` field.
#[must_use]
pub fn prompt_hash_of_tool_input(input: &serde_json::Value) -> Option<SystemHash> {
    input
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .map(|p| SystemHash::from_bytes(p.as_bytes()))
}

/// Request-side marking fingerprints, computed in one parse of the
/// request body: the canonical system hash (ADR 023 §2.5) and one
/// hash per text block of the **first** user message (the
/// pending-children match keys — ADR 048 gap review §6.R2).
#[derive(Debug, Default)]
pub struct RequestMarkingHashes {
    /// Canonical system-prompt hash; `None` when the body has no
    /// `system` field (same contract as
    /// [`compute_canonical_system_hash`]).
    pub system_hash: Option<SystemHash>,
    /// One fingerprint per `text` block of the first
    /// `role == "user"` message, in block order. Empty when the
    /// body is unparseable or has no user message.
    pub first_user_text_hashes: Vec<SystemHash>,
}

/// Parse the request body once and produce every marking
/// fingerprint the detector needs at request open. Replaces
/// separate `compute_canonical_system_hash` +
/// first-user-text scans on the proxy hot path (one
/// `serde_json` parse, ADR 049 §9.1 discipline).
#[must_use]
pub fn compute_request_marking_hashes(body: &[u8]) -> RequestMarkingHashes {
    if body.is_empty() {
        return RequestMarkingHashes::default();
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return RequestMarkingHashes::default();
    };
    let system_hash = v
        .get("system")
        .and_then(canonical_system_text)
        .map(|text| SystemHash::from_bytes(text.as_bytes()));
    let first_user_text_hashes = v
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        })
        .map(first_user_text_hashes_of_message)
        .unwrap_or_default();
    RequestMarkingHashes {
        system_hash,
        first_user_text_hashes,
    }
}

/// Fingerprint every text block of one user message. String-form
/// content is a single implicit text block; array-form content
/// contributes one hash per `type == "text"` block. Non-text
/// blocks (`tool_result`, images) are skipped.
fn first_user_text_hashes_of_message(message: &serde_json::Value) -> Vec<SystemHash> {
    let Some(content) = message.get("content") else {
        return Vec::new();
    };
    if let Some(s) = content.as_str() {
        return vec![SystemHash::from_bytes(s.as_bytes())];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .map(|t| SystemHash::from_bytes(t.as_bytes()))
        .collect()
}

/// The text prefix that identifies Anthropic's per-request cache /
/// billing-header block inside the `system` array. Blocks whose
/// text starts with this prefix vary per request and do **not**
/// participate in the canonical system prompt — they would
/// otherwise flap the `SystemHash` on every cache rotation and
/// every request would look like a new agent run. Definition
/// mirrors the viewer's `systemPromptCanonical` (ADR 023 §2.5).
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header";

/// Compute the canonical system-prompt [`SystemHash`] for an
/// Anthropic `/v1/messages` request body (ADR 023 §2.5).
///
/// The Anthropic API admits the `system` field in two shapes:
///
/// 1. **String** — `{"system": "you are a helpful assistant."}`.
///    The string is the canonical text.
/// 2. **Array of typed blocks** — `{"system": [{"type": "text",
///    "text": "..."}, ...]}`. The canonical text is the
///    concatenation (joined by `\n`) of every block's `text`
///    field, **excluding** blocks whose text starts with
///    `x-anthropic-billing-header` (those carry per-request cache
///    metadata and are not part of the effective system prompt).
///
/// Returns `None` when the body is empty, not parseable as JSON,
/// or has no `system` field. Returns `Some(SystemHash::from_bytes(""))`
/// when the body parses but the canonical text is the empty string
/// (e.g. all blocks were billing headers) — empty is a stable
/// distinct value, distinguishable from absent.
#[must_use]
pub fn compute_canonical_system_hash(body: &[u8]) -> Option<SystemHash> {
    if body.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let system = v.get("system")?;
    let text = canonical_system_text(system)?;
    Some(SystemHash::from_bytes(text.as_bytes()))
}

fn canonical_system_text(system: &serde_json::Value) -> Option<String> {
    if let Some(s) = system.as_str() {
        return Some(s.to_owned());
    }
    let arr = system.as_array()?;
    let mut parts = Vec::with_capacity(arr.len());
    for block in arr {
        let Some(text) = block.get("text").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if text.starts_with(BILLING_HEADER_PREFIX) {
            continue;
        }
        parts.push(text);
    }
    Some(parts.join("\n"))
}

/// Convenience extractor: read the `X-Claude-Code-Session-Id` value
/// from an HTTP header map, returning `None` if absent or not
/// valid UTF-8. Per §5.1, missing-or-malformed is an
/// `AuditEvent::Errored` upstream of this function — the detector
/// itself just needs the typed id.
#[must_use]
pub fn extract_session_id(headers: &http::HeaderMap) -> Option<MarkingSessionId> {
    headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| MarkingSessionId::new(SmolStr::from(s)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marking::InMemoryMarkingStore;
    use std::sync::Mutex;

    fn session(id: &str) -> MarkingSessionId {
        MarkingSessionId::new(id)
    }

    /// Build a detector whose turn-id + agent-run-id mints produce
    /// predictable sequences (`turn-1`, `turn-2`, … and `run-1`,
    /// `run-2`, …) so test assertions can name the expected mark
    /// directly.
    fn detector_with_counter(store: Arc<dyn MarkingStore>) -> AnthropicMarkingDetector {
        let turn_counter = Arc::new(Mutex::new(0u32));
        let run_counter = Arc::new(Mutex::new(0u32));
        AnthropicMarkingDetector::with_mints(
            store,
            move || {
                let mut g = turn_counter.lock().unwrap();
                *g += 1;
                TurnId::mint(format!("turn-{n}", n = *g))
            },
            move || {
                let mut g = run_counter.lock().unwrap();
                *g += 1;
                AgentRunId::mint(format!("run-{n}", n = *g))
            },
        )
    }

    // ─── §4.1 decision table — one test per row ──────────────────

    #[test]
    fn decision_row_1_absent_session_mints_fresh_turn() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::FreshSession);
        assert_eq!(dec.turn_id, TurnId::mint("turn-1"));
    }

    #[test]
    fn decision_row_2_tool_use_continuation_reuses_turn_id() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("cached-turn"),
                AgentRunId::mint("cached-run"),
                None,
                Some(StopReason::ToolUse),
                500,
            ),
        );

        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::Continuation);
        assert_eq!(
            dec.turn_id,
            TurnId::mint("cached-turn"),
            "continuation reuses cached turn id"
        );
    }

    #[test]
    fn decision_row_2b_pause_turn_continuation_reuses_turn_id() {
        // ADR 048 Appendix A: pause_turn (server-tool checkpoint)
        // is in the continuation set alongside tool_use — the next
        // round trip belongs to the same turn, same agent run.
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("cached-turn"),
                AgentRunId::mint("cached-run"),
                None,
                Some(StopReason::PauseTurn),
                500,
            ),
        );

        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::Continuation);
        assert_eq!(
            dec.turn_id,
            TurnId::mint("cached-turn"),
            "pause_turn continuation reuses cached turn id"
        );
        assert_eq!(
            dec.agent_run_id,
            AgentRunId::mint("cached-run"),
            "pause_turn continuation reuses cached agent run id"
        );
    }

    #[test]
    fn decision_row_3a_end_turn_mints_new_turn() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("old-turn"),
                AgentRunId::mint("old-run"),
                None,
                Some(StopReason::EndTurn),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::NewTurn);
        assert_eq!(dec.turn_id, TurnId::mint("turn-1"));
    }

    #[test]
    fn decision_row_3b_max_tokens_mints_new_turn() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("old-turn"),
                AgentRunId::mint("old-run"),
                None,
                Some(StopReason::MaxTokens),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::NewTurn);
        assert_eq!(dec.turn_id, TurnId::mint("turn-1"));
    }

    #[test]
    fn decision_row_3c_unknown_stop_reason_mints_new_turn_defensively() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("old-turn"),
                AgentRunId::mint("old-run"),
                None,
                Some(StopReason::Unknown),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.kind, MarkingDecisionKind::NewTurn);
    }

    // ─── Multi-round-trip scenarios (capture the §4 contract) ────

    /// End-to-end: two RTs within the same turn (`tool_use`
    /// continuation). Same `turn_id` on both records.
    #[test]
    fn tool_use_continuation_preserves_turn_id_across_two_rts() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());

        // RT 1: fresh session → mint turn-1.
        let dec1 = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec1.kind, MarkingDecisionKind::FreshSession);
        det.on_response_stop_reason(&session("s1"), StopReason::ToolUse);
        det.on_response_close(session("s1"), &dec1, None, 1_500);

        // RT 2: same session, last_stop_reason=tool_use → reuse turn-1.
        let dec2 = det.on_request_open(&session("s1"), None, &[], 2_000);
        assert_eq!(dec2.kind, MarkingDecisionKind::Continuation);
        assert_eq!(
            dec2.turn_id, dec1.turn_id,
            "same turn across RT boundary in tool-use continuation"
        );
    }

    /// End-to-end: three RTs across two turns. First two RTs share
    /// turn-1 (`tool_use`), third RT after `end_turn` mints turn-2.
    #[test]
    fn end_turn_starts_new_turn_id_on_next_rt() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());

        // Turn 1, RT 1.
        let dec1 = det.on_request_open(&session("s1"), None, &[], 1_000);
        det.on_response_stop_reason(&session("s1"), StopReason::ToolUse);
        det.on_response_close(session("s1"), &dec1, None, 1_500);

        // Turn 1, RT 2 (tool_use continuation).
        let dec2 = det.on_request_open(&session("s1"), None, &[], 2_000);
        assert_eq!(dec2.turn_id, dec1.turn_id, "still in turn 1");
        det.on_response_stop_reason(&session("s1"), StopReason::EndTurn);
        det.on_response_close(session("s1"), &dec2, None, 2_500);

        // Turn 2, RT 3 (next user request after end_turn).
        let dec3 = det.on_request_open(&session("s1"), None, &[], 3_000);
        assert_eq!(dec3.kind, MarkingDecisionKind::NewTurn);
        assert_ne!(dec3.turn_id, dec1.turn_id, "fresh turn after end_turn");
    }

    /// Independent sessions never share marks even across many RTs.
    #[test]
    fn distinct_sessions_get_distinct_turn_ids() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store);
        let a = det.on_request_open(&session("session-A"), None, &[], 1);
        let b = det.on_request_open(&session("session-B"), None, &[], 2);
        assert_ne!(a.turn_id, b.turn_id);
    }

    /// `on_response_close` actually writes back so the next RT sees
    /// the updated state. Without this round-trip, the §4.3 step
    /// would silently no-op.
    #[test]
    fn close_writes_back_session_state() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());

        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        det.on_response_stop_reason(&session("s1"), StopReason::EndTurn);
        det.on_response_close(
            session("s1"),
            &dec,
            Some(SystemHash::from_bytes(b"prompt-A")),
            1_500,
        );

        let stored = store.get(&session("s1")).expect("state was written");
        let hash = SystemHash::from_bytes(b"prompt-A");
        let run = stored.run(Some(&hash)).expect("run for hash");
        assert_eq!(run.current_turn_id, dec.turn_id);
        assert_eq!(run.last_stop_reason, Some(StopReason::EndTurn));
        assert_eq!(stored.last_observed_at_unix_ms, 1_500);
    }

    // ─── ADR 023 §2.5 agent-run boundary tests ───────────────────

    #[test]
    fn agent_run_fresh_session_mints_first_id() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), None, &[], 1_000);
        assert_eq!(dec.agent_run_kind, AgentRunDecisionKind::FreshSession);
        assert_eq!(dec.agent_run_id, AgentRunId::mint("run-1"));
    }

    #[test]
    fn agent_run_same_hash_continues_run() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let h = SystemHash::from_bytes(b"You are a helpful assistant.");
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("t-prev"),
                AgentRunId::mint("cached-run"),
                Some(h),
                Some(StopReason::EndTurn),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let dec = det.on_request_open(&session("s1"), Some(&h), &[], 1_000);
        assert_eq!(dec.agent_run_kind, AgentRunDecisionKind::Continuation);
        assert_eq!(
            dec.agent_run_id,
            AgentRunId::mint("cached-run"),
            "same canonical system prompt → same agent_run_id"
        );
    }

    #[test]
    fn agent_run_hash_transition_mints_new_id() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let prev = SystemHash::from_bytes(b"You are agent A.");
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("t-prev"),
                AgentRunId::mint("cached-run"),
                Some(prev),
                Some(StopReason::EndTurn),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let curr = SystemHash::from_bytes(b"You are agent B.");
        let dec = det.on_request_open(&session("s1"), Some(&curr), &[], 1_000);
        assert_eq!(dec.agent_run_kind, AgentRunDecisionKind::NewAgentRun);
        assert_eq!(
            dec.agent_run_id,
            AgentRunId::mint("run-1"),
            "different canonical system prompt → fresh agent_run_id"
        );
    }

    #[test]
    fn agent_run_unseen_hash_mints_new_id_within_existing_session() {
        // Edge case: prior state exists for a different hash (or no
        // hash at all). The new hash's slot is empty → mint fresh
        // agent_run_id. Under the per-agent-run shape, the absent-
        // hash case is just "no slot for this key yet."
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        store.put(
            session("s1"),
            SessionState::with_seeded_run(
                TurnId::mint("t-prev"),
                AgentRunId::mint("cached-run"),
                None,
                Some(StopReason::EndTurn),
                500,
            ),
        );
        let det = detector_with_counter(store);
        let curr = SystemHash::from_bytes(b"You are agent A.");
        let dec = det.on_request_open(&session("s1"), Some(&curr), &[], 1_000);
        assert_eq!(dec.agent_run_kind, AgentRunDecisionKind::NewAgentRun);
    }

    /// Tool-use continuation reuses both turn AND agent run when
    /// the system prompt is unchanged — the two boundary axes are
    /// orthogonal but typically move together inside a single
    /// `claude -p` invocation.
    #[test]
    fn tool_use_continuation_preserves_both_turn_and_agent_run() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());
        let h = SystemHash::from_bytes(b"You are a helpful assistant.");

        let dec1 = det.on_request_open(&session("s1"), Some(&h), &[], 1_000);
        det.on_response_stop_reason(&session("s1"), StopReason::ToolUse);
        det.on_response_close(session("s1"), &dec1, Some(h), 1_500);

        let dec2 = det.on_request_open(&session("s1"), Some(&h), &[], 2_000);
        assert_eq!(dec2.turn_id, dec1.turn_id, "same turn (tool_use)");
        assert_eq!(
            dec2.agent_run_id, dec1.agent_run_id,
            "same agent run (system hash unchanged)"
        );
        assert_eq!(dec2.agent_run_kind, AgentRunDecisionKind::Continuation);
    }

    /// Sub-agent transition mid-session: the wire `session_id` is
    /// unchanged (Claude Code propagates the parent header to
    /// sub-agents per E3 §A finding #3), but the canonical system
    /// hash changes → new `agent_run_id`, same `session_id`.
    #[test]
    fn sub_agent_transition_mints_new_agent_run_within_same_session() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());
        let h_main = SystemHash::from_bytes(b"You are Claude Code.");
        let h_sub = SystemHash::from_bytes(b"You are the Agent tool's sub-agent.");

        // Main agent RT.
        let dec1 = det.on_request_open(&session("s1"), Some(&h_main), &[], 1_000);
        det.on_response_stop_reason(&session("s1"), StopReason::EndTurn);
        det.on_response_close(session("s1"), &dec1, Some(h_main), 1_500);

        // Sub-agent RT — same wire session id, different system hash.
        let dec2 = det.on_request_open(&session("s1"), Some(&h_sub), &[], 2_000);
        assert_eq!(dec2.agent_run_kind, AgentRunDecisionKind::NewAgentRun);
        assert_ne!(
            dec2.agent_run_id, dec1.agent_run_id,
            "sub-agent transition mints fresh agent_run_id"
        );
    }

    /// `on_response_close` writes back the new `agent_run_id` so the
    /// next RT sees it. Without this, continuation reads would
    /// always see `None` and mint fresh forever.
    #[test]
    fn close_writes_back_agent_run_id() {
        let store: Arc<dyn MarkingStore> = Arc::new(InMemoryMarkingStore::new());
        let det = detector_with_counter(store.clone());
        let h = SystemHash::from_bytes(b"prompt-A");

        let dec = det.on_request_open(&session("s1"), Some(&h), &[], 1_000);
        det.on_response_stop_reason(&session("s1"), StopReason::EndTurn);
        det.on_response_close(session("s1"), &dec, Some(h), 1_500);

        let stored = store.get(&session("s1")).expect("state written");
        let run = stored.run(Some(&h)).expect("run for hash");
        assert_eq!(run.agent_run_id, dec.agent_run_id);
    }

    // ─── Header extraction ───────────────────────────────────────

    #[test]
    fn extract_session_id_reads_header_case_insensitively() {
        let mut h = http::HeaderMap::new();
        h.insert(SESSION_HEADER, "session-abc".parse().unwrap());
        assert_eq!(extract_session_id(&h), Some(session("session-abc")));
    }

    #[test]
    fn extract_session_id_returns_none_when_missing() {
        let h = http::HeaderMap::new();
        assert_eq!(extract_session_id(&h), None);
    }

    // ─── canonical-system-hash extraction (ADR 023 §2.5) ────────

    #[test]
    fn canonical_system_hash_handles_empty_body() {
        assert!(compute_canonical_system_hash(b"").is_none());
    }

    #[test]
    fn canonical_system_hash_handles_no_system_field() {
        let body = br#"{"model":"claude-3-5-sonnet-20241022","messages":[]}"#;
        assert!(compute_canonical_system_hash(body).is_none());
    }

    #[test]
    fn canonical_system_hash_handles_string_form() {
        let body = br#"{"system":"You are a helpful assistant.","messages":[]}"#;
        let hash = compute_canonical_system_hash(body).expect("hash computed");
        assert_eq!(
            hash,
            SystemHash::from_bytes(b"You are a helpful assistant.")
        );
    }

    #[test]
    fn canonical_system_hash_handles_array_form() {
        let body = br#"{
            "system":[
                {"type":"text","text":"You are Claude Code."},
                {"type":"text","text":"Reply concisely."}
            ],
            "messages":[]
        }"#;
        let hash = compute_canonical_system_hash(body).expect("hash computed");
        assert_eq!(
            hash,
            SystemHash::from_bytes(b"You are Claude Code.\nReply concisely.")
        );
    }

    #[test]
    fn canonical_system_hash_skips_billing_header_block() {
        // The billing-header block changes per-request as the cache
        // rotates; canonicalisation excludes it so the hash stays
        // stable across cache rotations.
        let body_a = br#"{
            "system":[
                {"type":"text","text":"x-anthropic-billing-header: cache-id=abc123"},
                {"type":"text","text":"You are Claude Code."}
            ]
        }"#;
        let body_b = br#"{
            "system":[
                {"type":"text","text":"x-anthropic-billing-header: cache-id=xyz789"},
                {"type":"text","text":"You are Claude Code."}
            ]
        }"#;
        let h_a = compute_canonical_system_hash(body_a).expect("a");
        let h_b = compute_canonical_system_hash(body_b).expect("b");
        assert_eq!(
            h_a, h_b,
            "billing-header block must not affect canonical hash"
        );
        assert_eq!(h_a, SystemHash::from_bytes(b"You are Claude Code."));
    }

    #[test]
    fn canonical_system_hash_distinguishes_different_prompts() {
        let a = compute_canonical_system_hash(br#"{"system":"prompt A"}"#).unwrap();
        let b = compute_canonical_system_hash(br#"{"system":"prompt B"}"#).unwrap();
        assert_ne!(a, b);
    }
}
