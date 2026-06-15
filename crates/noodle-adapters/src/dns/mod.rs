//! DNS adapter — types and wire parsing/serialization for the
//! L2 DNS sibling branch (015 §2).
//!
//! Currently ships **story 027.a, partial**: the [`DnsMessage`]
//! type plus wire parse/serialize with byte-exact round-trip on
//! unmodified messages. The `DnsWireCodec` (027.b) and the
//! [`Transform<DnsMessage>`][noodle_core::layered::Transform] set
//! (`StripH3`, `StripEch`, 027.c) build on this.
//!
//! Scope: minimum-viable DNS surface for h3-stripping use case
//! per 014 §5.1 (Option A). Supports the HTTPS RR (TYPE 65, RFC
//! 9460) with its `SvcParams` — the only record type we need to
//! inspect for the QUIC suppression workflow. All other RR types
//! pass through as opaque RDATA byte sequences.
//!
//! Round-trip invariant (per 015 §2.1.1): if `DnsMessage::decode`
//! returns `Ok(msg)`, then `msg.encode()` produces bytes that
//! re-parse to an equivalent `DnsMessage`. Strict byte-equality
//! holds for unmodified messages whose names do not use
//! compression pointers. Names *are* compression-aware on parse,
//! but the encoder writes uncompressed names (which decoders
//! must accept; RFC 1035 §4.1.4).

mod codec;
mod message;
mod transforms;
mod wire;

pub use codec::{DnsWireCodec, DnsWireCodecInstance};
pub use message::{
    DnsClass, DnsFlags, DnsHeader, DnsMessage, DnsName, DnsQuestion, DnsRecord, DnsRecordData,
    DnsRecordType, HttpsRecord, SvcParam, SvcParamKey, SvcParamValue, WireError,
};
pub use transforms::{StripEch, StripEchInstance, StripH3, StripH3Instance};
