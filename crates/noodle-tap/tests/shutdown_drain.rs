//! Verify graceful shutdown drains buffered events before closing.
//!
//! When the proxy exits cleanly, in-flight `WireEvent`s must end up on
//! disk. Anything dropped at shutdown is a debugger gap.

use std::sync::Arc;

use bytes::Bytes;
use noodle_core::{HeaderPair, WireDirection, WireEvent, WireSink};
use noodle_tap::TapJsonlLog;
use tempfile::tempdir;

fn ev(i: u64) -> WireEvent {
    WireEvent {
        direction: WireDirection::Request,
        request_id: format!("nl-{i}").into(),
        ts_unix_ms: 1_700_000_000_000 + i,
        method: Some("POST".into()),
        url: Some("https://api.anthropic.com/v1/messages".into()),
        status: None,
        headers: vec![HeaderPair {
            name: "Content-Type".into(),
            value: "application/json".into(),
        }],
        body_in: Bytes::from_static(br#"{"hello":"world"}"#),
        body_out: Bytes::from_static(br#"{"hello":"world"}"#),
        marks: None,
        provider: None,
        agent_app: None,
        machine: None,
        collector_app: None,
        subscription: None,
        usage: None,
        content_blocks: None,
        events: None,
        pairing: None,
        attribution: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_flushes_in_flight_events() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 256).await.unwrap());

    // Send a modest number of events so we don't hit backpressure;
    // we want EVERY one of them to land on disk after shutdown.
    let n: u64 = 100;
    for i in 0..n {
        sink.record(ev(i));
    }
    assert_eq!(sink.dropped_count(), 0, "no drops at this volume");

    // Graceful drain.
    Arc::try_unwrap(sink).ok().unwrap().shutdown().await;

    let contents = std::fs::read_to_string(&path).unwrap();
    let line_count = contents.lines().count();
    let expected = usize::try_from(n).expect("n fits in usize");
    assert_eq!(
        line_count, expected,
        "expected all {n} events flushed on shutdown; got {line_count}"
    );
    // Each line is valid JSON, request_id present and unique.
    let mut ids = std::collections::HashSet::new();
    for line in contents.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(ids.insert(v["event_id"].as_str().unwrap().to_owned()));
    }
    assert_eq!(ids.len(), expected);
}
