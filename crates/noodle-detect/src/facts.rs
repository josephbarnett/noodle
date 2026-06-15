//! The [`AttributionFacts`] bundle returned by [`crate::detect`].

use noodle_core::layered::{
    Artifact, AuditEvent, Correlation, Hint, ResolvedRecord, RoundTripRecord,
};

/// Self-contained per-flow attribution result.
///
/// Matches ADR 039 §2.3 verbatim — the host gateway consumes this
/// bundle and decides what to do with it (forward to its own
/// telemetry collector, persist to its own audit log, surface to
/// the user, etc.). The facade itself never writes.
#[derive(Debug, Clone)]
pub struct AttributionFacts {
    /// The four correlation IDs ADR 023 pins on every data-plane
    /// record. Any `None` slot is the contract for "not yet known"
    /// — the host can mint on its side or forward as-is.
    pub correlation: Correlation,
    /// Detector hints (provider, model, tool intent, etc.).
    pub hints: Vec<Hint>,
    /// Extracted artifacts (decoded usage, decoded tool calls,
    /// etc.).
    pub artifacts: Vec<Artifact>,
    /// Audit events emitted by transforms (filtered, enhanced,
    /// errored, etc.).
    pub audits: Vec<AuditEvent>,
    /// Final attribution decision the Resolver produced, if a
    /// response was supplied. `None` for request-only invocations.
    pub resolved: Option<ResolvedRecord>,
    /// Per-flow round-trip record (ADR 023 §4) assembled when
    /// the response is supplied. `None` for request-only.
    pub round_trip: Option<RoundTripRecord>,
    /// Clock reading at the moment the facade returned. Stamped
    /// here so the host doesn't need to re-time.
    pub at_unix_ms: u64,
}
