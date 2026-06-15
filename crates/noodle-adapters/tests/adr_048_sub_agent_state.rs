//! ADR 048 §11 item 0 — tests against real `claude -p` captures
//! that pin per-agent-run `SessionState` behavior. The two
//! "survives sub-agent run" tests were `#[ignore]`'d in PR-A
//! (proving the bug) and made to pass by PR-B's refactor.
//!
//! See `docs/adrs/048-enhance-extract-llm-self-classification.md`
//! §11 item 0, `docs/diagrams/adr-048-per-agent-run-sessionstate.md`,
//! and `docs/guides/capture-acquisition.md`.

#![allow(clippy::doc_markdown)]

mod common;

use std::sync::Arc;

use noodle_adapters::marking::{AnthropicMarkingDetector, InMemoryMarkingStore};
use noodle_core::{MarkingDetector, MarkingSessionId, MarkingStore, SessionState, StopReason};

struct ReplayResult {
    decisions: Vec<(usize, noodle_core::MarkingDecision)>,
    /// Per-turn snapshot of `(session_id, request_system_hash)`
    /// alongside the decision — lets lineage tests look up the
    /// `AgentRunState` for any turn after the replay completes.
    turn_hash: Vec<(usize, Option<noodle_core::SystemHash>)>,
    final_state: SessionState,
    session_id: MarkingSessionId,
}

impl ReplayResult {
    fn decision_for(&self, idx: usize) -> &noodle_core::MarkingDecision {
        &self
            .decisions
            .iter()
            .find(|(i, _)| *i == idx)
            .unwrap_or_else(|| panic!("no decision for turn #{idx}"))
            .1
    }

    fn run_for(&self, idx: usize) -> &noodle_core::AgentRunState {
        let hash = self
            .turn_hash
            .iter()
            .find(|(i, _)| *i == idx)
            .unwrap_or_else(|| panic!("no hash recorded for turn #{idx}"))
            .1;
        self.final_state
            .run(hash.as_ref())
            .unwrap_or_else(|| panic!("no run for turn #{idx} (hash {hash:?})"))
    }
}

/// Replay an entire fixture through the detector and collect every
/// `MarkingDecision`. The session_id is read from each turn's
/// captured metadata so we faithfully reproduce the real wire fact
/// (parent + sub-agent share one Anthropic session_id). Turns with
/// no captured session_id (rare side-calls — haiku title gen, etc.)
/// are skipped.
fn replay(fixture_name: &str) -> ReplayResult {
    let fixture = common::load_fixture(fixture_name);
    let turns = fixture["turns"].as_array().expect("turns array");

    let store = Arc::new(InMemoryMarkingStore::default());
    let detector = AnthropicMarkingDetector::new(store.clone());

    let mut decisions = Vec::with_capacity(turns.len());
    let mut turn_hash = Vec::with_capacity(turns.len());
    let mut session_id: Option<MarkingSessionId> = None;
    for turn in turns {
        let idx = usize::try_from(turn["idx"].as_u64().expect("turn idx")).expect("idx fits usize");
        let sid_str = turn["session_id"].as_str().unwrap_or("");
        if sid_str.is_empty() {
            continue;
        }
        let sid = MarkingSessionId::new(sid_str);
        session_id.get_or_insert_with(|| sid.clone());
        let hash = common::turn_system_hash(turn);
        let first_user_hashes = common::turn_first_user_hashes(turn);

        let decision = detector.on_request_open(&sid, hash.as_ref(), &first_user_hashes, 0);

        // Drive tool_use observation in wire order: content blocks
        // arrive in the SSE stream before the terminal
        // `message_delta.stop_reason`. PR-C1: the detector pushes
        // a pending child on `tool_use(Task|Agent)`; the request
        // carrying the spawn's prompt fingerprint pops it and
        // inherits the lineage (ADR 048 gap review §6.R2).
        if let Some(tool_uses) = turn["response"]["tool_uses"].as_array() {
            for tu in tool_uses {
                let name = tu["name"].as_str().unwrap_or("");
                let id = tu["id"].as_str().unwrap_or("");
                let prompt_hash = common::tool_use_prompt_hash(tu);
                detector.on_response_tool_use(&sid, name, id, prompt_hash);
            }
        }

        if let Some(stop_str) = turn["response"]["stop_reason"].as_str() {
            let stop = StopReason::from_wire(stop_str);
            detector.on_response_stop_reason(&sid, stop);
        }

        detector.on_response_close(sid, &decision, hash, 0);
        decisions.push((idx, decision));
        turn_hash.push((idx, hash));
    }
    let session_id = session_id.expect("at least one turn with a session_id");
    let final_state = store
        .get(&session_id)
        .expect("session state present after replay");
    ReplayResult {
        decisions,
        turn_hash,
        final_state,
        session_id,
    }
}

/// ADR 048 §11 item 0 — canonical case from
/// `parent-task-subagent.mitm`.
///
/// The capture's shape (verified by the fixture):
///
/// | turn | sys_blocks | canonical hash | stop_reason  | role        |
/// |------|-----------:|----------------|--------------|-------------|
/// | #1   | 4          | 1e4263617847   | tool_use     | parent      |
/// | #2-6 | 3          | 1157baf22b05   | tool_use…end_turn | sub-agent A |
/// | #7   | 3          | f7a4ce75d456   | stop_sequence| side-call   |
/// | #8   | 4          | 1e4263617847   | end_turn     | parent      |
///
/// All 8 turns share one Anthropic session_id. Parent's #1 ends
/// with `stop=tool_use` (waiting for the `Agent` tool result);
/// turn #8 is the parent resuming with the sub-agent's tool_result.
/// A correct detector stamps #1 and #8 with the **same**
/// `turn_id` (the parent's tool_use continuation) and the **same**
/// `agent_run_id` (the parent agent run). The old single-slot
/// `SessionState` could not.
#[test]
fn parent_turn_id_survives_sub_agent_run() {
    let r = replay("parent-task-subagent");
    let parent_first = r.decision_for(1);
    let parent_resume = r.decision_for(8);

    assert_eq!(
        parent_first.turn_id, parent_resume.turn_id,
        "parent turn_id was overwritten by sub-agent lifecycle: \
         #1 minted {:?} but #8 minted {:?}. \
         Per-agent-run SessionState (ADR 048 §11 item 0) is required.",
        parent_first.turn_id, parent_resume.turn_id,
    );
}

#[test]
fn parent_agent_run_id_survives_sub_agent_run() {
    let r = replay("parent-task-subagent");
    let parent_first = r.decision_for(1);
    let parent_resume = r.decision_for(8);

    assert_eq!(
        parent_first.agent_run_id, parent_resume.agent_run_id,
        "parent agent_run_id was overwritten by the sub-agent's \
         system-prompt hash: #1 minted {:?} but #8 minted {:?}.",
        parent_first.agent_run_id, parent_resume.agent_run_id,
    );
}

/// Sub-agent's own turn_id must be stable across its internal
/// tool-use chain. Turns #2-6 share canonical hash 1157baf22b05;
/// #2 starts with stop=tool_use and every continuation should keep
/// the same turn_id until #6 closes with stop=end_turn.
#[test]
fn sub_agent_turn_id_stable_across_its_tool_use_chain() {
    let r = replay("parent-task-subagent");
    let s2 = r.decision_for(2);
    let s6 = r.decision_for(6);

    assert_eq!(
        s2.turn_id, s6.turn_id,
        "sub-agent turn_id changed mid-tool_use-chain: #2 was {:?}, #6 was {:?}",
        s2.turn_id, s6.turn_id,
    );
}

// ─── PR-C1 lineage tests ─────────────────────────────────────────

/// PR-C1 — root agent runs have no parent. Turn #1 (the
/// parent's first request) opens the session cold, so its
/// `AgentRunState.lineage` is `None`.
#[test]
fn parent_run_has_no_parent() {
    let r = replay("parent-task-subagent");
    let parent_run = r.run_for(1);
    assert!(
        parent_run.lineage.is_none(),
        "root parent run unexpectedly carries lineage: {:?}",
        parent_run.lineage,
    );
}

/// PR-C1 — the `Task`/`Agent` tool_use on the parent's response
/// (#1) pushes a pending child onto the per-session stack. The
/// sub-agent's first request (#2, `NewAgentRun` decision) pops
/// it and the sub-agent's `AgentRunState.lineage` carries the
/// parent's ids plus the wire `tool_use.id`.
#[test]
fn sub_agent_run_carries_parent_lineage() {
    let r = replay("parent-task-subagent");
    let parent_decision = r.decision_for(1);
    let sub_agent_run = r.run_for(2);
    let lineage = sub_agent_run
        .lineage
        .as_ref()
        .expect("sub-agent #2 should carry lineage popped from parent #1's Task tool_use");

    assert_eq!(
        lineage.turn_id, parent_decision.turn_id,
        "lineage.turn_id should equal parent's #1 turn_id",
    );
    assert_eq!(
        lineage.agent_run_id, parent_decision.agent_run_id,
        "lineage.agent_run_id should equal parent's #1 agent_run_id",
    );
    assert_eq!(
        lineage.session_id, r.session_id,
        "lineage.session_id should match the shared wire session",
    );
    assert_eq!(
        lineage.tool_use_id.as_str(),
        "toolu_012Y8jeMfYYbNWTHPS1Nujbw",
        "lineage.tool_use_id should be the parent's Agent tool_use id from the capture",
    );
}

/// PR-C1 — `Continuation` and `NewTurn` decisions on the same
/// slot preserve the lineage written on the first sighting. The
/// sub-agent's turns #2-6 all live in the same slot; after #6
/// closes, that slot's lineage should still point at the parent.
#[test]
fn sub_agent_lineage_preserved_across_continuations() {
    let r = replay("parent-task-subagent");
    let after_first = r
        .run_for(2)
        .lineage
        .as_ref()
        .expect("lineage at #2")
        .clone();
    let after_last = r
        .run_for(6)
        .lineage
        .as_ref()
        .expect("lineage at #6 should still be present");
    assert_eq!(
        &after_first, after_last,
        "sub-agent slot lineage drifted across the tool_use chain",
    );
}

/// PR-C1 — a `NewAgentRun` decision with **no** pending parent on
/// the stack (e.g. an internal classifier spawned by the harness,
/// not via a visible `Task`/`Agent` tool_use) leaves lineage as
/// `None`. Turn #7's canonical hash differs from any prior turn,
/// and the only `Task` tool_use was already consumed by #2, so #7
/// is a fresh run with no observable parent at the wire.
#[test]
fn unrelated_new_agent_run_has_no_lineage_when_stack_empty() {
    let r = replay("parent-task-subagent");
    let unrelated = r.run_for(7);
    assert!(
        unrelated.lineage.is_none(),
        "turn #7's slot unexpectedly carries lineage: {:?}",
        unrelated.lineage,
    );
}

// ─── Baseline (single-agent) ─────────────────────────────────────

/// `parent-bash-loop.mitm` — 4 turns, single session, one parent
/// agent run (all canonical hashes match). The first three end
/// with `stop=tool_use`; the fourth ends with `stop=end_turn`.
/// All four belong to the SAME turn_id (one logical user turn with
/// three Bash tool round-trips) and to the SAME agent_run_id.
#[test]
fn bash_loop_single_turn_single_agent_run() {
    let r = replay("parent-bash-loop");
    assert_eq!(r.decisions.len(), 4, "expected 4 captured turns");

    let first = &r.decisions[0].1;
    for (idx, decision) in &r.decisions {
        assert_eq!(
            decision.turn_id, first.turn_id,
            "turn #{idx}: bash-loop turn_id drifted from #1"
        );
        assert_eq!(
            decision.agent_run_id, first.agent_run_id,
            "turn #{idx}: bash-loop agent_run_id drifted from #1"
        );
    }
}

// ─── R2: fingerprint-matched lineage (ADR 048 gap review §6.R2) ──

/// Build a deterministic SystemHash from a label.
fn h(label: &str) -> noodle_core::SystemHash {
    noodle_core::SystemHash::from_bytes(label.as_bytes())
}

fn fresh_detector() -> (AnthropicMarkingDetector, Arc<InMemoryMarkingStore>) {
    let store = Arc::new(InMemoryMarkingStore::default());
    (AnthropicMarkingDetector::new(store.clone()), store)
}

/// The G1 steal interleaving: a side-call (title-gen, quota probe,
/// compactor) opening between a parent's spawn and the true
/// child's first request must NOT inherit the pending child's
/// lineage — and the true child, arriving later, still must.
///
/// Pre-R2 the pending-children stack popped on ANY `NewAgentRun`,
/// so the side-call stole the `ParentRunRef` and the real
/// sub-agent went unattributed. The fingerprint match closes this:
/// only the request carrying the spawn's `input.prompt` as a
/// first-user text block pops the entry.
#[test]
fn interposed_side_call_cannot_steal_pending_lineage() {
    let (det, _store) = fresh_detector();
    let sid = MarkingSessionId::new("s-steal");
    let parent_hash = h("parent-system");
    let prompt = h("spawn-prompt-text");

    // Parent RT: opens fresh, spawns a sub-agent, stops tool_use.
    let d1 = det.on_request_open(&sid, Some(&parent_hash), &[], 0);
    det.on_response_tool_use(&sid, "Agent", "toolu_SPAWN", Some(prompt));
    det.on_response_stop_reason(&sid, StopReason::ToolUse);
    det.on_response_close(sid.clone(), &d1, Some(parent_hash), 0);

    // Side-call opens FIRST: unseen system hash, first-user blocks
    // that do NOT carry the spawn prompt.
    let side_hash = h("title-gen-system");
    let d2 = det.on_request_open(&sid, Some(&side_hash), &[h("title-gen-user-text")], 1);
    assert_eq!(d2.kind, noodle_core::MarkingDecisionKind::NewAgentRun);
    assert!(
        d2.lineage.is_none(),
        "side-call stole the pending child's lineage: {:?}",
        d2.lineage
    );
    det.on_response_stop_reason(&sid, StopReason::StopSequence);
    det.on_response_close(sid.clone(), &d2, Some(side_hash), 1);

    // True child opens SECOND: unseen hash, first-user blocks
    // carrying the spawn prompt verbatim.
    let child_hash = h("sub-agent-system");
    let d3 = det.on_request_open(&sid, Some(&child_hash), &[h("other-block"), prompt], 2);
    assert_eq!(d3.kind, noodle_core::MarkingDecisionKind::NewAgentRun);
    let lineage = d3
        .lineage
        .as_ref()
        .expect("true child must inherit lineage");
    assert_eq!(lineage.tool_use_id.as_str(), "toolu_SPAWN");
    assert_eq!(
        lineage.turn_id, d1.turn_id,
        "lineage credits the parent's turn"
    );
    assert_eq!(
        lineage.agent_run_id, d1.agent_run_id,
        "lineage credits the parent's agent run"
    );
}

/// Concurrent spawns whose children arrive out of LIFO order must
/// each attribute to their own spawn. Pre-R2 the blind LIFO pop
/// paired the first-arriving child with the LAST-pushed spawn.
#[test]
fn out_of_order_children_attribute_to_their_own_spawns() {
    let (det, _store) = fresh_detector();
    let sid = MarkingSessionId::new("s-concurrent");
    let parent_hash = h("parent-system");
    let prompt_a = h("prompt-A");
    let prompt_b = h("prompt-B");

    let d1 = det.on_request_open(&sid, Some(&parent_hash), &[], 0);
    det.on_response_tool_use(&sid, "Task", "toolu_A", Some(prompt_a));
    det.on_response_tool_use(&sid, "Task", "toolu_B", Some(prompt_b));
    det.on_response_stop_reason(&sid, StopReason::ToolUse);
    det.on_response_close(sid.clone(), &d1, Some(parent_hash), 0);

    // Child A opens first — under LIFO it would have received B.
    let da = det.on_request_open(&sid, Some(&h("child-A-system")), &[prompt_a], 1);
    assert_eq!(
        da.lineage.as_ref().map(|l| l.tool_use_id.as_str()),
        Some("toolu_A"),
        "child A must match spawn A regardless of push order"
    );
    let db = det.on_request_open(&sid, Some(&h("child-B-system")), &[prompt_b], 2);
    assert_eq!(
        db.lineage.as_ref().map(|l| l.tool_use_id.as_str()),
        Some("toolu_B"),
        "child B must match spawn B"
    );
}

/// A spawn whose `tool_use.input` carried no `prompt` string is
/// unmatchable: its child degrades to unattributed (lineage None),
/// and nothing else can claim the entry either.
#[test]
fn promptless_spawn_degrades_to_unattributed() {
    let (det, _store) = fresh_detector();
    let sid = MarkingSessionId::new("s-promptless");
    let parent_hash = h("parent-system");

    let d1 = det.on_request_open(&sid, Some(&parent_hash), &[], 0);
    det.on_response_tool_use(&sid, "Agent", "toolu_NOPROMPT", None);
    det.on_response_stop_reason(&sid, StopReason::ToolUse);
    det.on_response_close(sid.clone(), &d1, Some(parent_hash), 0);

    let d2 = det.on_request_open(
        &sid,
        Some(&h("child-system")),
        &[h("whatever-user-text")],
        1,
    );
    assert_eq!(d2.kind, noodle_core::MarkingDecisionKind::NewAgentRun);
    assert!(
        d2.lineage.is_none(),
        "promptless spawn must never match: {:?}",
        d2.lineage
    );
}
