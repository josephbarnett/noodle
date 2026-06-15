//! `GET /api/decoded-exchanges` — Server-Sent Events stream of
//! typed [`DecodedExchange`] records (S22 of the 027–031 refactor —
//! refactor-overview.md §10).
//!
//! Why SSE rather than reuse the existing `/ws` channel: the
//! WebSocket carries the legacy `ServerMsg` enum the existing React
//! frontend depends on. Rather than mix discriminator shapes on one
//! socket (which would invite parsing drift on the slim path), the
//! decoded layer rides its own one-way HTTP/1.1 stream. The
//! frontend opens both — the existing WS for the slim view, this SSE
//! for the typed view — and renders an additive UI layer on top of
//! the same rows.
//!
//! Wire shape: each `event: decoded_exchange\ndata: {…json…}\n\n`
//! frame is the JSON-serialized [`DecodedExchange`]. The JSON shape
//! mirrors the on-disk `tap.jsonl` (`snake_case` keys, on-disk token
//! field names) per [`crate::model`]'s wire-shape documentation.
//!
//! Lag handling: like the WS path, slow clients can lag the
//! broadcast channel. We log and continue on `Lagged`; the client
//! sees a gap but doesn't disconnect.

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::State,
    response::{
        Sse,
        sse::{Event, KeepAlive},
    },
};
use futures_util::stream::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::model::DecodedExchange;
use crate::server::api::ApiState;

/// SSE handler bound to `GET /api/decoded-exchanges`.
///
/// Subscribes the connecting client to the hub's
/// [`HubService::subscribe_decoded`] broadcast and streams one
/// `event: decoded_exchange` SSE frame per [`DecodedExchange`].
/// Keep-alive comments are emitted every 15s so intermediate
/// proxies don't reap the idle connection.
pub async fn handler<P: crate::ports::DebugProxy + Clone + 'static>(
    State(state): State<ApiState<P>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (history, rx) = state.hub.subscribe_decoded().await;
    let stream = decoded_stream(history, rx);
    Sse::new(stream).keep_alive(KeepAlive::new())
}

/// Map a broadcast receiver to an SSE event stream.
///
/// Per-event behaviour:
///
/// - Successful recv → one `event: decoded_exchange` frame with the
///   JSON-serialized `DecodedExchange` as `data:`.
/// - `Lagged(n)` → emit a `event: lag\ndata: n` frame so the client
///   can surface the gap, then continue.
/// - Channel closed / serializer error → end the stream.
///
/// Wrapped in `BroadcastStream` so the receiver lifetime is tied to
/// the SSE response — when the client disconnects, the stream is
/// dropped and the broadcast subscription with it.
fn decoded_stream(
    history: Vec<Arc<DecodedExchange>>,
    rx: broadcast::Receiver<Arc<DecodedExchange>>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    use futures_util::StreamExt;
    let replay =
        futures_util::stream::iter(history.into_iter().filter_map(
            |dx| match serde_json::to_string(&*dx) {
                Ok(json) => Some(Ok(Event::default().event("decoded_exchange").data(json))),
                Err(e) => {
                    tracing::warn!(?e, "decoded SSE: history serialize failed");
                    None
                }
            },
        ));
    let live = BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(dx) => match serde_json::to_string(&*dx) {
                Ok(json) => Some(Ok(Event::default().event("decoded_exchange").data(json))),
                Err(e) => {
                    tracing::warn!(?e, "decoded SSE: serialize failed");
                    None
                }
            },
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                tracing::warn!(n, "decoded SSE: client lagged; emitting lag frame");
                Some(Ok(Event::default().event("lag").data(n.to_string())))
            }
        }
    });
    replay.chain(live)
}
