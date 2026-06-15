//! `/api/tap/*` REST handlers — forward to noodle's `:9091` debug
//! API and broadcast capture-state changes back to clients.
//!
//! The viewer never holds tap state of its own; it's a thin
//! transport. The truth is on the noodle proxy side.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::hub::HubService;
use crate::model::CaptureState;
use crate::ports::{CaptureVerb, DebugProxy, DebugProxyError};

#[derive(Clone)]
pub struct ApiState<P: DebugProxy + Clone> {
    pub hub: Arc<HubService>,
    pub proxy: P,
}

pub async fn status<P: DebugProxy + Clone>(
    State(state): State<ApiState<P>>,
) -> Result<Json<CaptureState>, ApiError> {
    let s = state.proxy.dispatch(CaptureVerb::Status).await?;
    state.hub.set_capture(s.clone()).await;
    Ok(Json(s))
}

pub async fn enable<P: DebugProxy + Clone>(
    State(state): State<ApiState<P>>,
) -> Result<Json<CaptureState>, ApiError> {
    let s = state.proxy.dispatch(CaptureVerb::Enable).await?;
    state.hub.set_capture(s.clone()).await;
    Ok(Json(s))
}

pub async fn disable<P: DebugProxy + Clone>(
    State(state): State<ApiState<P>>,
) -> Result<Json<CaptureState>, ApiError> {
    let s = state.proxy.dispatch(CaptureVerb::Disable).await?;
    state.hub.set_capture(s.clone()).await;
    Ok(Json(s))
}

pub async fn clear<P: DebugProxy + Clone>(
    State(state): State<ApiState<P>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Two-tier clear: ask the proxy to clear the file (if it
    // supports it), and always drop our local replay buffer so the
    // browser stops showing past events.
    let proxy_outcome = match state.proxy.dispatch(CaptureVerb::Clear).await {
        Ok(_) => json!({"file_cleared": true}),
        Err(DebugProxyError::NotImplemented) => {
            // Documented behavior: noodle's `/debug/tap/clear` returns
            // 501 in this iteration. Local clear still helps.
            json!({"file_cleared": false, "reason": "not implemented; restart noodle for a fresh log"})
        }
        Err(e) => return Err(ApiError::from(e)),
    };
    state.hub.clear_local_history().await;
    Ok(Json(json!({
        "ok": true,
        "local_cleared": true,
        "upstream": proxy_outcome,
    })))
}

pub struct ApiError(DebugProxyError);

impl From<DebugProxyError> for ApiError {
    fn from(e: DebugProxyError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match self.0 {
            DebugProxyError::Transport(msg) => (
                StatusCode::BAD_GATEWAY,
                json!({"error": "noodle debug API unreachable", "detail": msg}),
            ),
            DebugProxyError::Upstream(code, body) => (
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                json!({"error": "noodle debug API returned non-2xx", "body": body}),
            ),
            DebugProxyError::NotImplemented => (
                StatusCode::NOT_IMPLEMENTED,
                json!({"error": "verb not implemented by noodle"}),
            ),
        };
        (status, Json(body)).into_response()
    }
}
