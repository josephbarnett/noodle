//! [`DnsWireCodec`] — L2 DNS branch of the layered codec stack
//! (015 §2). Wraps [`super::DnsMessage`]'s wire parse / serialize
//! into the [`Codec`] trait so the `Transform<DnsMessage>` set
//! (027.c) and the engine can dispatch DNS flows through the same
//! shape as HTTP flows at L4 / L5.
//!
//! # Error contract — 015 §16 known gap
//!
//! 015 §16.1 prescribes that `decode` / `encode` / `flush`
//! emit `AuditEvent::Errored` on the side channel and return
//! `Vec::new()` on failure. The current [`CodecInstance`] trait
//! signature (shipped in story 026.a) does **not** carry a
//! [`SideChannelTx`][noodle_core::layered::SideChannelTx] — the
//! channel only lives on [`TransformInstance`][noodle_core::layered::TransformInstance].
//!
//! Until that trait gap is closed, this impl:
//! - logs failures via `tracing::warn!` with structured fields so
//!   operational visibility isn't lost,
//! - records the last error on the instance and exposes it via
//!   [`DnsWireCodecInstance::last_error`] so the engine — and
//!   tests — can read it after a call,
//! - returns `Vec::new()` per the empty-on-error contract.
//!
//! Resolution path: a follow-up adds `&mut SideChannelTx<'_>` to
//! the `CodecInstance` method signatures; this codec moves the
//! `tracing::warn!` + `last_error` plumbing into a `side.emit_audit(…)`
//! call at the same time. Tests asserting `last_error` will be
//! rewritten to inspect the side channel instead.

use bytes::Bytes;
use noodle_core::layered::{Codec, CodecInstance, CodecProbe};

use super::message::{DnsMessage, WireError};

/// Factory: stateless, cheap to clone, registered once per
/// engine.
#[derive(Clone, Copy, Debug, Default)]
pub struct DnsWireCodec;

impl DnsWireCodec {
    /// Public stable name returned by [`Codec::name`].
    pub const NAME: &'static str = "dns-wire";
}

impl Codec for DnsWireCodec {
    type Input = Bytes;
    type Output = DnsMessage;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    /// DNS lives on its own L2 sibling branch (015 §2). Flow
    /// routing to this codec is the engine's job — there is no
    /// `CodecProbe` field that distinguishes "this is a DNS
    /// flow" from "this is an HTTP flow," and the probe is
    /// HTTP-shaped (host / path / method / headers). This codec
    /// therefore accepts any probe and relies on the engine to
    /// invoke it only on DNS flows. (Story 026.e's
    /// `CodecRegistry` is per-layer-typed; a registry of
    /// `Codec<Bytes, DnsMessage>` is the type-level signal that
    /// the bytes flowing in are DNS datagrams.)
    fn matches(&self, _probe: &CodecProbe<'_>) -> bool {
        true
    }

    fn open(&self) -> Box<dyn CodecInstance<Input = Bytes, Output = DnsMessage>> {
        Box::new(DnsWireCodecInstance::default())
    }
}

/// Per-flow instance. Stateless aside from `last_error`; DNS is
/// message-oriented (each datagram parses independently), so
/// there is no cross-call state to maintain.
#[derive(Debug, Default)]
pub struct DnsWireCodecInstance {
    last_error: Option<WireError>,
}

impl DnsWireCodecInstance {
    /// The most recent error observed by [`decode`] / [`encode`].
    /// Cleared on the next successful call. Used by the engine
    /// and tests to inspect the failure path until the
    /// `SideChannelTx` plumbing lands on `CodecInstance` (see
    /// module-level docs).
    ///
    /// [`decode`]: CodecInstance::decode
    /// [`encode`]: CodecInstance::encode
    #[must_use]
    pub fn last_error(&self) -> Option<&WireError> {
        self.last_error.as_ref()
    }
}

impl CodecInstance for DnsWireCodecInstance {
    type Input = Bytes;
    type Output = DnsMessage;

    fn decode(&mut self, item: Bytes) -> Vec<DnsMessage> {
        match DnsMessage::decode(&item) {
            Ok(msg) => {
                self.last_error = None;
                vec![msg]
            }
            Err(err) => {
                tracing::warn!(
                    codec = DnsWireCodec::NAME,
                    input_bytes = item.len(),
                    error = %err,
                    "DNS decode failed; returning empty per 015 §16 contract",
                );
                self.last_error = Some(err);
                Vec::new()
            }
        }
    }

    fn encode(&mut self, item: DnsMessage) -> Vec<Bytes> {
        match item.encode() {
            Ok(bytes) => {
                self.last_error = None;
                vec![Bytes::from(bytes)]
            }
            Err(err) => {
                tracing::warn!(
                    codec = DnsWireCodec::NAME,
                    error = %err,
                    "DNS encode failed; returning empty per 015 §16 contract",
                );
                self.last_error = Some(err);
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::message::{
        DnsClass, DnsFlags, DnsHeader, DnsName, DnsQuestion, DnsRecord, DnsRecordData,
        DnsRecordType, HttpsRecord, SvcParam, SvcParamKey, SvcParamValue,
    };
    use bytes::Bytes;
    use http::{HeaderMap, Method};
    use noodle_core::layered::{ChannelCapacity, CodecProbe, CodecRegistry};
    use smol_str::SmolStr;

    fn null_probe<'a>(method: &'a Method, headers: &'a HeaderMap) -> CodecProbe<'a> {
        CodecProbe {
            host: "irrelevant-for-dns",
            path: "/",
            method,
            request_headers: headers,
            response_status: None,
            response_content_type: None,
        }
    }

    /// Build a realistic DNS response carrying an HTTPS RR with
    /// `alpn=h2,h3` and an opaque `ech` `SvcParam` — the
    /// Cloudflare-shaped record `claude.ai` actually returns.
    fn sample_claude_ai_response() -> DnsMessage {
        DnsMessage {
            header: DnsHeader {
                id: 0xc0de,
                flags: DnsFlags(0x8180),
            },
            questions: vec![DnsQuestion {
                name: DnsName::from_ascii("claude.ai").unwrap(),
                qtype: DnsRecordType::Https,
                qclass: DnsClass::In,
            }],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("claude.ai").unwrap(),
                class: DnsClass::In,
                ttl: 60,
                data: DnsRecordData::Https(HttpsRecord {
                    priority: 1,
                    target: DnsName::root(),
                    params: vec![
                        SvcParam {
                            key: SvcParamKey::Alpn,
                            value: SvcParamValue::Alpn(vec![
                                SmolStr::new_static("h3"),
                                SmolStr::new_static("h2"),
                            ]),
                        },
                        SvcParam {
                            key: SvcParamKey::Ech,
                            value: SvcParamValue::Ech(Bytes::from_static(
                                b"\x00\x40opaque-ech-config",
                            )),
                        },
                    ],
                }),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        }
    }

    // ─── Factory / matches ─────────────────────────────────────────

    #[test]
    fn factory_name_is_stable() {
        assert_eq!(DnsWireCodec.name(), "dns-wire");
    }

    #[test]
    fn matches_accepts_any_probe() {
        // DNS routing is the engine's job (the per-layer registry
        // is type-keyed); this codec accepts any probe at the
        // selection step.
        let headers = HeaderMap::new();
        let method = Method::POST;
        let probe = null_probe(&method, &headers);
        assert!(DnsWireCodec.matches(&probe));
    }

    // ─── decode ────────────────────────────────────────────────────

    #[test]
    fn decode_parses_a_valid_dns_message() {
        let original = sample_claude_ai_response();
        let wire = Bytes::from(original.encode().expect("encode succeeds"));

        let codec = DnsWireCodec;
        let mut instance = codec.open();
        let out = instance.decode(wire);
        assert_eq!(out.len(), 1, "one input message → one output message");
        assert_eq!(out[0], original, "decoded message equals original");
    }

    #[test]
    fn decode_returns_empty_on_truncated_input() {
        // Only 8 bytes — header (12 bytes) is truncated. The
        // wire parser returns Err, the codec returns empty per
        // 015 §16 C-1.
        let truncated = Bytes::from_static(&[0u8; 8]);

        // Concrete instance so we can inspect `last_error`
        // without going through the trait object (the §16-gap
        // workaround). Trait-object dispatch is exercised by
        // the other tests via `codec.open()`.
        let mut instance = DnsWireCodecInstance::default();
        let out = instance.decode(truncated);
        assert!(out.is_empty(), "empty-on-error contract (015 §16 C-1)");
        assert!(
            instance.last_error().is_some(),
            "last_error must record the failure (known-gap workaround)",
        );
        assert!(matches!(
            instance.last_error(),
            Some(WireError::Truncated { .. }),
        ));
    }

    #[test]
    fn trait_object_dispatch_returns_empty_on_failure() {
        // Verifies the empty-Vec part of the contract goes
        // through the trait object correctly — we can't inspect
        // last_error this way (§16 gap), but we can confirm the
        // dispatch path lands the same Vec<DnsMessage> shape.
        let codec = DnsWireCodec;
        let mut instance = codec.open();
        let out = instance.decode(Bytes::from_static(&[0u8; 8]));
        assert!(out.is_empty());
    }

    #[test]
    fn decode_clears_last_error_on_subsequent_success() {
        // 015 §16 C-1 negative half: a successful call after a
        // failure must clear the error state. Otherwise the
        // engine cannot distinguish "stale error from the past"
        // from "current call failed."
        let codec = DnsWireCodec;
        let mut instance = DnsWireCodecInstance::default();

        // First: failure.
        let _ = instance.decode(Bytes::from_static(&[0u8; 4]));
        assert!(instance.last_error().is_some());

        // Then: success.
        let original = sample_claude_ai_response();
        let wire = Bytes::from(original.encode().unwrap());
        let out = instance.decode(wire);
        assert_eq!(out.len(), 1);
        assert!(
            instance.last_error().is_none(),
            "success must clear last_error",
        );

        // Confirm trait dispatch goes through the same path.
        let _ = codec.open();
    }

    // ─── encode ────────────────────────────────────────────────────

    #[test]
    fn encode_serialises_a_valid_message() {
        let original = sample_claude_ai_response();
        let codec = DnsWireCodec;
        let mut instance = codec.open();
        let out = instance.encode(original.clone());
        assert_eq!(out.len(), 1, "one message → one byte buffer");

        // The output bytes must re-parse to the same message —
        // the round-trip invariant (015 §2.1.1) at the codec
        // layer.
        let reparsed = DnsMessage::decode(&out[0]).expect("re-decode succeeds");
        assert_eq!(reparsed, original);
    }

    // ─── Round trip through the codec ──────────────────────────────

    #[test]
    fn codec_round_trip_preserves_bytes_for_unmutated_messages() {
        // The §2.1.1 round-trip invariant: encode(decode(bytes))
        // == bytes for any item not mutated by a transform.
        // Demonstrated end-to-end through the codec trait.
        let original = sample_claude_ai_response();
        let wire_in = Bytes::from(original.encode().unwrap());

        let codec = DnsWireCodec;
        let mut decoder = codec.open();
        let messages = decoder.decode(wire_in.clone());
        assert_eq!(messages.len(), 1);

        let mut encoder = codec.open();
        let wire_out = encoder.encode(messages[0].clone());
        assert_eq!(wire_out.len(), 1);
        assert_eq!(wire_out[0], wire_in, "byte-exact round trip");
    }

    // ─── State isolation between flows ─────────────────────────────

    #[test]
    fn instances_isolated_between_concurrent_flows() {
        // 015 §2.1.2: two flows never share state. Drive two
        // instances with different inputs; one's error state
        // must not leak into the other.
        let codec = DnsWireCodec;
        let mut a = DnsWireCodecInstance::default();
        let mut b = DnsWireCodecInstance::default();

        let _ = a.decode(Bytes::from_static(&[0u8; 4])); // A: error
        let original = sample_claude_ai_response();
        let _ = b.decode(Bytes::from(original.encode().unwrap())); // B: ok

        assert!(a.last_error().is_some(), "A retains its error");
        assert!(b.last_error().is_none(), "B unaffected by A's failure");

        let _ = codec; // codec factory is unaffected
    }

    // ─── Integration with CodecRegistry (story 026.e) ──────────────

    #[test]
    fn codec_registers_and_selects_through_codec_registry() {
        // The DnsWireCodec slots into a CodecRegistry typed on
        // (Bytes, DnsMessage) and is selectable. Proves the
        // layered architecture composes — 026.e's registry +
        // 026.a's Codec + 027.a's DnsMessage + 027.b's
        // DnsWireCodec interlock without ceremony.
        let registry = CodecRegistry::<Bytes, DnsMessage>::builder()
            .channel_capacity(ChannelCapacity::new(32))
            .with_codec(DnsWireCodec)
            .build();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.channel_capacity().get(), 32);

        let headers = HeaderMap::new();
        let method = Method::POST;
        let probe = null_probe(&method, &headers);
        let chosen = registry.select(&probe).expect("dns-wire matches");
        assert_eq!(chosen.name(), DnsWireCodec::NAME);
    }

    // Compile-time bound: `DnsWireCodec` and its instance carry
    // the `Send + Sync + 'static` / `Send + 'static` bounds
    // demanded by `Codec` and `CodecInstance`. If these slip,
    // this function fails to compile.
    #[allow(dead_code)]
    fn _assert_bounds() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        fn assert_send<T: Send + 'static>() {}
        assert_send_sync::<DnsWireCodec>();
        assert_send::<DnsWireCodecInstance>();
    }
}
