//! axum-based HTTP + WebSocket server.

pub mod api;
pub mod assets;
pub mod decoded_sse;
pub mod rollups;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::hub::HubService;
use crate::ports::DebugProxy;
use rollups::RollupsState;

/// Bind on `addr`, build the router, and return the running server's
/// `JoinHandle`. The caller decides when to await it.
pub async fn serve<P: DebugProxy + Clone + 'static>(
    addr: SocketAddr,
    hub: Arc<HubService>,
    proxy: P,
    rollups: RollupsState,
) -> Result<tokio::task::JoinHandle<()>, std::io::Error> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual = listener.local_addr()?;
    tracing::info!(addr = %actual, "noodle-viewer listening");

    let router = build_router(hub, proxy, rollups);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(?e, "noodle-viewer: serve failed");
        }
    });
    Ok(handle)
}

fn build_router<P: DebugProxy + Clone + 'static>(
    hub: Arc<HubService>,
    proxy: P,
    rollups: RollupsState,
) -> Router {
    let api_state = api::ApiState {
        hub: hub.clone(),
        proxy,
    };

    // V2 OTLP query tab — independent state (a `Connection` behind a
    // `Mutex`), so we mount it as a sub-router with its own
    // `.with_state` and merge it into the main router. Keeps
    // `api::ApiState` shape unchanged for the existing handlers.
    let rollups_router = Router::new()
        .route("/api/rollups/schema", get(rollups::schema))
        .route("/api/rollups/query", post(rollups::query))
        .with_state(rollups);

    Router::new()
        .route("/ws", get(ws::handler))
        // S22 (refactor-overview §10): typed `DecodedExchange` SSE
        // feed. Parallel to `/ws` — the legacy slim path keeps
        // running for OODA / HTTP / SSE views that read the
        // `Exchange` shape; the new frontend panels (turn_id badge,
        // usage panel, content-block tags, pairing arrows, envelope
        // inspector) consume this stream.
        .route("/api/decoded-exchanges", get(decoded_sse::handler))
        .route("/api/tap/status", get(api::status))
        .route("/api/tap/enable", post(api::enable))
        .route("/api/tap/disable", post(api::disable))
        .route("/api/tap/clear", post(api::clear))
        .with_state(api_state)
        .merge(rollups_router)
        .merge(assets::router(hub))
}
