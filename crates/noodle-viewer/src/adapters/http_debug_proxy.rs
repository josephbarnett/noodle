//! HTTP adapter for the noodle proxy's `/debug/tap/*` REST API.
//!
//! Default base URL: `http://127.0.0.1:9091`. Operators override via
//! `--debug-base` on the viewer's CLI when noodle runs on a different
//! port.

use crate::model::CaptureState;
use crate::ports::{CaptureVerb, DebugProxy, DebugProxyError};

/// Concrete `DebugProxy` that forwards verbs over HTTP.
#[derive(Clone)]
pub struct HttpDebugProxy {
    client: reqwest::Client,
    base: String,
}

impl HttpDebugProxy {
    /// `base` is the scheme+host+port prefix, no trailing slash. Per
    /// noodle's debug API contract: `http://127.0.0.1:9091`.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: base.into(),
        }
    }
}

impl DebugProxy for HttpDebugProxy {
    async fn dispatch(&self, verb: CaptureVerb) -> Result<CaptureState, DebugProxyError> {
        let (method, path) = match verb {
            CaptureVerb::Status => (reqwest::Method::GET, "/debug/tap/status"),
            CaptureVerb::Enable => (reqwest::Method::POST, "/debug/tap/enable"),
            CaptureVerb::Disable => (reqwest::Method::POST, "/debug/tap/disable"),
            CaptureVerb::Clear => (reqwest::Method::POST, "/debug/tap/clear"),
        };
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .request(method, url)
            .send()
            .await
            .map_err(|e| DebugProxyError::Transport(e.to_string()))?;

        let status = resp.status();
        if status == reqwest::StatusCode::NOT_IMPLEMENTED {
            return Err(DebugProxyError::NotImplemented);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DebugProxyError::Upstream(status.as_u16(), body));
        }
        let state: CaptureState = resp
            .json()
            .await
            .map_err(|e| DebugProxyError::Transport(e.to_string()))?;
        Ok(state)
    }
}
