//! Anthropic-specific tag constants.
//!
//! These name single-vendor patterns observed in Anthropic's
//! `api.anthropic.com` and `claude.ai` traffic. Per ADR 029 §3 they
//! are not yet first-class canonical variants — they need 3+ vendor
//! recurrence for promotion. Until then, consumers that know
//! Anthropic-shaped traffic read these tags directly off
//! `VendorSpecific(VendorTag { tag, .. })`.
//!
//! Tags are exposed as `&'static str` so callers can pattern-match
//! against them without allocating.

/// Anthropic's auto-enhanced `<system-reminder>` block carrying the
/// agent's open files or working-directory snapshot.
pub const TAG_REMINDER_WORKING_DIR: &str = "anthropic.reminder.working_dir";

/// Anthropic's `cache_control: ephemeral` content-block annotation —
/// not a content category in itself, but a vendor-flavoured marker.
pub const TAG_CACHE_CONTROL_EPHEMERAL: &str = "anthropic.cache_control.ephemeral";

/// `stop_reason: refusal` (server-side classifier hit) — distinct
/// from the model's own refusal speech act.
pub const TAG_STOP_REFUSAL: &str = "anthropic.stop_reason.refusal";

/// `stop_reason: pause_turn` — partial-turn checkpoint used by
/// long-running tool flows.
pub const TAG_STOP_PAUSE_TURN: &str = "anthropic.stop_reason.pause_turn";
