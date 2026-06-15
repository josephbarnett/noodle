//! `DebugProxy` — outbound port to noodle's `:9091` capture API.
//!
//! Adapters: `adapters::http_debug_proxy::HttpDebugProxy` for production,
//! a stub in tests.

use thiserror::Error;

use crate::model::CaptureState;

#[derive(Debug, Error)]
pub enum DebugProxyError {
    #[error("debug proxy: transport error: {0}")]
    Transport(String),
    #[error("debug proxy: upstream returned {0}: {1}")]
    Upstream(u16, String),
    #[error("debug proxy: not implemented for this verb")]
    NotImplemented,
}

/// Verbs the viewer can ask the noodle proxy to perform.
#[derive(Debug, Clone, Copy)]
pub enum CaptureVerb {
    Status,
    Enable,
    Disable,
    Clear,
}

pub trait DebugProxy: Send + Sync + 'static {
    /// Execute `verb` and return the resulting capture state.
    /// Errors are bubbled up to the HTTP handler verbatim.
    ///
    /// The `Send` bound on the returned future is required so axum can
    /// drive these handlers from its multi-threaded executor.
    fn dispatch(
        &self,
        verb: CaptureVerb,
    ) -> impl std::future::Future<Output = Result<CaptureState, DebugProxyError>> + Send;
}
