//! `noodle-domain` — typed vocabulary for noodle's Agent Protocol.
//!
//! Pure types. No runtime, no HTTP framework, no I/O. Defined by
//! ADR 029; per-crate plan in `docs/adrs/refactor-noodle-domain.md`.
//!
//! ## Module map
//!
//! Content families (§2.1 of ADR 029):
//!
//! - [`speech_act`] — pragmatic intent of a text block
//! - [`content_category`] — what the bytes contain
//! - [`capability`] — the action a tool call performs
//! - [`trust_level`] — how much the harness trusts the source
//! - [`citation_ref`] — external references the content cites
//! - [`reminder_subtype`] — system-reminder kinds
//! - [`task_plan`] — primitives from the agent's planning channel
//! - [`turn_end`] — normalised turn-termination signals
//! - [`envelope_metadata`] — per-record dispatch facts
//!   ([`envelope_metadata::ProviderId`], [`envelope_metadata::Direction`])
//!
//! Operational-context families (§2.2 of ADR 029):
//!
//! - [`observation_context`] — `AgentApp`, `Machine`, `CollectorApp`
//! - [`principal_identity`] — non-PII actor / device / role keys
//! - [`usage`] — token usage, latency, retry counts
//! - [`subscription_context`] — API-key fingerprint, org context, tier
//!
//! Vendor subtypes and decoder libraries:
//!
//! - [`vendor`] — `VendorId`, `VendorTag`, per-vendor tag tables
//! - [`decoders`] — per-provider decoder libraries (S14)
//! - [`classifier`] — `Classifier` trait surface
//!
//! ## Extensibility contract
//!
//! Every consumer that pattern-matches on a `noodle-domain` enum
//! **must** include a `_` arm that handles unknown variants
//! gracefully (ADR 029 §4.1). New canonical variants are added under
//! the cross-vendor recurrence rule (§3); single-vendor patterns are
//! carried via each family's `VendorSpecific` variant.

pub mod capability;
pub mod citation_ref;
pub mod classifier;
pub mod content_category;
pub mod decoders;
pub mod envelope_metadata;
pub mod observation_context;
pub mod principal_identity;
pub mod reminder_subtype;
pub mod speech_act;
pub mod subscription_context;
pub mod task_plan;
pub mod trust_level;
pub mod turn_end;
pub mod usage;
pub mod vendor;

// Convenience re-exports of the most-commonly-used types. Wider
// consumers should still reach into the family modules directly.
pub use envelope_metadata::{Direction, EndpointPath, ProviderId, RoundTripIndex};
pub use vendor::{VendorId, VendorTag};
