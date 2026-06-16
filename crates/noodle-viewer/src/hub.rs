//! `HubService` — owns the in-memory event broadcast.
//!
//! Reads events from one or more `EventSource`s, forwards them to all
//! WebSocket subscribers via a `tokio::sync::broadcast` channel, and
//! exposes `subscribe()` for new clients.
//!
//! Single responsibility: routing. No parsing logic (lives in the
//! source adapter); no client lifecycle (lives in `server::ws`).

use std::sync::Arc;

use tokio::sync::{Mutex, broadcast, mpsc};

use crate::model::{CaptureState, DecodedExchange, Exchange, ServerMsg};
use crate::ports::{DecodedExchangeSource, EventSource, FrameSource, SideEffectSource};

/// Default per-client channel depth. Slow clients lose old events
/// past this point — broadcast tolerates `Lagged`.
pub const SUBSCRIBER_CAPACITY: usize = 1024;

/// Recent-history buffer size. New connections replay this many events
/// so they see context, not just the next thing to happen.
pub const HISTORY_LIMIT: usize = 5_000;

/// The hub. Construct via [`Self::new`], then attach event sources via
/// [`Self::attach_source`], then call [`Self::subscribe`] for each
/// client.
///
/// History and broadcast both hold `Arc<ServerMsg>`. Publish wraps the
/// message exactly once; every downstream clone (history retention,
/// per-subscriber broadcast slot, new-client snapshot replay) is an
/// `Arc` bump rather than a deep clone of the full `Exchange` body.
/// This matters because a single `Exchange.body` is a `serde_json::Value`
/// that can reach megabytes for long SSE responses, and the replay
/// path can touch up to `HISTORY_LIMIT` (`5_000`) entries on every new
/// client connection.
pub struct HubService {
    tx: broadcast::Sender<Arc<ServerMsg>>,
    /// Parallel broadcast for the typed [`DecodedExchange`] feed
    /// (S21 of the 027–031 refactor — refactor-overview.md §10).
    ///
    /// The legacy `ServerMsg::Exchange` fanout (above) keeps
    /// running unchanged — that's what the existing React frontend
    /// reads (its `ooda.ts` heuristic depends on the slim shape).
    /// The decoded broadcast is separate so new consumers (the S22
    /// frontend, exec-claude e2e) subscribe to typed events
    /// without disturbing the legacy path. S22 retires the legacy
    /// channel once the frontend can render the typed fields.
    decoded_tx: broadcast::Sender<Arc<DecodedExchange>>,
    history: Mutex<History>,
    /// Parallel history for the decoded channel. Same retention as
    /// `history`. Page-refresh clients drain this snapshot before
    /// attaching to the live broadcast so attribution chips, turn
    /// ids, and usage are visible without re-driving traffic.
    decoded_history: Mutex<DecodedHistory>,
    capture: Mutex<CaptureState>,
}

struct History {
    buf: std::collections::VecDeque<Arc<ServerMsg>>,
}

impl History {
    fn new(limit: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(limit),
        }
    }
    fn push(&mut self, msg: Arc<ServerMsg>) {
        if self.buf.len() == HISTORY_LIMIT {
            self.buf.pop_front();
        }
        self.buf.push_back(msg);
    }
    fn snapshot(&self) -> Vec<Arc<ServerMsg>> {
        self.buf.iter().cloned().collect()
    }
    fn clear(&mut self) {
        self.buf.clear();
    }
}

struct DecodedHistory {
    buf: std::collections::VecDeque<Arc<DecodedExchange>>,
}

impl DecodedHistory {
    fn new(limit: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(limit),
        }
    }
    fn push(&mut self, dx: Arc<DecodedExchange>) {
        if self.buf.len() == HISTORY_LIMIT {
            self.buf.pop_front();
        }
        self.buf.push_back(dx);
    }
    fn snapshot(&self) -> Vec<Arc<DecodedExchange>> {
        self.buf.iter().cloned().collect()
    }
    fn clear(&mut self) {
        self.buf.clear();
    }
}

impl HubService {
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(SUBSCRIBER_CAPACITY);
        let (decoded_tx, _drx) = broadcast::channel(SUBSCRIBER_CAPACITY);
        Arc::new(Self {
            tx,
            decoded_tx,
            history: Mutex::new(History::new(HISTORY_LIMIT)),
            decoded_history: Mutex::new(DecodedHistory::new(HISTORY_LIMIT)),
            capture: Mutex::new(CaptureState {
                enabled: false,
                file: None,
            }),
        })
    }

    /// Wire an event source into the hub. Spawns one tokio task that
    /// drains the source's receiver and broadcasts to subscribers.
    pub fn attach_source<S: EventSource>(self: &Arc<Self>, source: &S) {
        let mut rx = source.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            // ADR 047 rung 1 brain observer — pairs round-trips by
            // `event_id` and emits `ServerMsg::Brain` once both halves
            // have arrived. State is per-source (one observer per
            // spawned task) — fine while the viewer attaches one
            // event source per hub; if multiple sources land,
            // promote this to a hub-level field behind a `Mutex`.
            let mut brain = crate::brain_observer::BrainObserver::new();
            while let Some(ex) = rx.recv().await {
                let brain_event = brain.observe(&ex);
                me.publish(ServerMsg::Exchange(ex)).await;
                if let Some((event_id, observation)) = brain_event {
                    me.publish(ServerMsg::Brain {
                        event_id,
                        observation,
                    })
                    .await;
                }
            }
        });
    }

    /// Wire a per-frame SSE source into the hub. Symmetric to
    /// [`Self::attach_source`] but for `ServerMsg::Frame`.
    pub fn attach_frame_source<S: FrameSource>(self: &Arc<Self>, source: &S) {
        let mut rx = source.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                me.publish(ServerMsg::Frame(f)).await;
            }
        });
    }

    /// Wire a typed [`DecodedExchange`] source into the hub (S21
    /// of the 027–031 refactor — refactor-overview.md §10).
    ///
    /// Symmetric to [`Self::attach_source`] but for the new typed
    /// feed: the source's mpsc receiver carries
    /// [`DecodedExchange`]s (the registry-decoded view) rather
    /// than slim [`Exchange`]s. New events fan out via
    /// [`Self::subscribe_decoded`].
    ///
    /// The legacy `ServerMsg::Exchange` broadcast (above) keeps
    /// running in parallel. The two paths are deliberately
    /// independent: the React frontend still depends on the slim
    /// shape, but the new exec-claude e2e and the upcoming S22
    /// frontend refresh consume the typed feed.
    pub fn attach_decoded_source<S: DecodedExchangeSource>(self: &Arc<Self>, source: &S) {
        let mut rx = source.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            // ADR 056 step 5 — pair the request body (system/tools
            // sizes) with the response's decoded usage by `event_id`,
            // emitting `ServerMsg::ContextWeight` once the response
            // arrives. Requests buffer their body; a response computes
            // from the buffered body + its decoded `usage.tokens`.
            // Response-before-request yields usage-only (no structural
            // sizes) — an accepted v1 degradation.
            let mut cw_req_bodies: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            while let Some(dx) = rx.recv().await {
                let cw: Option<(String, noodle_embellish_core::ContextWeight)> = {
                    let event_id = dx.exchange.event_id.clone();
                    if event_id.is_empty() {
                        None
                    } else {
                        match dx.exchange.direction {
                            crate::model::Direction::Request => {
                                if !dx.exchange.body.is_null() {
                                    cw_req_bodies.insert(event_id, dx.exchange.body.clone());
                                }
                                None
                            }
                            crate::model::Direction::Response => dx
                                .usage
                                .as_ref()
                                .and_then(|u| u.tokens.as_ref())
                                .map(|usage| {
                                    let req_body = cw_req_bodies.remove(&event_id);
                                    (
                                        event_id,
                                        noodle_embellish_core::measure_context_weight_from_parts(
                                            req_body.as_ref(),
                                            usage,
                                        ),
                                    )
                                }),
                        }
                    }
                };
                me.publish_decoded(dx).await;
                if let Some((event_id, weight)) = cw {
                    me.publish(ServerMsg::ContextWeight { event_id, weight }).await;
                }
            }
        });
    }

    /// Wire an attribution side-effect source into the hub
    /// (item 4 viewer-panel slice, ADR 020 §7). Symmetric to
    /// [`Self::attach_source`] but for `ServerMsg::SideEffect`.
    /// Carries the engine's emitted `Hint`/`Artifact`/`Audit`/
    /// `Resolved` records out to the frontend.
    pub fn attach_side_effect_source<S: SideEffectSource>(self: &Arc<Self>, source: &S) {
        let mut rx = source.subscribe();
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                me.publish(ServerMsg::SideEffect { event: ev }).await;
            }
        });
    }

    /// Push a message to history and broadcast. Errors (no subscribers)
    /// are swallowed — the history still grew.
    ///
    /// The message is wrapped in `Arc` once here; the history retention
    /// clone and every per-subscriber clone inside the broadcast channel
    /// become pointer bumps.
    pub async fn publish(&self, msg: ServerMsg) {
        let msg = Arc::new(msg);
        self.history.lock().await.push(msg.clone());
        let _ = self.tx.send(msg);
    }

    /// Broadcast a typed [`DecodedExchange`] to every subscriber of
    /// [`Self::subscribe_decoded`].
    ///
    /// Retains the message in `decoded_history` so a page-refresh
    /// client can replay typed fields (marks, usage, content blocks,
    /// pairing, attribution) via the snapshot returned by
    /// [`Self::subscribe_decoded`]. Wrapped in `Arc` once so the
    /// history clone, every per-subscriber broadcast slot, and the
    /// new-client snapshot replay are pointer bumps.
    pub async fn publish_decoded(&self, dx: DecodedExchange) {
        let dx = Arc::new(dx);
        self.decoded_history.lock().await.push(dx.clone());
        let _ = self.decoded_tx.send(dx);
    }

    /// New WebSocket client connection: returns the snapshot of recent
    /// history (so they catch up) and a fresh `Receiver` for live
    /// events.
    pub async fn subscribe(&self) -> (Vec<Arc<ServerMsg>>, broadcast::Receiver<Arc<ServerMsg>>) {
        let history = self.history.lock().await.snapshot();
        let rx = self.tx.subscribe();
        (history, rx)
    }

    /// New decoded-channel subscription. Returns the snapshot of
    /// recent decoded history (so a page-refresh client catches up
    /// on marks, usage, content blocks, pairing, attribution) and a
    /// fresh `Receiver` for live [`DecodedExchange`] events.
    pub async fn subscribe_decoded(
        &self,
    ) -> (
        Vec<Arc<DecodedExchange>>,
        broadcast::Receiver<Arc<DecodedExchange>>,
    ) {
        let history = self.decoded_history.lock().await.snapshot();
        let rx = self.decoded_tx.subscribe();
        (history, rx)
    }

    /// Update the cached capture status and broadcast a `Capture`
    /// event to all clients. Called by the server's REST handler
    /// after a successful debug-API round trip.
    pub async fn set_capture(&self, state: CaptureState) {
        *self.capture.lock().await = state.clone();
        self.publish(ServerMsg::Capture(state)).await;
    }

    /// Local clear: empties the history buffer and broadcasts. Does
    /// not touch the on-disk JSONL file. Operators clear that
    /// separately via the noodle proxy's debug API (or restart noodle).
    pub async fn clear_local_history(&self) {
        self.history.lock().await.clear();
        self.decoded_history.lock().await.clear();
    }
}

/// Convenience: pump events from an mpsc channel directly into the
/// hub (used by tests and for sources that don't implement
/// `EventSource` — e.g. a one-shot replay).
pub async fn pump_into_hub(hub: Arc<HubService>, mut rx: mpsc::Receiver<Exchange>) {
    while let Some(ex) = rx.recv().await {
        hub.publish(ServerMsg::Exchange(ex)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Direction;
    use std::time::Duration;

    fn ex(id: &str) -> Exchange {
        Exchange {
            direction: Direction::Request,
            timestamp: "2026-05-10T18:00:00Z".into(),
            event_id: id.into(),
            provider: "anthropic".into(),
            method: None,
            url: None,
            status: None,
            session_hash: None,
            headers: serde_json::Map::new(),
            body: serde_json::Value::Null,
            body_out: None,
        }
    }

    #[tokio::test]
    async fn publish_appears_on_subscriber() {
        let hub = HubService::new();
        let (history, mut rx) = hub.subscribe().await;
        assert!(history.is_empty());

        hub.publish(ServerMsg::Exchange(ex("nl-1"))).await;

        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match &*msg {
            ServerMsg::Exchange(e) => assert_eq!(e.event_id, "nl-1"),
            _ => panic!("expected Exchange"),
        }
    }

    #[tokio::test]
    async fn new_subscribers_get_recent_history() {
        let hub = HubService::new();
        for i in 0..5 {
            hub.publish(ServerMsg::Exchange(ex(&format!("nl-{i}"))))
                .await;
        }
        let (history, _rx) = hub.subscribe().await;
        assert_eq!(history.len(), 5);
    }

    #[tokio::test]
    async fn capture_state_change_is_broadcast() {
        let hub = HubService::new();
        let (_history, mut rx) = hub.subscribe().await;
        hub.set_capture(CaptureState {
            enabled: true,
            file: Some("/tmp/x".into()),
        })
        .await;
        let msg = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match &*msg {
            ServerMsg::Capture(s) => {
                assert!(s.enabled);
                assert_eq!(s.file.as_deref(), Some("/tmp/x"));
            }
            _ => panic!("expected Capture"),
        }
    }

    /// Lock in that history and the snapshot share one `Arc` per
    /// publish — i.e., we never deep-clone the `Exchange` body on the
    /// fanout path. If a future refactor accidentally re-wraps or
    /// clones the inner `ServerMsg`, this test fails loudly.
    ///
    /// We publish BEFORE any subscriber exists so `broadcast::send`
    /// returns `Err(SendError)` and drops the channel's reference,
    /// leaving only the history's `Arc` alive until the snapshot.
    #[tokio::test]
    async fn publish_shares_one_arc_across_history_and_snapshot() {
        let hub = HubService::new();
        hub.publish(ServerMsg::Exchange(ex("nl-share"))).await;

        let (snapshot, _rx) = hub.subscribe().await;
        assert_eq!(snapshot.len(), 1);
        // history buffer (1) + snapshot vec (1) = 2.
        assert_eq!(Arc::strong_count(&snapshot[0]), 2);
    }

    #[tokio::test]
    async fn attach_frame_source_publishes_frames_to_subscribers() {
        use crate::model::Frame;
        use crate::ports::FrameSource;

        // Tiny in-memory FrameSource: pumps a pre-built sequence
        // onto its channel and then closes it.
        struct VecSource {
            rx: std::sync::Mutex<Option<mpsc::Receiver<Frame>>>,
        }
        impl FrameSource for VecSource {
            fn subscribe(&self) -> mpsc::Receiver<Frame> {
                self.rx.lock().unwrap().take().expect("subscribe once")
            }
        }

        let (tx, rx) = mpsc::channel::<Frame>(4);
        let source = VecSource {
            rx: std::sync::Mutex::new(Some(rx)),
        };

        let hub = HubService::new();
        let (_history, mut wsrx) = hub.subscribe().await;
        hub.attach_frame_source(&source);

        tx.send(Frame {
            request_id: "nl-7".into(),
            frame_index: 0,
            timestamp: "2026-05-11T12:00:00Z".into(),
            ts_unix_ms: 0,
            event: Some("message_start".into()),
            data: serde_json::json!({"type":"message_start"}),
        })
        .await
        .unwrap();

        let msg = tokio::time::timeout(Duration::from_millis(200), wsrx.recv())
            .await
            .unwrap()
            .unwrap();
        match &*msg {
            ServerMsg::Frame(f) => {
                assert_eq!(f.request_id, "nl-7");
                assert_eq!(f.frame_index, 0);
                assert_eq!(f.event.as_deref(), Some("message_start"));
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_side_effect_source_publishes_to_subscribers() {
        use crate::model::SideEffectEvent;
        use crate::ports::SideEffectSource;

        struct VecSource {
            rx: std::sync::Mutex<Option<mpsc::Receiver<SideEffectEvent>>>,
        }
        impl SideEffectSource for VecSource {
            fn subscribe(&self) -> mpsc::Receiver<SideEffectEvent> {
                self.rx.lock().unwrap().take().expect("subscribe once")
            }
        }

        let (tx, rx) = mpsc::channel::<SideEffectEvent>(4);
        let source = VecSource {
            rx: std::sync::Mutex::new(Some(rx)),
        };

        let hub = HubService::new();
        let (_history, mut wsrx) = hub.subscribe().await;
        hub.attach_side_effect_source(&source);

        let mut resolved_map = std::collections::BTreeMap::new();
        resolved_map.insert("tool".into(), "Claude Code".into());
        tx.send(SideEffectEvent::Resolved {
            session_prefix: "abc12345".into(),
            flow_id: 0,
            at_unix_ms: 0,
            resolved: resolved_map,
            event_id: None,
            turn_id: None,
            frame_id: None,
        })
        .await
        .unwrap();

        let msg = tokio::time::timeout(Duration::from_millis(200), wsrx.recv())
            .await
            .unwrap()
            .unwrap();
        match &*msg {
            ServerMsg::SideEffect {
                event:
                    SideEffectEvent::Resolved {
                        session_prefix,
                        resolved,
                        ..
                    },
            } => {
                assert_eq!(session_prefix, "abc12345");
                assert_eq!(resolved.get("tool").unwrap(), "Claude Code");
            }
            other => panic!("expected SideEffect::Resolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn attach_decoded_source_publishes_decoded_exchanges_to_subscribers() {
        use crate::decoders::ProviderDecoderRegistry;
        use crate::model::DecodedExchange;
        use crate::ports::DecodedExchangeSource;

        // Tiny in-memory DecodedExchangeSource: hand a single
        // pre-built `DecodedExchange` to the hub and assert it
        // surfaces on the typed broadcast subscription.
        struct VecSource {
            rx: std::sync::Mutex<Option<mpsc::Receiver<DecodedExchange>>>,
        }
        impl DecodedExchangeSource for VecSource {
            fn subscribe(&self) -> mpsc::Receiver<DecodedExchange> {
                self.rx.lock().unwrap().take().expect("subscribe once")
            }
        }

        // Build a real DecodedExchange via the registry — same
        // path the production adapter uses.
        let registry = ProviderDecoderRegistry::with_defaults();
        let raw = serde_json::json!({
            "direction": "request",
            "timestamp": "2026-05-21T00:00:00Z",
            "event_id": "nl-d1",
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
            "marks": {"session_id": "sess_x", "turn_id": "turn_x"},
        });
        let dx = registry.decode(&raw).expect("decode");

        let (tx, rx) = mpsc::channel::<DecodedExchange>(4);
        let source = VecSource {
            rx: std::sync::Mutex::new(Some(rx)),
        };

        let hub = HubService::new();
        let (_history, mut decoded_rx) = hub.subscribe_decoded().await;
        hub.attach_decoded_source(&source);

        tx.send(dx).await.unwrap();

        let received = tokio::time::timeout(Duration::from_millis(200), decoded_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.exchange.event_id, "nl-d1");
        let marks = received.marks.as_ref().expect("marks populated");
        assert_eq!(marks.turn_id.as_ref().unwrap().as_str(), "turn_x");
    }

    #[tokio::test]
    async fn clear_local_history_drops_buffer() {
        let hub = HubService::new();
        for i in 0..3 {
            hub.publish(ServerMsg::Exchange(ex(&format!("nl-{i}"))))
                .await;
        }
        hub.clear_local_history().await;
        let (history, _rx) = hub.subscribe().await;
        assert!(history.is_empty());
    }
}
