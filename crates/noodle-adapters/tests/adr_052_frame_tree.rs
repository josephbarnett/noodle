//! ADR 052 §6 replay — drive the header-driven frame-tree detector over real
//! captures and assert the §5 marks against the known frame tree.
//!
//! - `claude-parallel-subagents`: one turn, main + three parallel sub-agents.
//!   Exercises the edge path `frame_signals::request_signals` (body → side-call)
//!   + the `x-claude-code-agent-id` header → `FrameTreeDetector`.
//! - `opencode-multi-prompt`: four turns, three sub-agents. Exercises the
//!   client-agnostic mapping (frame=session, `x-parent-session-id` → root
//!   session) feeding the same detector via `FrameTreeRegistry`.
//!
//! Each request JSON carries a `_link` block (thread, `stop_reason`) and a
//! `headers` object; the `index.md` in each dir is the golden frame tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use noodle_adapters::marking::frame_signals::request_signals;
use noodle_adapters::marking::{FrameRole, FrameTreeDetector, FrameTreeRegistry, RoundTripSignals};
use noodle_core::MarkingSessionId;
use serde_json::Value;

fn capture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../analysis")
        .join(name)
}

fn header<'a>(headers: &'a Value, key: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .and_then(|(_, v)| v.as_str())
}

fn request_files(dir: &PathBuf) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_request.json"))
        })
        .collect();
    files.sort();
    files
}

#[test]
fn claude_parallel_subagents_reconstructs_one_turn_four_frames() {
    let files = request_files(&capture_dir("claude-parallel-subagents"));
    assert_eq!(files.len(), 12, "12 round trips (depth-first 001..012)");

    // Deterministic turn ids so the assertion is stable.
    let mut det = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
    let mut frames = BTreeSet::new();

    for path in &files {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let link = &v["_link"];
        let thread = link["thread"].as_str().unwrap();
        let agent_id = header(&v["headers"], "x-claude-code-agent-id").map(str::to_string);

        // Edge path: body-derived signals, then stamp the header-derived agent id.
        let body = serde_json::to_vec(&v["body"]).unwrap();
        let s = request_signals(&body);
        let m = det.on_round_trip(&RoundTripSignals {
            max_tokens: s.max_tokens,
            trailing_wrapper_kind: s.trailing_wrapper_kind,
            agent_id: agent_id.clone(),
            side_call: s.side_call,
            stop_reason: link["stop_reason"].as_str().map(str::to_string),
            response_tool_uses: Vec::new(),
        });

        assert_eq!(m.turn_id.as_deref(), Some("turn-1"), "{name}: single turn");
        if thread == "main" {
            assert_eq!(m.role, FrameRole::Main, "{name}");
            assert_eq!(m.frame_id.as_deref(), Some("ROOT"), "{name}");
            assert_eq!(m.depth, Some(0), "{name}");
        } else {
            assert_eq!(m.role, FrameRole::SubAgent, "{name}");
            assert_eq!(m.frame_id, agent_id, "{name}: frame_id == agent id");
            assert_eq!(m.parent_frame_id.as_deref(), Some("ROOT"), "{name}");
            assert_eq!(m.depth, Some(1), "{name}");
        }
        if let Some(f) = m.frame_id {
            frames.insert(f);
        }
    }

    // ROOT + three distinct sub-agent frames.
    assert_eq!(frames.len(), 4, "main + 3 parallel sub-agent frames");
}

#[test]
fn opencode_multi_prompt_nests_subagents_in_their_turns() {
    let files = request_files(&capture_dir("opencode-multi-prompt"));
    assert_eq!(files.len(), 27, "27 round trips (depth-first 001..027)");

    // OpenCode frames are sessions; the proxy maps them to the CC-shaped
    // (root session, agent id) the detector expects, so all frames key under
    // the root session and turns span them.
    let reg = FrameTreeRegistry::new();
    let mut rows: Vec<Option<String>> = Vec::new(); // turn_id per round trip
    let mut frames = BTreeSet::new();
    let mut turns = BTreeSet::new();

    for path in &files {
        let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let link = &v["_link"];
        let thread = link["thread"].as_str().unwrap().to_string();
        let oc = header(&v["headers"], "x-session-id").unwrap().to_string();
        let parent = header(&v["headers"], "x-parent-session-id").map(str::to_string);
        let (session_str, agent_id) = match &parent {
            Some(p) => (p.clone(), Some(oc.clone())),
            None => (oc.clone(), None),
        };

        let body = serde_json::to_vec(&v["body"]).unwrap();
        let s = request_signals(&body);
        let m = reg.on_round_trip(
            &MarkingSessionId::new(session_str.as_str()),
            &RoundTripSignals {
                max_tokens: s.max_tokens,
                trailing_wrapper_kind: s.trailing_wrapper_kind,
                agent_id: agent_id.clone(),
                side_call: s.side_call,
                stop_reason: link["stop_reason"].as_str().map(str::to_string),
                response_tool_uses: Vec::new(),
            },
        );

        if thread == "main" {
            assert_eq!(m.role, FrameRole::Main, "{thread}");
            assert_eq!(m.frame_id.as_deref(), Some("ROOT"));
            assert_eq!(m.depth, Some(0));
        } else {
            assert_eq!(m.role, FrameRole::SubAgent, "{thread}");
            assert_eq!(m.frame_id, agent_id, "frame_id == the sub-agent session");
            assert_eq!(m.parent_frame_id.as_deref(), Some("ROOT"));
            assert_eq!(m.depth, Some(1));
        }
        if let Some(f) = &m.frame_id {
            frames.insert(f.clone());
        }
        if let Some(t) = &m.turn_id {
            turns.insert(t.clone());
        }
        rows.push(m.turn_id);
    }

    // ROOT + three distinct sub-agent frames; four turns (main stop_reasons:
    // tool_use, end, end, tool_use, end, tool_use, end).
    assert_eq!(frames.len(), 4, "ROOT + 3 sub-agent frames");
    assert_eq!(turns.len(), 4, "four turns");

    // Sub-agents inherit their spawning turn (rows are 0-based; seq is 1-based).
    let turn = |i: usize| rows[i].clone();
    assert_eq!(
        turn(4),
        turn(3),
        "subagent_1 (seq5) shares main rt3's turn (seq4)"
    );
    assert_eq!(turn(16), turn(3), "main rt4 (seq17) is still rt3's turn");
    assert_eq!(
        turn(18),
        turn(17),
        "subagent_2 (seq19) shares main rt5's turn (seq18)"
    );
    assert_eq!(turn(26), turn(17), "main rt6 (seq27) is still rt5's turn");
}
