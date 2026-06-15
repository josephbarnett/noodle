//! Host-supplied context — clock + marking store + prior IDs.

use std::sync::Arc;

use noodle_core::MarkingStore;
use smol_str::SmolStr;

/// Per-flow context the host supplies to [`crate::detect`].
///
/// Carries:
/// - the [`Clock`] used to stamp `at_unix_ms` on the returned facts
///   (enhanced so replay tests can pin a fake clock);
/// - the [`MarkingStore`] used to mint / look up session, turn, and
///   agent-run IDs;
/// - any prior `session_id` the host knows from its own
///   per-user/per-flow correlation.
pub struct DetectContext {
    pub clock: Arc<dyn Clock>,
    pub marking_store: Arc<dyn MarkingStore>,
    /// Prior `session_id` the host already correlates this flow
    /// with, if any. The facade prefers this over minting a new
    /// session id when present.
    pub session_id: Option<SmolStr>,
}

/// Enhanced time source. Wraps a `now_unix_ms()` reading so the
/// facade stays pure modulo the clock (ADR 039 §2.3 invariant).
///
/// Implementations:
/// - production: `SystemClock` reads `SystemTime::now()`.
/// - test: `FakeClock` returns a fixed value for snapshot tests.
pub trait Clock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> u64;
}

/// Production clock — reads `std::time::SystemTime::now()`.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}
