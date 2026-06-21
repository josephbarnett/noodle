//! Story 060 acceptance — turn-grouped hierarchical spans over the committed
//! `claude-parallel-subagents` capture.
//!
//! Reconstructs the corpus through the **real** detector + row builder
//! ([`noodle_trace_emitter::reconstruct`]) and runs it through the **real**
//! story-060 span builder ([`build_resource_spans_payload`]), then asserts the
//! whole-capture trace shape the offline emitter and the in-cluster shipper both
//! produce: one turn → one trace, four `invoke_agent` frames (ROOT + 3 parallel
//! sub-agents), twelve `chat` leaves, each parented correctly.
//!
//! This is the corpus-level guard story 060 calls for — the sibling
//! `exporter.rs` unit test proves the same parenting on hand-built rows; this
//! proves it on the real reconstructed capture, so a regression in the detector
//! → row → span path is caught against real data.

use std::path::PathBuf;

use noodle_shipper::exporter::build_resource_spans_payload;
use noodle_trace_emitter::{DEFAULT_CAPTURE, reconstruct};
use serde_json::Value;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(DEFAULT_CAPTURE)
}

fn span_list(payload: &Value) -> Vec<Value> {
    payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("spans array")
        .clone()
}

/// True when this span's `frame_id` attribute equals `frame`.
fn is_frame(span: &Value, frame: &str) -> bool {
    span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["key"] == "frame_id" && a["value"]["stringValue"] == frame)
}

#[test]
fn parallel_subagents_corpus_assembles_one_turn_four_frames_twelve_chats() {
    let rows = reconstruct(&corpus_dir()).expect("reconstruct corpus");
    assert_eq!(rows.len(), 12, "12 round-trips in the capture");

    let spans = span_list(&build_resource_spans_payload(&rows));

    // ADR 057 span kinds: 1 = INTERNAL (invoke_agent frame), 3 = CLIENT (chat).
    let chat: Vec<&Value> = spans.iter().filter(|s| s["kind"] == 3_i64).collect();
    let agents: Vec<&Value> = spans.iter().filter(|s| s["kind"] == 1_i64).collect();
    assert_eq!(chat.len(), 12, "one chat span per round-trip");
    assert_eq!(
        agents.len(),
        4,
        "one invoke_agent span per (turn, frame): ROOT + 3 sub-agents"
    );

    // One turn → one trace: every span shares the same trace id.
    let trace_id = spans[0]["traceId"].clone();
    assert!(
        trace_id.as_str().is_some_and(|t| t.len() == 32),
        "16-byte trace id"
    );
    for s in &spans {
        assert_eq!(s["traceId"], trace_id, "all spans on the one turn trace");
    }

    // ROOT frame is the trace root; the three sub-agent frames parent to it.
    let root_frame = agents
        .iter()
        .find(|s| is_frame(s, "ROOT"))
        .expect("ROOT invoke_agent span");
    assert!(
        root_frame.get("parentSpanId").is_none(),
        "ROOT invoke_agent span is the trace root"
    );
    let sub_frames: Vec<&&Value> = agents.iter().filter(|s| !is_frame(s, "ROOT")).collect();
    assert_eq!(sub_frames.len(), 3, "three distinct sub-agent frames");
    for sub in &sub_frames {
        assert_eq!(
            sub["parentSpanId"], root_frame["spanId"],
            "sub-agent frame ← ROOT frame"
        );
    }

    // Every chat leaf parents to its own frame's invoke_agent span.
    let frame_span_ids: std::collections::HashMap<String, Value> = agents
        .iter()
        .map(|s| {
            let frame = s["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "frame_id")
                .and_then(|a| a["value"]["stringValue"].as_str())
                .unwrap()
                .to_owned();
            (frame, s["spanId"].clone())
        })
        .collect();
    for c in &chat {
        let frame = c["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == "frame_id")
            .and_then(|a| a["value"]["stringValue"].as_str())
            .expect("in-tree chat carries a frame_id");
        let want_parent = frame_span_ids.get(frame).expect("chat's frame has a span");
        assert_eq!(
            &c["parentSpanId"], want_parent,
            "chat leaf ← its frame's invoke_agent span (frame {frame})"
        );
    }
}
