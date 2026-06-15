//! Per-flow lifecycle tracing for the proxy data path.
//!
//! [`FlowTrace`] wraps a per-connection `Service` so every flow gets a
//! unique `flow.id` and a `tracing` span. The span propagates into the
//! inner service's awaits — including rama's relay/bridge logs
//! (`BridgeCloseReason`, the MITM handshake, per-copy progress) — so
//! every log line for one connection shares the same `flow.id`.
//!
//! Why this exists: a wedged client connection emits `flow.start` (and
//! whatever inner checkpoints fire) but **no `flow.end`**. Grep the hung
//! `flow.id` and the last line under it pinpoints exactly where the flow
//! stalled — the SNI peek, the upstream TLS handshake, or the byte copy.
//!
//! Enable with `RUST_LOG`:
//! - `RUST_LOG=noodle_proxy=debug,rama=debug` — flow start/end plus
//!   rama's close reasons.
//! - add `rama=trace` for per-copy byte progress.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rama::Service;
use tracing::Instrument;

/// Process-wide monotonic flow counter. Relaxed is sufficient: we only
/// need uniqueness within a run, not cross-thread ordering.
static NEXT_FLOW_ID: AtomicU64 = AtomicU64::new(1);

/// Service decorator that opens a `flow` span around each `serve` call
/// and logs `flow.start` / `flow.end {elapsed_ms, outcome}`.
#[derive(Clone)]
pub struct FlowTrace<S> {
    inner: S,
    kind: &'static str,
}

impl<S> FlowTrace<S> {
    /// `kind` labels the path (`"forward"` / `"transparent"`) so a mixed
    /// log can be filtered by which proxy stack handled the flow.
    pub fn new(kind: &'static str, inner: S) -> Self {
        Self { inner, kind }
    }
}

impl<S, I> Service<I> for FlowTrace<S>
where
    S: Service<I>,
    I: Send + 'static,
{
    type Output = S::Output;
    type Error = S::Error;

    async fn serve(&self, input: I) -> Result<Self::Output, Self::Error> {
        let id = NEXT_FLOW_ID.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!("flow", id, kind = self.kind);
        async move {
            let started = Instant::now();
            tracing::debug!("flow.start");
            let result = self.inner.serve(input).await;
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::debug!(
                elapsed_ms,
                outcome = if result.is_ok() { "ok" } else { "err" },
                "flow.end"
            );
            result
        }
        .instrument(span)
        .await
    }
}
