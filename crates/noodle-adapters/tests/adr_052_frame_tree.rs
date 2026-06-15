//! ADR 052 §6 golden-replay — drive the real Rust frame-tree detector over the
//! sanitized capture fixtures and assert the §5 marks per round-trip against the
//! checked-in goldens. Deterministic, no network, no auth.
//!
//! Fixtures + goldens are produced by `tools/build_052_fixtures.py` from the
//! gitignored `captures/max/*.mitm` (one signal extraction feeds both, so the
//! oracle and this detector cannot drift). Re-build after recapturing.
//!
//! Scope (ADR 052 §9): single-turn / single-session reconstruction — CHAIN +
//! SPAWN routing, the depth-0 turn boundary, the quota/title/monitor/suggestion
//! wrapper catalog, and parallel sibling frames. The detector is driven
//! unpartitioned across `long-session-compaction`'s two session ids, matching
//! the oracle; per-session partitioning is V3.

use std::path::{Path, PathBuf};

use noodle_adapters::marking::{FrameTreeDetector, RoundTripSignals, ToolUse};
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    PathBuf::from(manifest).join("tests/fixtures/adr_052")
}

fn load(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap_or_else(|err| {
        panic!(
            "read {}: {err} — re-build with `mitmdump -nq -r captures/max/<name>.mitm -s tools/build_052_fixtures.py --set name=<name> --set fixture_out=… --set golden_out=…`",
            path.display()
        )
    });
    serde_json::from_slice(&bytes).expect("json parse")
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn signals_from(rt: &Value) -> RoundTripSignals {
    let response_tool_uses = rt
        .get("response_tool_uses")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|tu| ToolUse {
                    name: tu["name"].as_str().unwrap_or("").to_string(),
                    id: tu["id"].as_str().unwrap_or("").to_string(),
                    prompt_sha256: tu
                        .get("prompt_sha256")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    RoundTripSignals {
        max_tokens: rt.get("max_tokens").and_then(Value::as_u64),
        request_tool_result_ids: strings(rt, "request_tool_result_ids"),
        first_user_text_sha256s: strings(rt, "first_user_text_sha256s"),
        trailing_wrapper_kind: rt
            .get("trailing_wrapper_kind")
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string(),
        has_genuine_user_text: rt
            .get("has_genuine_user_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        message_sig: strings(rt, "message_sig"),
        stop_reason: rt
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        response_tool_uses,
    }
}

fn replay(name: &str) {
    let fixture = load(&fixtures_dir().join(format!("{name}.fixture.json")));
    let golden = load(&fixtures_dir().join(format!("expected_marks/{name}.json")));
    let rts = fixture["round_trips"]
        .as_array()
        .expect("round_trips array");
    let expected = golden["marks"].as_array().expect("marks array");
    assert_eq!(
        rts.len(),
        expected.len(),
        "{name}: fixture / golden length mismatch"
    );

    // Deterministic turn ids so the goldens stay stable; production mints a
    // ULID per turn (FrameTreeDetector::new).
    let mut det = FrameTreeDetector::with_turn_mint(|n| format!("turn-{n}"));
    for (i, (rt, g)) in rts.iter().zip(expected.iter()).enumerate() {
        let m = det.on_round_trip(&signals_from(rt));
        let rt_no = i + 1;
        assert_eq!(
            m.role.as_str(),
            g["role"].as_str().unwrap(),
            "{name} RT{rt_no}: role"
        );
        assert_eq!(
            m.frame_id.as_deref(),
            g["frame_id"].as_str(),
            "{name} RT{rt_no}: frame_id"
        );
        assert_eq!(
            m.parent_frame_id.as_deref(),
            g["parent_frame_id"].as_str(),
            "{name} RT{rt_no}: parent_frame_id"
        );
        assert_eq!(
            m.depth.map(u64::from),
            g["depth"].as_u64(),
            "{name} RT{rt_no}: depth"
        );
        assert_eq!(
            m.turn_id.as_deref(),
            g["turn_id"].as_str(),
            "{name} RT{rt_no}: turn_id"
        );
    }
}

#[test]
fn bash_loop_is_one_root_turn() {
    replay("parent-bash-loop");
}

#[test]
fn sequential_sub_agent_under_root_one_turn() {
    replay("parent-task-subagent");
}

#[test]
fn parallel_siblings_one_turn_side_calls_off_tree() {
    replay("parent-parallel-subagents");
}

#[test]
fn quota_and_title_pure_text_root_seed() {
    replay("quota-and-title");
}

#[test]
fn long_session_compaction_side_calls() {
    replay("long-session-compaction");
}
