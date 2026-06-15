//! Inbound ports.
//!
//! Adapters in `crate::adapters` implement these and the
//! [`crate::hub::HubService`] subscribes to fan messages out to
//! WebSocket clients.
//!
//! Two distinct sources today — whole-exchange (`tap.jsonl`) and
//! per-frame (`frames.jsonl`). They're separate traits rather than
//! one polymorphic feed because each adapter has one concern: a
//! `FrameSource` parser knows nothing about request/response shape,
//! and vice versa.

use tokio::sync::mpsc;

use crate::model::{DecodedExchange, Exchange, Frame, SideEffectEvent};

/// Inbound: a stream of whole-exchange (`tap.jsonl`) events.
///
/// Implementors spawn their own tokio tasks; `subscribe` returns the
/// receiver side of a channel they push events onto.
pub trait EventSource: Send + Sync + 'static {
    fn subscribe(&self) -> mpsc::Receiver<Exchange>;
}

/// Inbound: a stream of per-frame SSE events (`frames.jsonl`).
///
/// Same single-consumer contract as [`EventSource`] — `subscribe`
/// takes the channel receiver out exactly once. The hub owns the
/// only legitimate caller.
pub trait FrameSource: Send + Sync + 'static {
    fn subscribe(&self) -> mpsc::Receiver<Frame>;
}

/// Inbound: a stream of attribution side-effects
/// (`side_effects.jsonl`).
///
/// Same single-consumer contract as [`EventSource`]. Carries the
/// engine's emitted `Hint`/`Artifact`/`Audit`/`Resolved` records
/// for the viewer's attribution panel (ADR 020 §7, item 4
/// follow-on).
pub trait SideEffectSource: Send + Sync + 'static {
    fn subscribe(&self) -> mpsc::Receiver<SideEffectEvent>;
}

/// Inbound: a stream of typed [`DecodedExchange`]s (S21 of the
/// 027–031 refactor — refactor-overview.md §10).
///
/// Same single-consumer contract as [`EventSource`], but the
/// receiver carries the typed `DecodedExchange` produced by the
/// [`crate::decoders::ProviderDecoderRegistry`] — every field the
/// proxy populated on the `tap.jsonl` record, projected through
/// `noodle-domain`'s typed vocabulary.
///
/// New consumers (exec-claude e2e, the upcoming S22 frontend
/// refresh) subscribe here. The legacy [`EventSource`] / `Exchange`
/// path keeps working in parallel until S22 lands.
pub trait DecodedExchangeSource: Send + Sync + 'static {
    fn subscribe(&self) -> mpsc::Receiver<DecodedExchange>;
}
