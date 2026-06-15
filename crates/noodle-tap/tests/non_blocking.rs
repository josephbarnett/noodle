//! Verify the hot path stays non-blocking even when the writer task
//! can't keep up.
//!
//! This is the load-bearing performance contract for `noodle-tap`: if
//! `record()` ever blocks on file I/O, the engine pays the cost on
//! every request.

use std::sync::Arc;
use std::time::{Duration, Instant};

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
        body_in: Bytes::from_static(br#"{"model":"x","messages":[]}"#),
        body_out: Bytes::from_static(br#"{"model":"x","messages":[]}"#),
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
async fn record_returns_quickly_even_under_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tap.jsonl");
    let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 32).await.unwrap());

    // Hammer the sink: 5_000 events as fast as we can. We don't care
    // what the writer task does — we care that the *caller* never
    // pays more than a few microseconds per call on average.
    let n = 5_000;
    let start = Instant::now();
    for i in 0..n {
        sink.record(ev(i));
    }
    let elapsed = start.elapsed();

    // 5_000 calls in well under 1 second is the conservative bound;
    // typical local runs come in under 50 ms. Anything in seconds
    // means we're blocking on I/O.
    assert!(
        elapsed < Duration::from_secs(1),
        "record() under load took {elapsed:?} for {n} events; expected <1s"
    );

    // Cleanup so we don't leak the writer task into the next test.
    Arc::try_unwrap(sink).ok().unwrap().shutdown().await;
}
