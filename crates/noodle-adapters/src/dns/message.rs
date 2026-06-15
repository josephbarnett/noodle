//! DNS message types.
//!
//! Layout follows RFC 1035 for header / question / RR framing,
//! and RFC 9460 for the HTTPS RR's SVCB-style `SvcParams`. Other
//! record types pass through as opaque RDATA bytes — this module
//! is scoped to what's needed for h3-stripping, not RFC-complete
//! DNS handling.

use bytes::Bytes;
use smol_str::SmolStr;

/// Error returned by wire parse / serialize.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("truncated DNS message: needed {needed} bytes at offset {at}, have {have}")]
    Truncated {
        needed: usize,
        at: usize,
        have: usize,
    },
    #[error("invalid DNS name: {reason}")]
    InvalidName { reason: &'static str },
    #[error("name compression pointer loops or exceeds depth")]
    PointerLoop,
    #[error("HTTPS RR rdata malformed: {reason}")]
    BadHttpsRdata { reason: &'static str },
    #[error("encoded message exceeds 65535 bytes")]
    TooLarge,
}

/// A DNS message — header, questions, and records.
///
/// Question sections carry the queried name + type. The four
/// record sections (`answers`, `authorities`, `additionals`) hold
/// resource records. Order within each section is preserved by
/// round-trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsMessage {
    pub header: DnsHeader,
    pub questions: Vec<DnsQuestion>,
    pub answers: Vec<DnsRecord>,
    pub authorities: Vec<DnsRecord>,
    pub additionals: Vec<DnsRecord>,
}

/// DNS header (RFC 1035 §4.1.1). Twelve bytes on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsHeader {
    pub id: u16,
    pub flags: DnsFlags,
}

/// Wire flags packed into the header's second u16. Stored as the
/// raw 16-bit field so round-trip is bit-exact regardless of how
/// each flag is interpreted at higher layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DnsFlags(pub u16);

impl DnsFlags {
    /// QR bit — `true` for a response, `false` for a query.
    #[must_use]
    pub fn is_response(self) -> bool {
        (self.0 & 0x8000) != 0
    }

    /// RCODE field (low nibble).
    #[must_use]
    pub fn rcode(self) -> u8 {
        (self.0 & 0x000f) as u8
    }
}

/// A question section entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsQuestion {
    pub name: DnsName,
    pub qtype: DnsRecordType,
    pub qclass: DnsClass,
}

/// A resource record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsRecord {
    pub name: DnsName,
    pub class: DnsClass,
    pub ttl: u32,
    pub data: DnsRecordData,
}

/// DNS record type. We name the ones we care about and store the
/// rest as opaque numeric codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DnsRecordType {
    A,
    Aaaa,
    Cname,
    Https,
    Svcb,
    Other(u16),
}

impl DnsRecordType {
    /// Convert from on-wire numeric code.
    #[must_use]
    pub fn from_code(code: u16) -> Self {
        match code {
            1 => DnsRecordType::A,
            5 => DnsRecordType::Cname,
            28 => DnsRecordType::Aaaa,
            64 => DnsRecordType::Svcb,
            65 => DnsRecordType::Https,
            other => DnsRecordType::Other(other),
        }
    }

    /// Convert back to the on-wire numeric code.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            DnsRecordType::A => 1,
            DnsRecordType::Cname => 5,
            DnsRecordType::Aaaa => 28,
            DnsRecordType::Svcb => 64,
            DnsRecordType::Https => 65,
            DnsRecordType::Other(c) => c,
        }
    }
}

/// DNS class. In practice `IN` (Internet, code 1) is universal;
/// other classes pass through as opaque codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DnsClass {
    In,
    Other(u16),
}

impl DnsClass {
    /// Convert from on-wire numeric code.
    #[must_use]
    pub fn from_code(code: u16) -> Self {
        match code {
            1 => DnsClass::In,
            other => DnsClass::Other(other),
        }
    }

    /// Convert back to the on-wire numeric code.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            DnsClass::In => 1,
            DnsClass::Other(c) => c,
        }
    }
}

/// A DNS owner / target name, parsed from labels.
///
/// Stored as an ordered list of labels (root represented as an
/// empty `labels` vector). Compression pointers encountered during
/// parsing are dereferenced; on encode the labels are emitted
/// uncompressed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DnsName {
    pub labels: Vec<SmolStr>,
}

impl DnsName {
    /// Root domain (`.`).
    #[must_use]
    pub fn root() -> Self {
        Self { labels: Vec::new() }
    }

    /// Construct from a dotted ASCII string (case-insensitive).
    /// Returns an `InvalidName` error if a label is empty, longer
    /// than 63 bytes, or contains non-ASCII bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::InvalidName`] when the input has
    /// empty labels, oversized labels (> 63 bytes), or non-ASCII
    /// content.
    pub fn from_ascii(s: &str) -> Result<Self, WireError> {
        if s == "." || s.is_empty() {
            return Ok(Self::root());
        }
        let trimmed = s.strip_suffix('.').unwrap_or(s);
        let mut labels = Vec::new();
        for label in trimmed.split('.') {
            if label.is_empty() {
                return Err(WireError::InvalidName {
                    reason: "empty label",
                });
            }
            if label.len() > 63 {
                return Err(WireError::InvalidName {
                    reason: "label longer than 63 bytes",
                });
            }
            if !label.is_ascii() {
                return Err(WireError::InvalidName {
                    reason: "non-ASCII byte",
                });
            }
            labels.push(SmolStr::new(label));
        }
        Ok(Self { labels })
    }

    /// Render as dotted ASCII (no trailing dot, lowercased).
    #[must_use]
    pub fn to_ascii(&self) -> String {
        self.labels
            .iter()
            .map(|l| l.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// Typed RDATA. Records we don't parse specifically are kept as
/// raw bytes plus their numeric type so the wire round-trip
/// preserves them faithfully.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DnsRecordData {
    /// IPv4 address (RFC 1035 §3.4.1).
    A(std::net::Ipv4Addr),
    /// IPv6 address (RFC 3596).
    Aaaa(std::net::Ipv6Addr),
    /// HTTPS RR (RFC 9460). Type 65.
    Https(HttpsRecord),
    /// Any other type — RDATA preserved verbatim.
    Other { rtype: u16, rdata: Bytes },
}

impl DnsRecordData {
    /// Record type of this rdata.
    #[must_use]
    pub fn rtype(&self) -> DnsRecordType {
        match self {
            DnsRecordData::A(_) => DnsRecordType::A,
            DnsRecordData::Aaaa(_) => DnsRecordType::Aaaa,
            DnsRecordData::Https(_) => DnsRecordType::Https,
            DnsRecordData::Other { rtype, .. } => DnsRecordType::from_code(*rtype),
        }
    }
}

/// HTTPS resource record (RFC 9460).
///
/// `priority == 0` is the `AliasMode` form; non-zero is
/// `ServiceMode` carrying `params`. We keep both forms uniform;
/// `AliasMode` messages will have `params` empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpsRecord {
    pub priority: u16,
    pub target: DnsName,
    pub params: Vec<SvcParam>,
}

/// One `SvcParam` entry (key + value pair) inside an HTTPS RR.
///
/// `key` is the registered [`SvcParamKey`]; `value` carries the
/// parsed shape when we recognize the key, or opaque bytes
/// otherwise. Round-trip invariant: encoding preserves the
/// original wire bytes for `Unknown` values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvcParam {
    pub key: SvcParamKey,
    pub value: SvcParamValue,
}

/// Named `SvcParamKey` values we care about, plus opaque
/// passthrough for everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SvcParamKey {
    /// `mandatory` (RFC 9460 §8) — key=0.
    Mandatory,
    /// `alpn` — key=1. The QUIC discovery hint we strip.
    Alpn,
    /// `no-default-alpn` — key=2.
    NoDefaultAlpn,
    /// `port` — key=3.
    Port,
    /// `ipv4hint` — key=4.
    Ipv4Hint,
    /// `ech` (Encrypted Client Hello) — key=5. We strip this
    /// alongside `alpn=h3` so the TLS MITM can see plaintext SNI.
    Ech,
    /// `ipv6hint` — key=6.
    Ipv6Hint,
    /// Anything we don't have a named case for; carried by its
    /// numeric key.
    Other(u16),
}

impl SvcParamKey {
    /// Convert from on-wire numeric key.
    #[must_use]
    pub fn from_code(code: u16) -> Self {
        match code {
            0 => SvcParamKey::Mandatory,
            1 => SvcParamKey::Alpn,
            2 => SvcParamKey::NoDefaultAlpn,
            3 => SvcParamKey::Port,
            4 => SvcParamKey::Ipv4Hint,
            5 => SvcParamKey::Ech,
            6 => SvcParamKey::Ipv6Hint,
            other => SvcParamKey::Other(other),
        }
    }

    /// Convert back to the on-wire numeric key.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            SvcParamKey::Mandatory => 0,
            SvcParamKey::Alpn => 1,
            SvcParamKey::NoDefaultAlpn => 2,
            SvcParamKey::Port => 3,
            SvcParamKey::Ipv4Hint => 4,
            SvcParamKey::Ech => 5,
            SvcParamKey::Ipv6Hint => 6,
            SvcParamKey::Other(c) => c,
        }
    }
}

/// `SvcParamValue` typed for the keys we parse; opaque otherwise.
///
/// Each variant maps to one [`SvcParamKey`]. The `Unknown`
/// variant carries the raw bytes for any key we don't parse —
/// this is how we round-trip records carrying unfamiliar
/// `SvcParam`s without dropping them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SvcParamValue {
    /// `alpn` value: a list of ALPN protocol identifiers
    /// (e.g. `["h2", "h3"]`).
    Alpn(Vec<SmolStr>),
    /// `ech` value: opaque (the encrypted `ClientHello` config).
    Ech(Bytes),
    /// Any value we don't parse — preserved verbatim.
    Unknown(Bytes),
}

impl HttpsRecord {
    /// Return `true` when `alpn` lists `h3` (or any explicit h3
    /// variant such as `h3-29`, `h3-Q050`).
    #[must_use]
    pub fn advertises_h3(&self) -> bool {
        for p in &self.params {
            if p.key == SvcParamKey::Alpn
                && let SvcParamValue::Alpn(protos) = &p.value
            {
                return protos.iter().any(|s| {
                    let lower = s.to_ascii_lowercase();
                    lower == "h3" || lower.starts_with("h3-")
                });
            }
        }
        false
    }

    /// Return `true` when an `ech` `SvcParam` is present.
    #[must_use]
    pub fn has_ech(&self) -> bool {
        self.params.iter().any(|p| p.key == SvcParamKey::Ech)
    }
}
