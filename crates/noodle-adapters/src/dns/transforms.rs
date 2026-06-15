//! `Transform<DnsMessage>` implementations: [`StripH3`] and
//! [`StripEch`].
//!
//! These are the per-origin rewriters that implement the QUIC
//! suppression workflow per 014 §5.1 (Option A): strip the
//! `alpn=h3` `SvcParam` to prevent clients from negotiating QUIC,
//! and strip the `ech=` `SvcParam` so the TLS MITM at L1 can see
//! plaintext SNI.
//!
//! Both transforms:
//! - take ownership of one `DnsMessage` via [`TransformInstance::apply`],
//! - mutate it in place, returning `vec![mutated]` (mutation is
//!   structural — no per-record drops),
//! - emit `AuditKind::Redacted` per [015 §16] on the side channel
//!   *only when something actually changed*,
//! - leave non-HTTPS records and HTTPS records without the
//!   targeted `SvcParam` alone.
//!
//! Composition: multiple transforms register at the same
//! `(Layer::AppProtocol, Pipeline::Response)` slot with distinct
//! `order` values; the [`TransformRegistry`] iterates them in
//! ascending order. `StripH3` and `StripEch` may run in either
//! order — they touch disjoint `SvcParam` keys.
//!
//! [015 §16]: ../../../../../docs/adrs/015-layered-codec-architecture.md
//! [`TransformRegistry`]: noodle_core::layered::TransformRegistry

use noodle_core::layered::{
    AuditEvent, AuditKind, Layer, SideChannelTx, Transform, TransformAttachment, TransformInstance,
};
use smol_str::SmolStr;

use super::message::{DnsMessage, DnsRecordData, HttpsRecord, SvcParamKey, SvcParamValue};

/// Factory: [`Transform<Event = DnsMessage>`] that removes
/// `h3` (and any `h3-` prefixed variant such as `h3-29`,
/// `h3-Q050`) from the `alpn` `SvcParam` in every HTTPS RR.
///
/// If `h3` was the only protocol listed, the entire `alpn`
/// `SvcParam` is removed (an empty alpn list is invalid per
/// RFC 9460 §7.1). Other `SvcParam`s are untouched.
#[derive(Clone, Copy, Debug, Default)]
pub struct StripH3;

impl StripH3 {
    /// Public name used by [`Transform::name`] and audit events.
    pub const NAME: &'static str = "dns.strip-h3";
}

impl Transform for StripH3 {
    type Event = DnsMessage;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn open(
        &self,
        _attachment: &TransformAttachment,
    ) -> Box<dyn TransformInstance<Event = DnsMessage>> {
        Box::new(StripH3Instance)
    }
}

/// Per-flow instance. Stateless — DNS messages are independent.
#[derive(Debug, Default)]
pub struct StripH3Instance;

impl TransformInstance for StripH3Instance {
    type Event = DnsMessage;

    fn apply(&mut self, mut event: DnsMessage, side: &mut SideChannelTx<'_>) -> Vec<DnsMessage> {
        let mut changed = false;
        for record in event
            .answers
            .iter_mut()
            .chain(event.authorities.iter_mut())
            .chain(event.additionals.iter_mut())
        {
            if let DnsRecordData::Https(https) = &mut record.data
                && strip_h3_from_alpn(https)
            {
                changed = true;
            }
        }
        if changed {
            side.emit_audit(audit_redacted(
                StripH3::NAME,
                "stripped h3 from HTTPS alpn SvcParam",
            ));
        }
        vec![event]
    }
}

/// Factory: [`Transform<Event = DnsMessage>`] that removes the
/// `ech` `SvcParam` from every HTTPS RR. Forces the client to
/// send a plaintext SNI on the subsequent TLS handshake, which
/// is what the L1 MITM relies on.
#[derive(Clone, Copy, Debug, Default)]
pub struct StripEch;

impl StripEch {
    /// Public name used by [`Transform::name`] and audit events.
    pub const NAME: &'static str = "dns.strip-ech";
}

impl Transform for StripEch {
    type Event = DnsMessage;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn open(
        &self,
        _attachment: &TransformAttachment,
    ) -> Box<dyn TransformInstance<Event = DnsMessage>> {
        Box::new(StripEchInstance)
    }
}

/// Per-flow instance. Stateless.
#[derive(Debug, Default)]
pub struct StripEchInstance;

impl TransformInstance for StripEchInstance {
    type Event = DnsMessage;

    fn apply(&mut self, mut event: DnsMessage, side: &mut SideChannelTx<'_>) -> Vec<DnsMessage> {
        let mut changed = false;
        for record in event
            .answers
            .iter_mut()
            .chain(event.authorities.iter_mut())
            .chain(event.additionals.iter_mut())
        {
            if let DnsRecordData::Https(https) = &mut record.data {
                let before = https.params.len();
                https.params.retain(|p| p.key != SvcParamKey::Ech);
                if https.params.len() != before {
                    changed = true;
                }
            }
        }
        if changed {
            side.emit_audit(audit_redacted(
                StripEch::NAME,
                "stripped ech SvcParam from HTTPS RR",
            ));
        }
        vec![event]
    }
}

// ─── Internal helpers ──────────────────────────────────────────────

/// Removes `h3` and any `h3-*` variant from the alpn protocol
/// list inside `https`. If the resulting list is empty, removes
/// the entire alpn `SvcParam` (an empty list is invalid per
/// RFC 9460 §7.1).
///
/// Returns `true` when something was removed.
fn strip_h3_from_alpn(https: &mut HttpsRecord) -> bool {
    let mut changed = false;
    let mut keep_indices = Vec::with_capacity(https.params.len());
    for (idx, param) in https.params.iter_mut().enumerate() {
        if param.key != SvcParamKey::Alpn {
            keep_indices.push(idx);
            continue;
        }
        if let SvcParamValue::Alpn(protos) = &mut param.value {
            let before = protos.len();
            protos.retain(|p| !proto_is_h3(p));
            if protos.len() != before {
                changed = true;
            }
            if protos.is_empty() {
                // Don't keep an empty alpn list — it's invalid
                // per RFC 9460 §7.1.
                continue;
            }
        }
        keep_indices.push(idx);
    }
    if changed {
        // Rebuild params keeping only the indices we marked.
        let kept = std::mem::take(&mut https.params);
        https.params = kept
            .into_iter()
            .enumerate()
            .filter_map(|(idx, p)| keep_indices.contains(&idx).then_some(p))
            .collect();
    }
    changed
}

/// Match an ALPN protocol ID against `h3` (case-insensitive,
/// including `h3-*` variants like `h3-29`, `h3-Q050`).
fn proto_is_h3(proto: &SmolStr) -> bool {
    let lower = proto.to_ascii_lowercase();
    lower == "h3" || lower.starts_with("h3-")
}

/// Construct a `Redacted` audit attributable to `transform`.
fn audit_redacted(transform: &'static str, summary: &'static str) -> AuditEvent {
    AuditEvent {
        kind: AuditKind::Redacted,
        layer: Layer::AppProtocol,
        transform: SmolStr::new_static(transform),
        flow_id: 0,
        at_unix_ms: 0,
        detail: serde_json::json!({ "summary": summary }),
        correlation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::message::{
        DnsClass, DnsFlags, DnsHeader, DnsName, DnsRecord, DnsRecordType, HttpsRecord, SvcParam,
    };
    use bytes::Bytes;
    use http::{HeaderMap, Method};
    use noodle_core::layered::{
        CodecProbe, Layer, Pipeline, SideChannelTx, SideEffect, TransformAttachment,
        TransformRegistry,
    };

    // ─── Test fixtures ─────────────────────────────────────────────

    fn alpn_value(protos: &[&str]) -> SvcParamValue {
        SvcParamValue::Alpn(protos.iter().copied().map(SmolStr::new).collect())
    }

    fn https_with_params(params: Vec<SvcParam>) -> DnsRecord {
        DnsRecord {
            name: DnsName::from_ascii("claude.ai").unwrap(),
            class: DnsClass::In,
            ttl: 60,
            data: DnsRecordData::Https(HttpsRecord {
                priority: 1,
                target: DnsName::root(),
                params,
            }),
        }
    }

    /// Build a response carrying the given HTTPS-RR params.
    fn response_with_https(params: Vec<SvcParam>) -> DnsMessage {
        DnsMessage {
            header: DnsHeader {
                id: 0x1234,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![https_with_params(params)],
            authorities: Vec::new(),
            additionals: Vec::new(),
        }
    }

    /// Open an instance, drive it once, return the output + side
    /// effects.
    fn drive<T: Transform<Event = DnsMessage>>(
        factory: &T,
        event: DnsMessage,
    ) -> (Vec<DnsMessage>, Vec<SideEffect>) {
        let attachment = TransformAttachment::new(Layer::AppProtocol, Pipeline::Response, 0);
        let mut instance = factory.open(&attachment);
        let mut effects = Vec::new();
        let mut side = SideChannelTx::new(&mut effects, 0, 0);
        let out = instance.apply(event, &mut side);
        (out, effects)
    }

    fn https_rr(msg: &DnsMessage) -> &HttpsRecord {
        let DnsRecordData::Https(rr) = &msg.answers[0].data else {
            panic!("expected HTTPS rdata in answers[0]");
        };
        rr
    }

    fn count_audits(effects: &[SideEffect], name: &str) -> usize {
        effects
            .iter()
            .filter(|e| match e {
                SideEffect::Audit(a) => a.kind == AuditKind::Redacted && a.transform == name,
                _ => false,
            })
            .count()
    }

    // ─── StripH3 ───────────────────────────────────────────────────

    #[test]
    fn strip_h3_removes_h3_from_alpn_list_keeping_h2() {
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h3", "h2"]),
        }]);
        let (out, effects) = drive(&StripH3, event);
        assert_eq!(out.len(), 1);
        let rr = https_rr(&out[0]);
        let alpn_param = &rr.params[0];
        let SvcParamValue::Alpn(protos) = &alpn_param.value else {
            panic!("expected Alpn value");
        };
        assert_eq!(protos, &[SmolStr::new("h2")]);
        assert_eq!(count_audits(&effects, StripH3::NAME), 1);
    }

    #[test]
    fn strip_h3_removes_h3_variants() {
        // RFC 9460 allows h3-29, h3-Q050, etc. as concrete
        // ALPN tokens negotiated by Chromium / Firefox during
        // QUIC version evolution. All of them count as h3
        // discovery hints and must be stripped.
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h2", "h3-29", "h3-Q050"]),
        }]);
        let (out, _) = drive(&StripH3, event);
        let rr = https_rr(&out[0]);
        let SvcParamValue::Alpn(protos) = &rr.params[0].value else {
            panic!("expected Alpn value");
        };
        assert_eq!(protos, &[SmolStr::new("h2")]);
    }

    #[test]
    fn strip_h3_removes_entire_alpn_param_when_h3_was_only_entry() {
        // Empty alpn list is invalid per RFC 9460 §7.1. The
        // transform removes the param entirely rather than
        // leaving it empty.
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h3"]),
        }]);
        let (out, effects) = drive(&StripH3, event);
        let rr = https_rr(&out[0]);
        assert!(rr.params.is_empty(), "alpn SvcParam removed entirely");
        assert_eq!(count_audits(&effects, StripH3::NAME), 1);
    }

    #[test]
    fn strip_h3_preserves_other_svc_params() {
        // Other params (port, ipv4hint, ech, anything unknown)
        // must survive the rewrite — the transform is scoped to
        // the alpn key only.
        let opaque = Bytes::from_static(b"opaque-ech-config");
        let event = response_with_https(vec![
            SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h3", "h2"]),
            },
            SvcParam {
                key: SvcParamKey::Ech,
                value: SvcParamValue::Ech(opaque.clone()),
            },
            SvcParam {
                key: SvcParamKey::Other(99),
                value: SvcParamValue::Unknown(Bytes::from_static(b"keep-me")),
            },
        ]);
        let (out, _) = drive(&StripH3, event);
        let rr = https_rr(&out[0]);
        assert_eq!(
            rr.params.len(),
            3,
            "ech and unknown survive; alpn rewritten"
        );
        let alpn_param = rr
            .params
            .iter()
            .find(|p| p.key == SvcParamKey::Alpn)
            .expect("alpn still present (now [h2])");
        let SvcParamValue::Alpn(protos) = &alpn_param.value else {
            panic!("alpn value");
        };
        assert_eq!(protos, &[SmolStr::new("h2")]);
        let ech_param = rr
            .params
            .iter()
            .find(|p| p.key == SvcParamKey::Ech)
            .expect("ech survived");
        assert!(matches!(&ech_param.value, SvcParamValue::Ech(_)));
    }

    #[test]
    fn strip_h3_no_change_no_audit() {
        // 015 §16 C-1 contract for transforms: successful
        // pass-through (zero changes) does NOT emit an audit.
        // Only changes are auditable events.
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h2"]),
        }]);
        let (_out, effects) = drive(&StripH3, event);
        assert_eq!(
            count_audits(&effects, StripH3::NAME),
            0,
            "no change → no Redacted audit",
        );
    }

    #[test]
    fn strip_h3_leaves_non_https_records_alone() {
        let event = DnsMessage {
            header: DnsHeader {
                id: 0x42,
                flags: DnsFlags(0x8180),
            },
            questions: vec![],
            answers: vec![DnsRecord {
                name: DnsName::from_ascii("example.com").unwrap(),
                class: DnsClass::In,
                ttl: 300,
                data: DnsRecordData::A(std::net::Ipv4Addr::new(1, 2, 3, 4)),
            }],
            authorities: Vec::new(),
            additionals: Vec::new(),
        };
        let original_clone = event.clone();
        let (out, effects) = drive(&StripH3, event);
        assert_eq!(out[0], original_clone);
        assert_eq!(count_audits(&effects, StripH3::NAME), 0);
    }

    // ─── StripEch ──────────────────────────────────────────────────

    #[test]
    fn strip_ech_removes_ech_param() {
        let ech_bytes = Bytes::from_static(b"\x00\x40opaque");
        let event = response_with_https(vec![
            SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h2", "h3"]),
            },
            SvcParam {
                key: SvcParamKey::Ech,
                value: SvcParamValue::Ech(ech_bytes),
            },
        ]);
        let (out, effects) = drive(&StripEch, event);
        let rr = https_rr(&out[0]);
        assert_eq!(rr.params.len(), 1, "ech removed; alpn survives");
        assert_eq!(rr.params[0].key, SvcParamKey::Alpn);
        assert_eq!(count_audits(&effects, StripEch::NAME), 1);
    }

    #[test]
    fn strip_ech_no_change_no_audit() {
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h2", "h3"]),
        }]);
        let (_out, effects) = drive(&StripEch, event);
        assert_eq!(count_audits(&effects, StripEch::NAME), 0);
    }

    #[test]
    fn strip_ech_leaves_alpn_alone() {
        // Belt-and-suspenders: the StripEch transform must not
        // touch alpn even if alpn carries h3. h3-stripping is
        // StripH3's job; the two compose by registration order.
        let event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h2", "h3"]),
        }]);
        let (out, _) = drive(&StripEch, event);
        let rr = https_rr(&out[0]);
        let SvcParamValue::Alpn(protos) = &rr.params[0].value else {
            panic!("alpn value");
        };
        assert_eq!(protos, &[SmolStr::new("h2"), SmolStr::new("h3")]);
    }

    // ─── Composition via TransformRegistry ─────────────────────────

    #[test]
    fn strip_h3_and_strip_ech_compose_via_registry() {
        // Register both transforms with StripH3 ordered before
        // StripEch; drive a Cloudflare-shaped HTTPS RR through
        // and confirm both rewrites apply. Proves 026.e
        // TransformRegistry + 027.c transforms interlock — the
        // architecture composes for the real h3-suppression
        // workflow.
        let registry = TransformRegistry::<DnsMessage>::builder()
            .with_transform(
                StripH3,
                TransformAttachment::new(Layer::AppProtocol, Pipeline::Response, 10),
            )
            .with_transform(
                StripEch,
                TransformAttachment::new(Layer::AppProtocol, Pipeline::Response, 20),
            )
            .build();

        let headers = HeaderMap::new();
        let method = Method::POST;
        let probe = CodecProbe {
            host: "claude.ai",
            path: "/",
            method: &method,
            request_headers: &headers,
            response_status: None,
            response_content_type: Some("application/dns-message"),
        };
        let selected = registry.select(Layer::AppProtocol, Pipeline::Response, &probe);
        assert_eq!(selected.len(), 2, "both transforms selected");
        assert_eq!(selected[0].0.name(), StripH3::NAME, "StripH3 first");
        assert_eq!(selected[1].0.name(), StripEch::NAME, "StripEch second");

        // Open instances and drive a Cloudflare-shaped event
        // through the pair in registry order.
        let mut event = response_with_https(vec![
            SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h3", "h2"]),
            },
            SvcParam {
                key: SvcParamKey::Ech,
                value: SvcParamValue::Ech(Bytes::from_static(b"\x00\x40opq")),
            },
        ]);
        let mut effects = Vec::new();
        for (transform, attachment) in &selected {
            let mut inst = transform.open(attachment);
            let mut side = SideChannelTx::new(&mut effects, 0, 0);
            let mut out = inst.apply(event, &mut side);
            assert_eq!(out.len(), 1, "each transform passes the message through");
            event = out.pop().unwrap();
        }

        let rr = https_rr(&event);
        let SvcParamValue::Alpn(protos) = &rr.params[0].value else {
            panic!("alpn expected");
        };
        assert_eq!(protos, &[SmolStr::new("h2")], "h3 stripped");
        assert!(
            !rr.params.iter().any(|p| p.key == SvcParamKey::Ech),
            "ech stripped",
        );
        assert_eq!(count_audits(&effects, StripH3::NAME), 1);
        assert_eq!(count_audits(&effects, StripEch::NAME), 1);
    }

    // ─── Round trip: stripped message survives wire serialization ─

    #[test]
    fn stripped_message_survives_wire_round_trip() {
        // Validate the full pipeline:
        //   bytes → DnsWireCodec.decode → StripH3 → StripEch
        //   → DnsWireCodec.encode → bytes
        // The stripped message must round-trip cleanly through
        // the wire codec — no formatting drift from the rewrite.
        let original = response_with_https(vec![
            SvcParam {
                key: SvcParamKey::Alpn,
                value: alpn_value(&["h3", "h2"]),
            },
            SvcParam {
                key: SvcParamKey::Ech,
                value: SvcParamValue::Ech(Bytes::from_static(b"\x00\x40z")),
            },
        ]);

        let (out1, _) = drive(&StripH3, original);
        let (out2, _) = drive(&StripEch, out1.into_iter().next().unwrap());
        let stripped = out2.into_iter().next().unwrap();

        let wire = stripped.encode().expect("encode succeeds");
        let reparsed = DnsMessage::decode(&wire).expect("decode succeeds");
        assert_eq!(
            reparsed, stripped,
            "wire round-trip preserves stripped form"
        );

        // And verify the stripped form is the expected shape: no
        // h3 in alpn, no ech anywhere.
        let rr = https_rr(&reparsed);
        let SvcParamValue::Alpn(protos) = &rr.params[0].value else {
            panic!("alpn expected");
        };
        assert!(!protos.iter().any(super::proto_is_h3));
        assert!(!rr.params.iter().any(|p| p.key == SvcParamKey::Ech));
    }

    // Compile-time bound assertions.
    #[allow(dead_code)]
    fn _assert_bounds() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        fn assert_send<T: Send + 'static>() {}
        assert_send_sync::<StripH3>();
        assert_send_sync::<StripEch>();
        assert_send::<StripH3Instance>();
        assert_send::<StripEchInstance>();
    }

    // Extra check: confirm question-section RRs are left
    // untouched (questions don't have rdata, so the iter chain
    // doesn't visit them, but pin the property).
    #[test]
    fn questions_section_is_not_visited() {
        let mut event = response_with_https(vec![SvcParam {
            key: SvcParamKey::Alpn,
            value: alpn_value(&["h3", "h2"]),
        }]);
        event.questions.push(crate::dns::message::DnsQuestion {
            name: DnsName::from_ascii("claude.ai").unwrap(),
            qtype: DnsRecordType::Https,
            qclass: DnsClass::In,
        });
        let q_before = event.questions.clone();
        let (out, _) = drive(&StripH3, event);
        assert_eq!(out[0].questions, q_before);
    }
}
