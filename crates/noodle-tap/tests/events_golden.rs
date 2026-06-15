//! Golden test for the `events[]` on-disk JSON shape (ADR 030
//! §3, refactor overview §2 S10).
//!
//! Verifies the JSONL line a `WireEvent` with populated `events`
//! produces matches ADR 030 §3.1 exactly. Downstream consumers
//! (the viewer's OODA projection, the `noodle-embellish` `SQLite`
//! emitter, the `ai-telemetry` v0.0.2 mapping) parse these
//! positions — drift here breaks everything downstream of the
//! proxy.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use noodle_core::{HeaderPair, WireDirection, WireEvent, WireSink};
use noodle_tap::TapJsonlLog;
use serde_json::Value;
use tempfile::tempdir;

/// Build a response `WireEvent` with `events` populated as the
/// proxy would after running its `EventsAccumulator` over an
/// Anthropic SSE stream of `message_start` → `content_block_*`
/// → `message_delta` → `message_stop`.
fn response_with_parsed_events() -> WireEvent {
    let events = serde_json::json!([
        {
            "ts_offset_ms": 0,
            "type": "message_start",
            "message": { "id": "msg_01XYZ", "model": "claude-haiku-4-5",
                         "usage": { "input_tokens": 1024 } }
        },
        {
            "ts_offset_ms": 18,
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text" }
        },
        {
            "ts_offset_ms": 22,
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "Hello" }
        },
        {
            "ts_offset_ms": 156,
            "type": "content_block_stop",
            "index": 0
        },
        {
            "ts_offset_ms": 158,
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": { "output_tokens": 42 }
        },
        {
            "ts_offset_ms": 159,
            "type": "message_stop"
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
        content_blocks: None,
        events: Some(events),
        pairing: None,
        attribution: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_serialize_under_top_level_events_per_adr_030() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    sink.record(response_with_parsed_events());

    // Drain the writer.
    let sink_owned = Arc::try_unwrap(sink)
        .map_err(|_| "sink still has Arc holders")
        .unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let line = contents.lines().next().expect("at least one line");
    let v: Value = serde_json::from_str(line).expect("parse tap.jsonl line");

    // ADR 030 §3.1: `events[]` is a top-level field on response
    // records.
    let events = v["events"].as_array().expect("events is an array");
    assert_eq!(events.len(), 6);

    // §3.1 worked example — every event carries `ts_offset_ms`
    // and `type` plus a flattened payload.
    assert_eq!(events[0]["ts_offset_ms"], 0);
    assert_eq!(events[0]["type"], "message_start");
    assert_eq!(events[0]["message"]["id"], "msg_01XYZ");
    assert_eq!(events[0]["message"]["model"], "claude-haiku-4-5");
    assert_eq!(events[0]["message"]["usage"]["input_tokens"], 1024);

    assert_eq!(events[1]["type"], "content_block_start");
    assert_eq!(events[1]["index"], 0);
    assert_eq!(events[1]["content_block"]["type"], "text");

    assert_eq!(events[2]["type"], "content_block_delta");
    assert_eq!(events[2]["delta"]["text"], "Hello");

    assert_eq!(events[4]["type"], "message_delta");
    assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
    assert_eq!(events[4]["usage"]["output_tokens"], 42);

    assert_eq!(events[5]["type"], "message_stop");

    // §3.1 invariants downstream consumers rely on:
    // - First event's ts_offset_ms is 0 (anchored to first-byte
    //   instant).
    // - ts_offset_ms is monotonically non-decreasing across the
    //   array.
    assert_eq!(events[0]["ts_offset_ms"], 0);
    let offsets: Vec<u64> = events
        .iter()
        .map(|e| e["ts_offset_ms"].as_u64().expect("ts_offset_ms is u64"))
        .collect();
    for w in offsets.windows(2) {
        assert!(
            w[0] <= w[1],
            "ts_offset_ms not monotonic non-decreasing: {} then {}",
            w[0],
            w[1],
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_field_omitted_when_wire_event_has_no_events() {
    // The `events` field is `skip_serializing_if = Option::is_none`
    // so passthrough records (no codec, non-SSE, etc.) stay
    // byte-identical to pre-S10.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    let mut ev = response_with_parsed_events();
    ev.events = None;
    sink.record(ev);

    let sink_owned = Arc::try_unwrap(sink)
        .map_err(|_| "sink still has Arc holders")
        .unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let line = contents.lines().next().expect("at least one line");
    let v: Value = serde_json::from_str(line).expect("parse tap.jsonl line");
    assert!(
        v.get("events").is_none(),
        "passthrough record must not carry `events` field — got: {line}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_round_trip_across_writer_with_timeout() {
    // Defensive: the writer task does its work async; the
    // shutdown_drain story already asserts the flush contract.
    // This test confirms the `events` field specifically survives
    // that path — a regression that drops it on the floor (e.g.
    // a serde tag-collision) would surface as `null` in the JSON.
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 64).await.unwrap());
    sink.record(response_with_parsed_events());

    // Give the writer task a small explicit window to flush;
    // belt-and-suspenders with the `shutdown` call below.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sink_owned = Arc::try_unwrap(sink).map_err(|_| "sink Arc").unwrap();
    sink_owned.shutdown().await;

    let contents = std::fs::read_to_string(&path).expect("read tap.jsonl");
    let v: Value =
        serde_json::from_str(contents.lines().next().expect("line")).expect("parse tap.jsonl line");
    let events = v["events"].as_array().expect("events array");
    assert_eq!(events.len(), 6);
    assert_eq!(events[0]["type"], "message_start");
    assert_eq!(events[5]["type"], "message_stop");
}
