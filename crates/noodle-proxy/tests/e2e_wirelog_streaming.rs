//! Pins the `WireLogLayer` streaming-tee contract:
//!
//! - response chunks must arrive at the client progressively (the tee
//!   does not buffer the body before forwarding);
//! - the wire log captures one `WireEvent::Response` on end-of-stream
//!   carrying the full transcript.
//!
//! Without this test, a future refactor that turned the tee back into a
//! buffer-then-forward (i.e. `body.collect()`) would silently break SSE
//! streaming for real LLM clients.
//!
//! `WireLogLayer` is exercised in isolation, not through the full MITM
//! stack — the streaming behaviour is a property of the layer itself,
//! and the MITM end-to-end is validated separately by the live smoke
//! against `api.anthropic.com`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use noodle_core::{WireDirection, WireEvent, WireSink};
use noodle_proxy::wirelog::WireLogLayer;
use rama::{
    Layer, Service,
    bytes::Bytes,
    http::{Body, Request, Response, StatusCode, body::util::BodyExt},
    service::service_fn,
};

#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<WireEvent>>,
}
impl WireSink for CapturingSink {
    fn record(&self, event: WireEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[tokio::test]
async fn streaming_response_passes_chunks_progressively_and_logs_full_transcript() {
    // Inner service: returns a Response whose body emits 3 chunks
    // with a 60 ms gap between each. Mimics SSE event timing.
    let inner = service_fn(|_req: Request| async move {
        let stream = async_stream::stream! {
            for chunk in [
                "event: ping\ndata: 1\n\n",
                "event: ping\ndata: 2\n\n",
                "event: ping\ndata: 3\n\n",
            ] {
                tokio::time::sleep(Duration::from_millis(60)).await;
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(chunk.as_bytes()));
            }
        };
        Ok::<_, std::convert::Infallible>(
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap(),
        )
    });

    let sink = Arc::new(CapturingSink::default());
    let svc = WireLogLayer::new(sink.clone() as Arc<dyn WireSink>).layer(inner);

    let req = Request::builder()
        .method("POST")
        .uri("http://upstream.invalid/v1/messages")
        .body(Body::from(""))
        .unwrap();
    let resp = svc.serve(req).await.expect("serve");

    // Stream the response body, recording when each chunk arrives at
    // the consumer. If the layer were buffering, all arrivals would
    // cluster at the same instant (after the full upstream stream
    // completed); progressive delivery means they're spread out.
    let mut body = resp.into_body();
    let start = Instant::now();
    let mut arrival_times = Vec::new();
    let mut received = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("frame ok");
        if let Some(data) = frame.data_ref() {
            arrival_times.push(start.elapsed());
            received.extend_from_slice(data);
        }
    }

    // Three chunks with 60 ms gaps → first arrives ≥ 60 ms in, last
    // arrives ≥ 180 ms in, span between first and last ≥ 100 ms.
    assert_eq!(arrival_times.len(), 3, "expected 3 chunks");
    let span = arrival_times
        .last()
        .unwrap()
        .checked_sub(*arrival_times.first().unwrap())
        .expect("monotonic Instant order");
    assert!(
        span >= Duration::from_millis(100),
        "chunks must arrive progressively (≥100ms span); got {span:?}"
    );

    // Full transcript was reassembled by the consumer.
    assert_eq!(
        std::str::from_utf8(&received).unwrap(),
        "event: ping\ndata: 1\n\nevent: ping\ndata: 2\n\nevent: ping\ndata: 3\n\n"
    );

    // Wire log captured exactly one Request + one Response event,
    // with the full transcript on the Response side.
    let events = sink.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "expected 1 Request + 1 Response event");
    assert!(matches!(events[0].direction, WireDirection::Request));
    let resp_event = &events[1];
    assert!(matches!(resp_event.direction, WireDirection::Response));
    assert_eq!(resp_event.status, Some(200));
    assert_eq!(resp_event.body_in.len(), 63); // 3 × 21 bytes
    assert_eq!(
        std::str::from_utf8(&resp_event.body_in).ok(),
        Some("event: ping\ndata: 1\n\nevent: ping\ndata: 2\n\nevent: ping\ndata: 3\n\n")
    );
}

#[tokio::test]
async fn empty_response_body_still_emits_response_event() {
    let inner = service_fn(|_req: Request| async move {
        Ok::<_, std::convert::Infallible>(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::from(""))
                .unwrap(),
        )
    });

    let sink = Arc::new(CapturingSink::default());
    let svc = WireLogLayer::new(sink.clone() as Arc<dyn WireSink>).layer(inner);

    let req = Request::builder()
        .method("GET")
        .uri("http://upstream.invalid/health")
        .body(Body::from(""))
        .unwrap();
    let resp = svc.serve(req).await.expect("serve");

    // Drain the (empty) body so end-of-stream fires.
    let _ = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();

    let events = sink.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    let resp_event = &events[1];
    assert_eq!(resp_event.status, Some(204));
    assert_eq!(resp_event.body_in.len(), 0);
}
