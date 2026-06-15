//! Hexagonal ports.
//!
//! Each port is a trait that describes a contract; adapters in
//! `crate::adapters` provide concrete implementations.

pub mod debug_proxy;
pub mod event_source;

pub use debug_proxy::{CaptureVerb, DebugProxy, DebugProxyError};
pub use event_source::{DecodedExchangeSource, EventSource, FrameSource, SideEffectSource};
