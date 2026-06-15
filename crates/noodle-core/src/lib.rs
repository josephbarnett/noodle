#![allow(deprecated)]
// A.8.a: this module wires or carries legacy ProviderCodec types as a fallback for `NOODLE_LAYERED_CORE=0`. Layered is the production default; legacy registration is deprecated-but-supported until A.8.b removes the opt-out and migrates tests.

//! noodle-core: domain types and ports.
//!
//! Hexagonal architecture: this crate is the **domain core**. It defines the
//! types the application reasons about (events, sessions, markers) and the
//! **ports** (traits) it uses to talk to the outside world. It deliberately
//! pulls in no async runtime, no HTTP framework, no rama, no tokio. Driven
//! adapters live in `noodle-adapters`; the driving adapter (rama service
//! stack) lives in `noodle-proxy`.
//!
//! See `docs/adrs/002-hexagonal-and-patterns.md` for the architecture
//! rationale and pattern catalog, and `docs/adrs/005-trait-refactor.md`
//! for the three-role surface (`Detector` / `ContextEnhancer` / `Filter` / `ProviderCodec`).

#![forbid(unsafe_code)]

pub mod audit;
pub mod cert;
pub mod codec;
pub mod config;
pub mod detector;
pub mod endpoint;
pub mod engine;
pub mod enhancer;
pub mod event;
pub mod filter;
pub mod layered;
pub mod marker;
pub mod marking;
pub mod probe;
pub mod request;
pub mod resolver;
pub mod session;
pub mod store;
pub mod stream;
pub mod wire;

pub use audit::{AuditEvent, AuditSink};
pub use cert::{
    CertMintService, DynCertMintAdapter, DynCertMintService, LeafCert, LeafRequest, MintError,
};
pub use codec::{CodecRegistry, ProviderCodec, StreamingDecoder};
pub use detector::{
    ContextHint, Detector, DiscardFieldWriter, FieldDetector, FieldValue, FieldWriter,
    FlowResolver, HintWriter, VecHintWriter,
};
pub use endpoint::{EndpointMatcher, HostMatch, PathMatch};
pub use engine::{COMMON_GROUP, InspectionEngine, InspectionEngineBuilder, InspectionPipeline};
pub use enhancer::{ContextEnhancer, DiscoverContext, EnhanceContext};
pub use event::{
    AgentRunId, FinishReason, NormalizedEvent, ProviderChunk, Role, RoundTripId, TurnId, TurnUsage,
};
pub use filter::{Filter, FilterContext, FilterFactory, FilterOutput};
pub use marker::{MarkerHit, MarkerScanner, ScanOutput, is_tag_name_char};
pub use marking::{
    AgentRunDecisionKind, AgentRunState, MarkingDecision, MarkingDecisionKind, MarkingDetector,
    MarkingSessionId, MarkingStore, ParentRunRef, SessionState, SharedMarkingStore, StopReason,
    SystemHash,
};
pub use probe::{RequestProbe, ResponseKind, ResponseShape};
pub use request::{NormalizedRequest, RequestMessage, SystemDirective};
pub use resolver::{CategoryConfig, CategoryDef, Resolved, resolve};
pub use session::{Session, SessionId, SessionKey};
pub use store::SessionStore;
pub use stream::{BodyStream, EventStream};
pub use wire::{
    CacheCreationTtl, HeaderPair, WireDirection, WireEvent, WireLatency, WireMarks, WirePatch,
    WirePatchEntry, WireSink, WireSource, WireSourceSeek, WireTokenUsage, WireUsage,
    provider_from_url,
};

/// Standard boxed-error type for the noodle domain.
/// Driven adapters convert their concrete errors into this at the boundary.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
