//! WebSocket session lifecycle.
//!
//! On connect:
//! 1. Send `Hello` with the viewer's version.
//! 2. Send each message in the hub's recent history.
//! 3. Then forward live broadcasts until the client disconnects.

use std::sync::Arc;

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::IntoResponse,
};

use crate::hub::HubService;
use crate::model::ServerMsg;
use crate::server::api::ApiState;

pub async fn handler<P: crate::ports::DebugProxy + Clone + 'static>(
    ws: WebSocketUpgrade,
    State(state): State<ApiState<P>>,
) -> impl IntoResponse {
    let hub = state.hub;
    ws.on_upgrade(|socket| handle_socket(socket, hub))
}

async fn handle_socket(mut socket: WebSocket, hub: Arc<HubService>) {
    // Hello.
    let hello = ServerMsg::Hello {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if !send_msg(&mut socket, &hello).await {
        return;
    }

    // Replay history. Each entry is an `Arc<ServerMsg>` shared with the
    // hub's history buffer and the broadcast channel; we only borrow it
    // for serialization, so no deep clone of the body happens here.
    let (history, mut rx) = hub.subscribe().await;
    for msg in history {
        if !send_msg(&mut socket, &msg).await {
            return;
        }
    }

    // Live forward until either side hangs up.
    loop {
        tokio::select! {
            biased;
            recv = rx.recv() => {
                let msg = match recv {
                    Ok(m) => m,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "ws client lagged; some events skipped");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if !send_msg(&mut socket, &msg).await { break; }
            }
            inbound = socket.recv() => {
                match inbound {
                    // Client hung up or sent an explicit close.
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    // Ignore other client→server payloads for now.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

async fn send_msg(socket: &mut WebSocket, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(s) => socket.send(Message::Text(s.into())).await.is_ok(),
        Err(e) => {
            tracing::warn!(?e, "ws: serialize failed");
            true
        }
    }
}
