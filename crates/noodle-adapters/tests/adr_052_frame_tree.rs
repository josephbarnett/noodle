//! ADR 052 §6 replay — drive the header-driven frame-tree detector over the
//! real `claude-parallel-subagents` capture and assert the §5 marks against the
//! known frame tree (one turn; main + three parallel sub-agents). Exercises the
//! full edge path: `frame_signals::request_signals` (body → side-call) + the
//! `agent_id` header → `FrameTreeDetector`.
//!
//! Capture: `analysis/claude-parallel-subagents/NNN_*_request.json`, each
//! carrying a `_link` block (thread, stop_reason) and a `headers` object with
//! `x-claude-code-agent-id` on sub-agent round-trips.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use noodle_adapters::marking::frame_signals::request_signals;
use noodle_adapters::marking::{FrameRole, FrameTreeDetector, RoundTripSignals};
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

#[test]
fn claude_parallel_subagents_reconstructs_one_turn_four_frames() {
    let dir = capture_dir("claude-parallel-subagents");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {dir:?}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_request.json"))
        })
        .collect();
    files.sort();
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
