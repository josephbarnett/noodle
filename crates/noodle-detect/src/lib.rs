//! `noodle-detect` — plugin-host facade for noodle's attribution
//! pipeline (ADR 039 §2.3).
//!
//! Where the proxy host (`noodle-proxy`) drives the pipeline through
//! the rama `InspectionEngine` over the wire-byte stream, an existing
//! LLM gateway that already has the bytes calls [`detect`]
//! **synchronously** with the request + (optional) response and gets
//! back an [`AttributionFacts`] bundle.
//!
//! ## Invariants (ADR 039 §2.3)
//!
//! - **Synchronous.** No `async` in the signature.
//! - **No I/O.** No file paths, no sinks, no network calls.
//! - **No runtime.** No tokio dependency in this crate.
//! - **Pure function modulo `Clock`.** Same inputs + same clock →
//!   same outputs.
//!
//! ## Surface
//!
//! - [`DetectRequest`] / [`DetectResponse`] / [`DetectContext`] —
//!   inputs.
//! - [`AttributionFacts`] — output.
//! - [`Clock`] — enhanced time source for deterministic replay.
//! - Re-exported pure types from `noodle-core`, `noodle-domain`,
//!   `noodle-embellish-core`, and `noodle-adapters`' pure-logic
//!   submodules so a plugin author can build adapter shims against
//!   one crate.

#![forbid(unsafe_code)]

mod context;
mod facts;
mod request;
mod response;

pub use context::{Clock, DetectContext, SystemClock};
pub use facts::AttributionFacts;
pub use request::DetectRequest;
pub use response::DetectResponse;

// ─── re-exports (pure surface) ────────────────────────────────

pub use noodle_core::layered::{
    Artifact, AuditEvent, AuditKind, Correlation, Hint, ResolvedRecord, RoundTripRecord,
    RoundTripRequest, RoundTripResponse, SideEffect, ToolInvocation, ToolResolution,
};
pub use noodle_core::{MarkingSessionId, MarkingStore};

pub use noodle_domain::{
    capability, citation_ref, classifier, content_category, decoders, envelope_metadata,
    observation_context, principal_identity, reminder_subtype, speech_act, subscription_context,
    task_plan, trust_level, turn_end, usage, vendor,
};

pub use noodle_embellish_core::{TelemetryRow, map_decoded_pair, map_pair};

// Pure-logic submodules of noodle-adapters. The remaining
// submodules (cert, sse, codec, tls, store, filter, enhancer, log,
// dns) are proxy-host concerns and intentionally NOT re-exported
// here — the facade's public surface is plugin-only.
pub use noodle_adapters::marking;
pub use noodle_adapters::request_detector;
pub use noodle_adapters::transform::{marker_strip, placement};

// ─── the detect API ───────────────────────────────────────────

/// Run the attribution pipeline over a complete request/response pair.
///
/// **Current implementation: contract-only stub.** This entry point
/// pins the public surface specified by ADR 039 §2.3 — the type
/// shapes (`DetectRequest`, `DetectResponse`, `DetectContext`,
/// `AttributionFacts`) and the synchronous, no-I/O, no-runtime
/// invariants — but does **not** yet dispatch detectors,
/// transforms, or the Resolver. The returned `AttributionFacts`
/// carries the host-supplied `session_id` (if any), the current
/// clock reading, and otherwise-empty hint / artifact / audit /
/// resolved / `round_trip` slots.
///
/// The intent is that plugin authors and host integrations can
/// target the stable public surface today; the body is populated
/// in a follow-up slice (tracked by feature story
/// [`docs/features/048-wasm-plugin-author-experience.md`](https://github.com/josephbarnett/noodle/blob/main/docs/features/048-wasm-plugin-author-experience.md))
/// without changing this function's signature or return type.
#[must_use]
pub fn detect(
    _request: &DetectRequest,
    _response: Option<&DetectResponse>,
    context: &DetectContext,
) -> AttributionFacts {
    let at_unix_ms = context.clock.now_unix_ms();
    AttributionFacts {
        correlation: Correlation {
            event_id: smol_str::SmolStr::new("noodle-detect-v1"),
            turn_id: None,
            session_id: context.session_id.clone(),
            agent_run_id: None,
            at_unix_ms,
        },
        hints: Vec::new(),
        artifacts: Vec::new(),
        audits: Vec::new(),
        resolved: None,
        round_trip: None,
        at_unix_ms,
    }
}
