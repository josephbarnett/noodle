//! TLS MITM primitives — self-signed CA and the in-process leaf
//! signer.
//!
//! Carved out of `noodle-adapters` per ADR 039 §4. These are the only
//! truly proxy-host-coupled modules in `noodle-adapters` — every
//! other submodule is pure logic compilable to WASM. Pulling
//! `rcgen` (→ `ring` → `getrandom`) and `std::fs` is what made
//! `noodle-adapters` impossible to ship in the plugin topology;
//! relocating those two modules here removes that block.
//!
//! Modules:
//!
//! - [`ca`] — self-signed root CA (generate / persist / `load_static`).
//! - [`local`] — [`local::LocalCertMintService`] implements
//!   [`noodle_core::CertMintService`] in-process from the loaded
//!   [`ca::Ca`].
//!
//! The CSR-over-HTTPS external signer lives in `noodle-cert-external`
//! (separate carve-out, ADR 039 §4 row 2).

#![forbid(unsafe_code)]

pub mod ca;
pub mod local;

pub use local::LocalCertMintService;
