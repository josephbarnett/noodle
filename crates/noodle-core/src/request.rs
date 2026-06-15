//! `NormalizedRequest` — the request-side analog of
//! [`NormalizedEvent`](crate::event::NormalizedEvent) (ADR 018
//! §2.2). Per-domain L5 *request* codecs decode a wire envelope
//! into this; transforms mutate it; the same codec encodes it
//! back. The engine and the attribution enhancer never name a
//! vendor — all wire-format knowledge lives in the per-domain
//! codecs.
//!
//! ADR 019: this is the normalized payload that flows through a
//! `request→upstream` cell; the enhancer is one capability bound
//! to that cell.

use smol_str::SmolStr;

use crate::event::Role;

/// One message in the normalized request conversation. For
/// stateful backends (claude.ai: server-side history keyed by
/// conversation id) this is typically just the new user turn;
/// for stateless ones (api.anthropic.com Messages: full history
/// resent every turn) it is the whole list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMessage {
    pub role: Role,
    pub content: String,
}

impl RequestMessage {
    #[must_use]
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// The abstract system / steering slot (ADR 018 §2.3, §6).
///
/// `existing` is whatever the wire carried — the app's or user's
/// system prompt (`api.anthropic.com` `system`), or
/// `claude.ai`'s `personalized_styles` text. `directive` is what
/// an enhancer set. The per-domain *encoder* composes the two
/// onto the right wire field; keeping them separate is the
/// invariant that prevents an enhancer from clobbering the
/// caller's own system prompt.
///
/// **Idempotency (ADR 018 §6):** an enhancer only ever writes
/// `directive`, and the steering slot never round-trips back
/// through the client (claude.ai resends a client-rebuilt slot
/// every turn; api.anthropic.com is stateless). So
/// [`set_directive`](Self::set_directive) is a plain replacement
/// — no marker tracking, no accumulation, safe to apply on every
/// request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemDirective {
    existing: Option<SmolStr>,
    directive: Option<SmolStr>,
}

impl SystemDirective {
    /// Empty slot — no wire system text, no enhanced directive.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Slot seeded from the wire's existing system/steering text.
    #[must_use]
    pub fn from_wire(existing: Option<impl Into<SmolStr>>) -> Self {
        Self {
            existing: existing.map(Into::into),
            directive: None,
        }
    }

    /// The caller's original system text, untouched by enhancement.
    #[must_use]
    pub fn existing(&self) -> Option<&str> {
        self.existing.as_deref()
    }

    /// The enhanced directive, if any.
    #[must_use]
    pub fn directive(&self) -> Option<&str> {
        self.directive.as_deref()
    }

    #[must_use]
    pub fn is_directive_set(&self) -> bool {
        self.directive.is_some()
    }

    /// Set/replace the enhanced directive. Idempotent by
    /// replacement (ADR 018 §6) — applying it on every request is
    /// correct and required; the prior value is never read back.
    pub fn set_directive(&mut self, text: impl Into<SmolStr>) {
        self.directive = Some(text.into());
    }

    /// The steering text the per-domain encoder writes to the
    /// wire: the caller's `existing` text with the `directive`
    /// appended after a blank line. `None` when neither is
    /// present. Existing comes first so enhancement is additive to
    /// the caller's configuration, never destructive.
    #[must_use]
    pub fn composed(&self) -> Option<String> {
        match (self.existing.as_deref(), self.directive.as_deref()) {
            (None, None) => None,
            (Some(e), None) => Some(e.to_owned()),
            (None, Some(d)) => Some(d.to_owned()),
            (Some(e), Some(d)) => Some(format!("{e}\n\n{d}")),
        }
    }
}

/// Vendor-agnostic normalized request (ADR 018 §2.2). The
/// `AttributionEnhancer` mutates **only** `system` (via
/// [`SystemDirective::set_directive`]); it never edits
/// `messages`, `model`, or tool configuration — see ADR 018 §6
/// (conversation integrity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRequest {
    /// Model identifier as carried on the wire, when present.
    pub model: Option<SmolStr>,
    /// The conversation turn(s). May be just the new user turn
    /// (stateful backend) or the full list (stateless).
    pub messages: Vec<RequestMessage>,
    /// The abstract steering slot the enhancer writes.
    pub system: SystemDirective,
}

impl NormalizedRequest {
    #[must_use]
    pub fn new(
        model: Option<impl Into<SmolStr>>,
        messages: Vec<RequestMessage>,
        system: SystemDirective,
    ) -> Self {
        Self {
            model: model.map(Into::into),
            messages,
            system,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slot_has_nothing() {
        let s = SystemDirective::none();
        assert!(s.existing().is_none());
        assert!(s.directive().is_none());
        assert!(!s.is_directive_set());
        assert!(s.composed().is_none());
    }

    #[test]
    fn from_wire_preserves_existing_no_directive() {
        let s = SystemDirective::from_wire(Some("Normal\n"));
        assert_eq!(s.existing(), Some("Normal\n"));
        assert!(!s.is_directive_set());
        assert_eq!(s.composed().as_deref(), Some("Normal\n"));
    }

    #[test]
    fn set_directive_is_replacement_not_accumulation() {
        // ADR 018 §6: applying on every request must not stack.
        let mut s = SystemDirective::from_wire(Some("sys"));
        s.set_directive("D1");
        s.set_directive("D2");
        assert_eq!(s.directive(), Some("D2"));
        // Existing is never touched by enhancement.
        assert_eq!(s.existing(), Some("sys"));
        assert_eq!(s.composed().as_deref(), Some("sys\n\nD2"));
    }

    #[test]
    fn composed_directive_only() {
        let mut s = SystemDirective::none();
        s.set_directive("just directive");
        assert_eq!(s.composed().as_deref(), Some("just directive"));
    }

    #[test]
    fn composed_orders_existing_before_directive() {
        let mut s = SystemDirective::from_wire(Some("caller system"));
        s.set_directive("noodle directive");
        // Existing first → enhancement is additive, not destructive.
        assert_eq!(
            s.composed().as_deref(),
            Some("caller system\n\nnoodle directive"),
        );
    }

    #[test]
    fn normalized_request_construct_and_inspect() {
        let req = NormalizedRequest::new(
            Some("claude-haiku-4-5"),
            vec![RequestMessage::new(Role::User, "what is a mitm?")],
            SystemDirective::from_wire(Some("Normal\n")),
        );
        assert_eq!(req.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[0].content, "what is a mitm?");
        assert_eq!(req.system.existing(), Some("Normal\n"));
        assert!(!req.system.is_directive_set());
    }

    #[test]
    fn enhancer_touches_only_system_not_messages_or_model() {
        // ADR 018 §6 conversation-integrity invariant, expressed
        // as a test: mutating `system` leaves the rest identical.
        let base = NormalizedRequest::new(
            Some("m"),
            vec![RequestMessage::new(Role::User, "hi")],
            SystemDirective::none(),
        );
        let mut enhanced = base.clone();
        enhanced.system.set_directive("tag your work");
        assert_eq!(enhanced.model, base.model);
        assert_eq!(enhanced.messages, base.messages);
        assert!(enhanced.system.is_directive_set());
        assert!(!base.system.is_directive_set());
    }
}
