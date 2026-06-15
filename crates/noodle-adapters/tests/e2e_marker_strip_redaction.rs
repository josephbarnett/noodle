//! ADR 017 §2.4 — end-to-end redaction proof.
//!
//! The security contract this pins: a `<noodle:NAME>` marker
//! present in an upstream Anthropic text delta must be **absent
//! from the bytes a client would receive**, and its value must be
//! captured as an `Artifact` on the side channel.
//!
//! It asserts on the **re-serialised client-visible wire bytes**,
//! NOT on `Token.text` — a test that checked `Token.text` would
//! pass while the original, unredacted bytes still reached the
//! client (the exact bug class ADR 017 closes; see §6).
//!
//! ## Why this composes the codecs directly instead of driving
//! `InspectionEngine`
//!
//! `InspectionEngine::ResponseFlow` is decode + transform only —
//! it yields `FlowOutput { events, side_effects }` and exposes no
//! response-encode-to-bytes path. Re-serialising the transformed
//! stream back onto the client body is not yet wired into the
//! engine (backlog item 4: side-effect sink + response
//! substitution; item 12: flip layered → default). So this test
//! composes the **real** L4/L5 codec instances and the real
//! transform exactly as the engine will once that path lands —
//! proving every component on the redaction path is correct and
//! byte-faithful end to end. The remaining work is wiring, not
//! correctness of these parts. (Recorded as the ADR 017 §7
//! addendum.)
//!
//! ## fail-before / pass-after, in one test
//!
//! `wire_without_strip()` runs the identical decode→encode round
//! trip with NO transform: the marker survives to the wire (this
//! is the "before" — proving the assertion is meaningful and the
//! pipeline replays faithfully when nothing mutates). `wire_with_
//! strip()` inserts `MarkerStripTransform`: the marker is gone and
//! the value is captured (the "after").

use bytes::Bytes;
use noodle_adapters::provider::anthropic_layered::LayeredAnthropicCodec;
use noodle_adapters::sse::SseFrameCodec;
use noodle_adapters::transform::marker_strip::MarkerStripTransform;
use noodle_core::event::NormalizedEvent;
use noodle_core::layered::{
    Codec, Layer, Pipeline, SideChannelTx, SideEffect, Transform, TransformAttachment,
    TransformInstance,
};

/// A realistic Anthropic SSE stream: a `message_start`, a single
/// `content_block_delta` whose text embeds a `work_type` marker
/// surrounded by visible prose, then `message_stop`.
const STREAM: &[&[u8]] = &[
    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_x\",\"role\":\"assistant\"}}\n\n",
    b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Here is the plan. <noodle:work_type>refactor</noodle:work_type>Proceeding now.\"}}\n\n",
    b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

const MARKER_NAME: &str = "work_type";
const MARKER_VALUE: &str = "refactor";

/// Decode the stream through real L4+L5 codecs, optionally run
/// `MarkerStripTransform` at L5, then re-encode L5→L4 back to wire
/// bytes. Returns `(client_visible_bytes, side_effects)`.
fn round_trip(strip: bool) -> (Vec<u8>, Vec<SideEffect>) {
    let mut l4_dec = SseFrameCodec.open();
    let mut l5_dec = LayeredAnthropicCodec.open();
    let mut l5_enc = LayeredAnthropicCodec.open();
    let mut l4_enc = SseFrameCodec.open();

    let mut xform = strip.then(|| {
        MarkerStripTransform::new([MARKER_NAME]).open(&TransformAttachment::new(
            Layer::VendorSemantics,
            Pipeline::Response,
            0,
        ))
    });

    let mut side_buf: Vec<SideEffect> = Vec::new();
    let mut wire: Vec<u8> = Vec::new();

    let mut pump =
        |events: Vec<NormalizedEvent>,
         side_buf: &mut Vec<SideEffect>,
         wire: &mut Vec<u8>,
         xform: &mut Option<Box<dyn TransformInstance<Event = NormalizedEvent>>>| {
            let staged: Vec<NormalizedEvent> = match xform {
                Some(x) => {
                    let mut out = Vec::new();
                    for ev in events {
                        let mut side = SideChannelTx::new(side_buf, 0, 0);
                        out.extend(x.apply(ev, &mut side));
                    }
                    out
                }
                None => events,
            };
            for ev in staged {
                for frame in l5_enc.encode(ev) {
                    for b in l4_enc.encode(frame) {
                        wire.extend_from_slice(&b);
                    }
                }
            }
        };

    for chunk in STREAM {
        let frames = l4_dec.decode(Bytes::from_static(chunk));
        let mut events = Vec::new();
        for f in frames {
            events.extend(l5_dec.decode(f));
        }
        pump(events, &mut side_buf, &mut wire, &mut xform);
    }

    // End-of-stream drain: codec + transform flush.
    let mut tail = Vec::new();
    for f in l4_dec.flush() {
        tail.extend(l5_dec.decode(f));
    }
    tail.extend(l5_dec.flush());
    pump(tail, &mut side_buf, &mut wire, &mut xform);
    if let Some(x) = &mut xform {
        let mut side = SideChannelTx::new(&mut side_buf, 0, 0);
        let flushed = x.flush(&mut side);
        for ev in flushed {
            for frame in l5_enc.encode(ev) {
                for b in l4_enc.encode(frame) {
                    wire.extend_from_slice(&b);
                }
            }
        }
    }

    (wire, side_buf)
}

/// fail-before: with NO transform the decode→encode round trip
/// replays faithfully, so the marker is STILL on the wire. This
/// proves the assertion below is meaningful (the bytes really do
/// carry the marker unless something strips it) and that the
/// codec round trip is byte-honest.
#[test]
fn without_strip_marker_survives_to_the_wire() {
    let (wire, side) = round_trip(false);
    let s = String::from_utf8(wire).expect("utf8 wire");
    assert!(
        s.contains("<noodle:work_type>"),
        "control: unstripped stream must still carry the marker",
    );
    assert!(s.contains(MARKER_VALUE));
    assert!(
        side.is_empty(),
        "no transform ⇒ no artifacts/audits: {side:?}",
    );
}

/// pass-after: with `MarkerStripTransform` the marker is ABSENT
/// from the client-visible bytes and its value is captured as an
/// `Artifact`. This is the redaction-reaches-the-client contract.
#[test]
fn with_strip_marker_absent_from_client_bytes_and_captured() {
    let (wire, side) = round_trip(true);
    let s = String::from_utf8(wire).expect("utf8 wire");

    // The security assertion: nothing about the marker reaches
    // the wire — not the tags, not the value.
    assert!(
        !s.contains("noodle:"),
        "marker tag leaked to client bytes: {s:?}",
    );
    assert!(
        !s.contains(MARKER_VALUE),
        "redacted value leaked to client bytes: {s:?}",
    );
    // The surrounding visible prose is preserved.
    assert!(s.contains("Here is the plan."), "prose dropped: {s:?}");
    assert!(s.contains("Proceeding now."), "prose dropped: {s:?}");

    // The captured value is on the side channel as an Artifact.
    let artifact = side
        .iter()
        .find_map(|e| match e {
            SideEffect::Artifact(a) => Some(a),
            _ => None,
        })
        .expect("work_type artifact must be captured");
    assert_eq!(artifact.name.as_str(), MARKER_NAME);
    assert_eq!(artifact.value.as_str(), MARKER_VALUE);
}
