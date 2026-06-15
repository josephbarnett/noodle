//! Golden test for the `content.blocks[]` on-disk JSON shape
//! (ADR 030 §2, refactor overview §2 S9).
//!
//! Verifies the JSONL line a `WireEvent` with populated
//! `content_blocks` produces matches ADR 030 §2.1 / §2.2
//! exactly. Downstream consumers (the viewer, the
//! `noodle-embellish` `SQLite` emitter, the `ai-telemetry`
//! v0.0.2 mapping) parse these positions — drift here breaks
//! everything downstream of the proxy.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use noodle_core::{HeaderPair, WireDirection, WireEvent, WireSink};
use noodle_tap::TapJsonlLog;
use serde_json::Value;
use tempfile::tempdir;

/// Build a response `WireEvent` with `content_blocks` populated
/// as the proxy would after running its
/// `ContentBlocksAccumulator` over an Anthropic SSE stream that
/// produced one `text`, one `thinking`, and one `tool_use`
/// block.
fn response_with_decoded_blocks() -> WireEvent {
    let blocks = serde_json::json!([
        { "kind": "text", "text": "Hello there." },
        {
            "kind": "thinking",
            "text": "I should respond.",
            "signature": "sig_abc"
        },
        {
            "kind": "tool_use",
            "tool_use_id": "tu_01ABCDEF",
            "tool_name": "Read",
            "input": { "path": "/repo/main.rs" }
        }
    ]);
    WireEvent {
        direction: WireDirection::Response,
        request_id: "nl-1".into(),
        ts_unix_ms: 1_700_000_000_000,
        method: None,
        url: None,
        status: Some(200),
        headers: vec![HeaderPair {
            name: "Content-Type".into(),
            value: "text/event-stream".into(),
        }],
        body_in: Bytes::from_static(b"event: message_start\n\n"),
        body_out: Bytes::from_static(b"event: message_start\n\n"),
        marks: None,
        provider: Some("anthropic".into()),
        agent_app: None,
        machine: None,
        collector_app: None,
        subscription: None,
        usage: None,
        content_blocks: Some(blocks),
        events: None,
        pairing: None,
        attribution: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_blocks_serialize_under_content_blocks_per_adr_030() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    sink.record(response_with_decoded_blocks());

    // Drain the writer.
    let sink_owned = Arc::try_unwrap(sink)
        .map_err(|_| "sink still has Arc holders")
        .unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let line = contents.lines().next().expect("at least one line");
    let v: Value = serde_json::from_str(line).expect("parse tap.jsonl line");

    // ADR 030 §2.1: `content.blocks[]` exactly.
    let blocks = v["content"]["blocks"]
        .as_array()
        .expect("content.blocks is an array");
    assert_eq!(blocks.len(), 3);

    // ADR 030 §2.2 — `text` block.
    assert_eq!(blocks[0]["kind"], "text");
    assert_eq!(blocks[0]["text"], "Hello there.");
    assert!(
        blocks[0].get("signature").is_none(),
        "text block must not carry a signature field"
    );

    // ADR 030 §2.2 — `thinking` block carries text + signature.
    assert_eq!(blocks[1]["kind"], "thinking");
    assert_eq!(blocks[1]["text"], "I should respond.");
    assert_eq!(blocks[1]["signature"], "sig_abc");

    // ADR 030 §2.2 — `tool_use` carries tool_use_id, tool_name,
    // input as a JSON value (§2.1's worked example).
    assert_eq!(blocks[2]["kind"], "tool_use");
    assert_eq!(blocks[2]["tool_use_id"], "tu_01ABCDEF");
    assert_eq!(blocks[2]["tool_name"], "Read");
    assert_eq!(blocks[2]["input"]["path"], "/repo/main.rs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_block_omitted_when_wire_event_has_no_content_blocks() {
    // The `content` block is `skip_serializing_if = Option::is_none`
    // so passthrough records (no codec, non-SSE, etc.) stay
    // byte-identical to pre-S9.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    let mut ev = response_with_decoded_blocks();
    ev.content_blocks = None;
    sink.record(ev);

    let sink_owned = Arc::try_unwrap(sink)
        .map_err(|_| "sink still has Arc holders")
        .unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let line = contents.lines().next().expect("at least one line");
    let v: Value = serde_json::from_str(line).expect("parse tap.jsonl line");
    assert!(
        v.get("content").is_none(),
        "passthrough record must not carry a `content` block — got: {line}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_block_round_trips_across_writer_with_timeout() {
    // Defensive: the writer task does its work async; the
    // shutdown_drain story (covered by `shutdown_drain.rs`)
    // already asserts the flush contract. This test confirms
    // the `content` field specifically survives that path —
    // a regression that drops it on the floor (e.g. a serde
    // tag-collision) would surface as `null` in the JSON.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    sink.record(response_with_decoded_blocks());

    // Give the writer task a small explicit window to flush;
    // belt-and-suspenders with the `shutdown` call below.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sink_owned = Arc::try_unwrap(sink).map_err(|_| "sink Arc").unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let v: Value =
        serde_json::from_str(contents.lines().next().expect("line")).expect("parse tap.jsonl line");
    let blocks = v["content"]["blocks"].as_array().expect("blocks array");
    assert_eq!(blocks.len(), 3);
    // Spot-check positional indexing to catch any order drift.
    assert_eq!(blocks[0]["kind"], "text");
    assert_eq!(blocks[2]["kind"], "tool_use");
}
