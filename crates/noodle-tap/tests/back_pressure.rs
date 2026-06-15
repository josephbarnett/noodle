//! Verify backpressure semantics: when the writer can't keep up, the
//! sink drops events (counted) instead of blocking.

use std::sync::Arc;
use std::time::Duration;

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
        // Larger body forces the writer task to do more work per item,
        // making channel saturation more reliably reproducible.
        body_in: Bytes::from(vec![b'x'; 8 * 1024]),
        body_out: Bytes::from(vec![b'x'; 8 * 1024]),
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

// `current_thread` flavor is load-bearing, not incidental. The test
// asserts the bounded channel saturates while we synchronously burst
// 10_000 events. That premise only holds if the spawned writer task
// gets *no* execution time during the burst. On a `current_thread`
// runtime the synchronous `for` loop (no `.await`) never yields, so
// the writer stays parked and the channel fills at capacity →
// deterministic drops. Under a multi-thread runtime the writer
// drains concurrently on another worker (cheap `BufWriter` memcpy)
// faster than the producer serializes 8 KiB JSON, so drops race to
// zero and the test flakes. The sibling unit tests in
// `events_sink.rs` / `frames_sink.rs` rely on the same property.
#[tokio::test(flavor = "current_thread")]
async fn over_capacity_records_drop_and_increment_counter() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tap.jsonl");
    // Tiny channel (4) so we force saturation almost immediately.
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 4).await.unwrap());

    // Burst many more events than the channel can hold.
    let n = 10_000;
    for i in 0..n {
        sink.record(ev(i));
    }

    // Some events MUST have been dropped. The writer can't possibly
    // drain 10_000 8KiB lines synchronously while we're still iterating.
    let dropped = sink.dropped_count();
    assert!(
        dropped > 0,
        "expected drops under saturation; got {dropped}"
    );

    // Give the writer a moment, then ensure that what *did* land in
    // the file is well-formed JSONL.
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(sink);
    let contents = std::fs::read_to_string(&path).unwrap();
    for line in contents.lines() {
        let _: serde_json::Value =
            serde_json::from_str(line).expect("each written line is valid JSON");
    }
}
