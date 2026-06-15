//! `noodle-viewer` — live debug viewer for noodle.
//!
//! Hexagonal layout (see `docs/adrs/007-viewer-architecture.md`):
//!
//! ```text
//! ports/event_source.rs   - inbound: where typed events come from
//! ports/client_channel.rs - outbound: where typed events go
//! ports/debug_proxy.rs    - outbound: forward capture controls
//!
//! adapters/tap_jsonl_source.rs  - tap.jsonl reader (S15: backed by
//!                                  `noodle_tap::source::FileTail`)
//! adapters/http_debug_proxy.rs  - HTTP client → noodle's :9091 API
//!
//! hub.rs                  - service: parse + broadcast
//! model.rs                - typed events on the wire
//! server/                 - axum HTTP/WS bootstrap
//! ```

#![forbid(unsafe_code)]

pub mod adapters;
pub mod brain_observer;
pub mod decoders;
pub mod hub;
pub mod model;
pub mod ports;
pub mod server;
