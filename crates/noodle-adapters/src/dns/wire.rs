//! Wire-format codec for [`super::DnsMessage`].
//!
//! Implements parse / serialize for the subset of RFC 1035 and
//! RFC 9460 the h3-stripping workflow needs.

use bytes::Bytes;
use smol_str::SmolStr;

use super::message::{
    DnsClass, DnsFlags, DnsHeader, DnsMessage, DnsName, DnsQuestion, DnsRecord, DnsRecordData,
    DnsRecordType, HttpsRecord, SvcParam, SvcParamKey, SvcParamValue, WireError,
};

const MAX_NAME_POINTERS: usize = 32;

impl DnsMessage {
    /// Parse a DNS message from a buffer.
    ///
    /// # Errors
    ///
    /// Returns [`WireError`] on truncation, invalid name encoding,
    /// pointer loops exceeding [`MAX_NAME_POINTERS`], or malformed
    /// HTTPS RR rdata.
    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        let mut p = Parser::new(buf);
        let header_bytes = p.read_slice(12)?;
        let id = u16::from_be_bytes([header_bytes[0], header_bytes[1]]);
        let flags = DnsFlags(u16::from_be_bytes([header_bytes[2], header_bytes[3]]));
        let question_count = u16::from_be_bytes([header_bytes[4], header_bytes[5]]);
        let answer_count = u16::from_be_bytes([header_bytes[6], header_bytes[7]]);
        let authority_count = u16::from_be_bytes([header_bytes[8], header_bytes[9]]);
        let additional_count = u16::from_be_bytes([header_bytes[10], header_bytes[11]]);

        let header = DnsHeader { id, flags };

        let mut questions = Vec::with_capacity(question_count as usize);
        for _ in 0..question_count {
            questions.push(p.read_question()?);
        }

        let answers = read_records(&mut p, answer_count as usize)?;
        let authorities = read_records(&mut p, authority_count as usize)?;
        let additionals = read_records(&mut p, additional_count as usize)?;

        Ok(DnsMessage {
            header,
            questions,
            answers,
            authorities,
            additionals,
        })
    }

    /// Serialize the message to a fresh byte buffer.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooLarge`] when the encoded length
    /// would exceed 65 535 bytes (the UDP DNS limit before EDNS).
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(&self.header.id.to_be_bytes());
        out.extend_from_slice(&self.header.flags.0.to_be_bytes());
        out.extend_from_slice(&u16_count(self.questions.len())?);
        out.extend_from_slice(&u16_count(self.answers.len())?);
        out.extend_from_slice(&u16_count(self.authorities.len())?);
        out.extend_from_slice(&u16_count(self.additionals.len())?);

        for q in &self.questions {
            encode_name(&q.name, &mut out)?;
            out.extend_from_slice(&q.qtype.code().to_be_bytes());
            out.extend_from_slice(&q.qclass.code().to_be_bytes());
        }
        for r in &self.answers {
            encode_record(r, &mut out)?;
        }
        for r in &self.authorities {
            encode_record(r, &mut out)?;
        }
        for r in &self.additionals {
            encode_record(r, &mut out)?;
        }

        if out.len() > u16::MAX as usize {
            return Err(WireError::TooLarge);
        }
        Ok(out)
    }
}

fn u16_count(n: usize) -> Result<[u8; 2], WireError> {
    let n = u16::try_from(n).map_err(|_| WireError::TooLarge)?;
    Ok(n.to_be_bytes())
}

fn read_records(p: &mut Parser<'_>, count: usize) -> Result<Vec<DnsRecord>, WireError> {
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(p.read_record()?);
    }
    Ok(out)
}

/// Streaming parser over a flat DNS message buffer with name-
/// compression pointer resolution.
struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn read_slice(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::Truncated {
                needed: n,
                at: self.pos,
                have: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn read_u16(&mut self) -> Result<u16, WireError> {
        let s = self.read_slice(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, WireError> {
        let s = self.read_slice(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn read_name(&mut self) -> Result<DnsName, WireError> {
        let mut labels = Vec::new();
        let mut visited = 0usize;
        let mut return_to: Option<usize> = None;
        loop {
            if self.remaining() == 0 {
                return Err(WireError::Truncated {
                    needed: 1,
                    at: self.pos,
                    have: 0,
                });
            }
            let prefix = self.buf[self.pos];
            if prefix == 0 {
                self.pos += 1;
                break;
            }
            if (prefix & 0xc0) == 0xc0 {
                // Compression pointer.
                let hi = (prefix & 0x3f) as usize;
                let lo = self
                    .buf
                    .get(self.pos + 1)
                    .copied()
                    .ok_or(WireError::Truncated {
                        needed: 2,
                        at: self.pos,
                        have: self.remaining(),
                    })? as usize;
                let target = (hi << 8) | lo;
                if target >= self.buf.len() {
                    return Err(WireError::InvalidName {
                        reason: "pointer out of range",
                    });
                }
                if return_to.is_none() {
                    return_to = Some(self.pos + 2);
                }
                self.pos = target;
                visited += 1;
                if visited > MAX_NAME_POINTERS {
                    return Err(WireError::PointerLoop);
                }
                continue;
            }
            if (prefix & 0xc0) != 0 {
                return Err(WireError::InvalidName {
                    reason: "reserved length bits set",
                });
            }
            self.pos += 1;
            let len = prefix as usize;
            if len > 63 {
                return Err(WireError::InvalidName {
                    reason: "label longer than 63 bytes",
                });
            }
            let bytes = self.read_slice(len)?;
            if !bytes.is_ascii() {
                return Err(WireError::InvalidName {
                    reason: "non-ASCII byte",
                });
            }
            let s = std::str::from_utf8(bytes).map_err(|_| WireError::InvalidName {
                reason: "invalid UTF-8 in label",
            })?;
            labels.push(SmolStr::new(s));
        }
        if let Some(after) = return_to {
            self.pos = after;
        }
        Ok(DnsName { labels })
    }

    fn read_question(&mut self) -> Result<DnsQuestion, WireError> {
        let name = self.read_name()?;
        let qtype = DnsRecordType::from_code(self.read_u16()?);
        let qclass = DnsClass::from_code(self.read_u16()?);
        Ok(DnsQuestion {
            name,
            qtype,
            qclass,
        })
    }

    fn read_record(&mut self) -> Result<DnsRecord, WireError> {
        let name = self.read_name()?;
        let rtype_code = self.read_u16()?;
        let class = DnsClass::from_code(self.read_u16()?);
        let ttl = self.read_u32()?;
        let rdlen = self.read_u16()? as usize;
        let rdata_start = self.pos;
        if self.remaining() < rdlen {
            return Err(WireError::Truncated {
                needed: rdlen,
                at: self.pos,
                have: self.remaining(),
            });
        }
        let rtype = DnsRecordType::from_code(rtype_code);
        let data = match rtype {
            DnsRecordType::A => {
                let b = self.read_slice(rdlen)?;
                if b.len() != 4 {
                    return Err(WireError::InvalidName {
                        reason: "A record rdata not 4 bytes",
                    });
                }
                DnsRecordData::A(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]))
            }
            DnsRecordType::Aaaa => {
                let b = self.read_slice(rdlen)?;
                if b.len() != 16 {
                    return Err(WireError::InvalidName {
                        reason: "AAAA rdata not 16 bytes",
                    });
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(b);
                DnsRecordData::Aaaa(std::net::Ipv6Addr::from(octets))
            }
            DnsRecordType::Https | DnsRecordType::Svcb => {
                let https = read_https_rdata(self, rdata_start, rdlen)?;
                DnsRecordData::Https(https)
            }
            DnsRecordType::Cname | DnsRecordType::Other(_) => {
                let b = self.read_slice(rdlen)?;
                DnsRecordData::Other {
                    rtype: rtype_code,
                    rdata: Bytes::copy_from_slice(b),
                }
            }
        };
        Ok(DnsRecord {
            name,
            class,
            ttl,
            data,
        })
    }
}

fn read_https_rdata(
    p: &mut Parser<'_>,
    rdata_start: usize,
    rdlen: usize,
) -> Result<HttpsRecord, WireError> {
    let rdata_end = rdata_start + rdlen;
    let priority = p.read_u16()?;
    let target = p.read_name()?;
    let mut params = Vec::new();
    while p.pos < rdata_end {
        let key = SvcParamKey::from_code(p.read_u16()?);
        let vlen = p.read_u16()? as usize;
        if p.pos + vlen > rdata_end {
            return Err(WireError::BadHttpsRdata {
                reason: "SvcParam value runs past rdata end",
            });
        }
        let raw = Bytes::copy_from_slice(p.read_slice(vlen)?);
        let value = match key {
            SvcParamKey::Alpn => SvcParamValue::Alpn(parse_alpn_list(&raw)?),
            SvcParamKey::Ech => SvcParamValue::Ech(raw),
            _ => SvcParamValue::Unknown(raw),
        };
        params.push(SvcParam { key, value });
    }
    if p.pos != rdata_end {
        return Err(WireError::BadHttpsRdata {
            reason: "SvcParam section underruns rdata",
        });
    }
    Ok(HttpsRecord {
        priority,
        target,
        params,
    })
}

fn parse_alpn_list(raw: &[u8]) -> Result<Vec<SmolStr>, WireError> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let len = raw[i] as usize;
        i += 1;
        if i + len > raw.len() {
            return Err(WireError::BadHttpsRdata {
                reason: "alpn list runs past value",
            });
        }
        let s = std::str::from_utf8(&raw[i..i + len]).map_err(|_| WireError::BadHttpsRdata {
            reason: "alpn entry not UTF-8",
        })?;
        out.push(SmolStr::new(s));
        i += len;
    }
    Ok(out)
}

fn encode_name(name: &DnsName, out: &mut Vec<u8>) -> Result<(), WireError> {
    for label in &name.labels {
        // Labels are validated to ≤ 63 bytes at parse / construct
        // time (RFC 1035 §3.1); the cast is defensive but
        // unreachable in practice.
        let len = u8::try_from(label.len()).map_err(|_| WireError::InvalidName {
            reason: "label longer than 63 bytes",
        })?;
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

fn encode_record(r: &DnsRecord, out: &mut Vec<u8>) -> Result<(), WireError> {
    encode_name(&r.name, out)?;
    let rtype = r.data.rtype();
    out.extend_from_slice(&rtype.code().to_be_bytes());
    out.extend_from_slice(&r.class.code().to_be_bytes());
    out.extend_from_slice(&r.ttl.to_be_bytes());

    // Reserve two bytes for RDLENGTH; we fill them in after
    // encoding RDATA so we know the byte count.
    let rdlen_pos = out.len();
    out.extend_from_slice(&[0, 0]);
    let rdata_start = out.len();

    match &r.data {
        DnsRecordData::A(ip) => {
            out.extend_from_slice(&ip.octets());
        }
        DnsRecordData::Aaaa(ip) => {
            out.extend_from_slice(&ip.octets());
        }
        DnsRecordData::Https(h) => {
            encode_https_rdata(h, out)?;
        }
        DnsRecordData::Other { rdata, .. } => {
            out.extend_from_slice(rdata);
        }
    }

    let rdata_len = u16::try_from(out.len() - rdata_start).map_err(|_| WireError::TooLarge)?;
    let len_bytes = rdata_len.to_be_bytes();
    out[rdlen_pos] = len_bytes[0];
    out[rdlen_pos + 1] = len_bytes[1];
    Ok(())
}

fn encode_https_rdata(h: &HttpsRecord, out: &mut Vec<u8>) -> Result<(), WireError> {
    out.extend_from_slice(&h.priority.to_be_bytes());
    encode_name(&h.target, out)?;
    for param in &h.params {
        out.extend_from_slice(&param.key.code().to_be_bytes());
        let value_bytes = match &param.value {
            SvcParamValue::Alpn(protos) => {
                let mut v = Vec::new();
                for proto in protos {
                    // ALPN protocol IDs are ≤ 255 bytes per RFC
                    // 7301 §3 — guarded explicitly.
                    let len = u8::try_from(proto.len()).map_err(|_| WireError::BadHttpsRdata {
                        reason: "ALPN protocol ID exceeds 255 bytes",
                    })?;
                    v.push(len);
                    v.extend_from_slice(proto.as_bytes());
                }
                v
            }
            SvcParamValue::Ech(b) | SvcParamValue::Unknown(b) => b.to_vec(),
        };
        let value_len = u16::try_from(value_bytes.len()).map_err(|_| WireError::TooLarge)?;
        out.extend_from_slice(&value_len.to_be_bytes());
        out.extend_from_slice(&value_bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Helpers ───────────────────────────────────────────────────

    fn alpn_value(protos: &[&str]) -> SvcParamValue {
        SvcParamValue::Alpn(protos.iter().copied().map(SmolStr::new).collect())
    }

    // ─── Header / flag tests ───────────────────────────────────────

    #[test]
    fn header_decodes_query_flag_correctly() {
        let buf: Vec<u8> = vec![
            0x12, 0x34, // ID
            0x01, 0x00, // flags: standard query, RD=1
            0x00, 0x00, // QDCOUNT
            0x00, 0x00, // ANCOUNT
            0x00, 0x00, // NSCOUNT
            0x00, 0x00, // ARCOUNT
        ];
        let msg = DnsMessage::decode(&buf).expect("decode succeeds");
        assert_eq!(msg.header.id, 0x1234);
        assert!(!msg.header.flags.is_response(), "QR=0 means query");
        assert_eq!(msg.header.flags.rcode(), 0);
    }

    #[test]
    fn header_decodes_response_flag_correctly() {
        let buf: Vec<u8> = vec![
            0x00, 0x01, 0x81, 0x80, // ID=1, flags: response, no error
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let msg = DnsMessage::decode(&buf).expect("decode succeeds");
        assert!(msg.header.flags.is_response(), "QR=1 means response");
    }

    // ─── Question / record framing ─────────────────────────────────

    #[test]
    fn round_trip_preserves_a_record() {
        let original = DnsMessage {
            header: DnsHeader {
                id: 0xabcd,
                flags: DnsFlags(0x8180),
            },
            questions: vec![DnsQuestion {
                name: DnsName::from_ascii("example.com").unwrap(),
                qtype: DnsRecordType::A,
                qclass: DnsClass::In,
            }],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("example.com").unwrap(),
                class: DnsClass::In,
                ttl: 300,
                data: DnsRecordData::A(std::net::Ipv4Addr::new(93, 184, 216, 34)),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_preserves_aaaa_record() {
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x0001,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("ipv6.example.com").unwrap(),
                class: DnsClass::In,
                ttl: 60,
                data: DnsRecordData::Aaaa("2606:4700:4700::1111".parse().unwrap()),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn round_trip_preserves_opaque_other_record() {
        // A TXT record (type 16) — we don't parse it, just round-
        // trip the rdata bytes. Proves the Other-variant
        // passthrough preserves byte-exact rdata.
        let txt_rdata = Bytes::from_static(b"\x0bhello world");
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x0042,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("text.example").unwrap(),
                class: DnsClass::In,
                ttl: 120,
                data: DnsRecordData::Other {
                    rtype: 16,
                    rdata: txt_rdata.clone(),
                },
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        match &decoded.answers[0].data {
            DnsRecordData::Other { rtype, rdata } => {
                assert_eq!(*rtype, 16);
                assert_eq!(rdata, &txt_rdata);
            }
            _ => panic!("expected Other rdata"),
        }
    }

    // ─── HTTPS RR (RFC 9460) tests ─────────────────────────────────

    #[test]
    fn https_record_round_trips_with_alpn_h2_h3() {
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x1111,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("claude.ai").unwrap(),
                class: DnsClass::In,
                ttl: 300,
                data: DnsRecordData::Https(HttpsRecord {
                    priority: 1,
                    target: DnsName::root(),
                    params: vec![SvcParam {
                        key: SvcParamKey::Alpn,
                        value: alpn_value(&["h3", "h2"]),
                    }],
                }),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn https_record_advertises_h3_detection() {
        let with_h3 = HttpsRecord {
            priority: 1,
            target: DnsName::root(),
            params: vec![SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h2", "h3"]),
            }],
        };
        assert!(with_h3.advertises_h3());

        let without_h3 = HttpsRecord {
            priority: 1,
            target: DnsName::root(),
            params: vec![SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h2"]),
            }],
        };
        assert!(!without_h3.advertises_h3());

        // h3 variants like h3-29 also count — Chromium/Firefox
        // negotiate against them.
        let with_h3_variant = HttpsRecord {
            priority: 1,
            target: DnsName::root(),
            params: vec![SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h2", "h3-29"]),
            }],
        };
        assert!(with_h3_variant.advertises_h3());
    }

    #[test]
    fn https_record_round_trips_with_ech() {
        let ech_bytes = Bytes::from_static(b"\x00\x40opaque-ech-config");
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x2222,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("claude.ai").unwrap(),
                class: DnsClass::In,
                ttl: 300,
                data: DnsRecordData::Https(HttpsRecord {
                    priority: 1,
                    target: DnsName::root(),
                    params: vec![
                        SvcParam {
                            key: SvcParamKey::Alpn,
                            value: alpn_value(&["h2", "h3"]),
                        },
                        SvcParam {
                            key: SvcParamKey::Ech,
                            value: SvcParamValue::Ech(ech_bytes.clone()),
                        },
                    ],
                }),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
        let DnsRecordData::Https(rr) = &decoded.answers[0].data else {
            panic!("expected HTTPS rdata");
        };
        assert!(rr.has_ech());
        assert!(rr.advertises_h3());
    }

    #[test]
    fn https_record_preserves_unknown_svc_params_verbatim() {
        // SvcParam key 99 (made up) — must round-trip its bytes
        // unchanged. This is the contract that prevents us from
        // dropping unfamiliar SvcParams while editing the ones
        // we recognize.
        let opaque = Bytes::from_static(b"unknown-param-value");
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x3333,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("svcb.example").unwrap(),
                class: DnsClass::In,
                ttl: 60,
                data: DnsRecordData::Https(HttpsRecord {
                    priority: 1,
                    target: DnsName::root(),
                    params: vec![SvcParam {
                        key: SvcParamKey::Other(99),
                        value: SvcParamValue::Unknown(opaque.clone()),
                    }],
                }),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    #[test]
    fn https_record_with_no_params_round_trips() {
        // Bare HTTPS RR with priority + target only — the
        // AliasMode form.
        let original = DnsMessage {
            header: DnsHeader {
                id: 0x4444,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("alias.example").unwrap(),
                class: DnsClass::In,
                ttl: 30,
                data: DnsRecordData::Https(HttpsRecord {
                    priority: 0,
                    target: DnsName::from_ascii("svc.example").unwrap(),
                    params: Vec::new(),
                }),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let encoded = original.encode().expect("encode succeeds");
        let decoded = DnsMessage::decode(&encoded).expect("decode succeeds");
        assert_eq!(decoded, original);
    }

    // ─── Compression-pointer decoding ──────────────────────────────

    #[test]
    fn decode_resolves_name_compression_pointer() {
        // Hand-built response with compression: the answer's NAME
        // is a pointer back to the question's NAME at offset 12.
        let mut buf: Vec<u8> = Vec::new();
        // Header (id=1, flags=response, qdcount=1, ancount=1)
        buf.extend_from_slice(&[
            0x00, 0x01, 0x81, 0x80, // ID + flags
            0x00, 0x01, 0x00, 0x01, // QD=1, AN=1
            0x00, 0x00, 0x00, 0x00, // NS=0, AR=0
        ]);
        // Question: example.com, type A, class IN.
        buf.extend_from_slice(b"\x07example\x03com\x00");
        buf.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        buf.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        // Answer: NAME = pointer to offset 12 (the question name),
        // TYPE A, CLASS IN, TTL=60, RDLEN=4, RDATA=1.2.3.4.
        buf.extend_from_slice(&[0xc0, 0x0c]); // pointer to byte 12
        buf.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        buf.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        buf.extend_from_slice(&60u32.to_be_bytes()); // TTL
        buf.extend_from_slice(&4u16.to_be_bytes()); // RDLEN
        buf.extend_from_slice(&[1, 2, 3, 4]);

        let msg = DnsMessage::decode(&buf).expect("compressed name decodes");
        assert_eq!(msg.questions.len(), 1);
        assert_eq!(msg.answers.len(), 1);
        assert_eq!(msg.questions[0].name.to_ascii(), "example.com");
        assert_eq!(msg.answers[0].name.to_ascii(), "example.com");
        assert!(matches!(msg.answers[0].data, DnsRecordData::A(_),));
    }

    #[test]
    fn decode_rejects_pointer_loop() {
        // Pointer at offset 12 points to itself.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&[
            0x00, 0x01, 0x81, 0x80, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]);
        buf.extend_from_slice(&[0xc0, 0x0c]); // self-pointer
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        let result = DnsMessage::decode(&buf);
        assert!(matches!(result, Err(WireError::PointerLoop)));
    }

    // ─── Truncation and validation ─────────────────────────────────

    #[test]
    fn decode_rejects_truncated_header() {
        let buf = vec![0u8; 8]; // less than 12 bytes
        let err = DnsMessage::decode(&buf).expect_err("must error");
        assert!(matches!(err, WireError::Truncated { .. }));
    }

    #[test]
    fn decode_rejects_truncated_rdata() {
        // Header claims 1 answer, but rdata is short.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&[
            0x00, 0x01, 0x81, 0x80, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ]);
        buf.extend_from_slice(b"\x07example\x03com\x00");
        buf.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
        buf.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
        buf.extend_from_slice(&60u32.to_be_bytes()); // TTL
        buf.extend_from_slice(&4u16.to_be_bytes()); // RDLEN says 4
        buf.extend_from_slice(&[1, 2]); // only 2 bytes — truncated
        let err = DnsMessage::decode(&buf).expect_err("must error");
        assert!(matches!(err, WireError::Truncated { .. }));
    }

    #[test]
    fn name_from_ascii_rejects_oversized_label() {
        let long = "a".repeat(64);
        let err = DnsName::from_ascii(&long).expect_err("must error");
        assert!(matches!(err, WireError::InvalidName { .. }));
    }

    #[test]
    fn name_from_ascii_accepts_root() {
        assert_eq!(DnsName::from_ascii(".").unwrap(), DnsName::root());
        assert_eq!(DnsName::from_ascii("").unwrap(), DnsName::root());
    }
}
