//! `MarkerStripTransform` — the [`Filter`] role on the layered
//! architecture (015 §11 step 2 / backlog item 2, ADR 017 §2.3).
//!
//! A faithful port of the legacy [`MarkerStripFilter`]: it wraps
//! the SAME load-bearing [`MarkerScanner`] FSM (reused verbatim —
//! not reimplemented) and restates it as a
//! `Transform<Event = NormalizedEvent>` so it runs on the L5
//! layered pipeline.
//!
//! On a `Token` it strips every `<noodle:NAME>VALUE</noodle:NAME>`
//! span whose `NAME` is in the allow-list, emits the captured
//! value as an [`Artifact`] plus an [`AuditKind::Redacted`] audit
//! per [015 §16], and re-emits the cleaned text as a
//! [`EventSource::Mutated`] `Token`.
//!
//! ## Why every re-emitted `Token` is `Mutated` (ADR 017)
//!
//! When the scanner is enabled it is *authoritative* over the
//! byte stream: across calls it may hold a suspect prefix, eat a
//! newline after a close marker, or strip a span. Replaying the
//! event's original upstream bytes (`EventSource::Upstream`) would
//! therefore risk leaking a marker the FSM is mid-detecting across
//! a chunk boundary. So whenever the enabled scanner re-emits
//! text, the `Token` is tagged `Mutated`, which forces the L5
//! codec to re-serialise from the cleaned `text` (ADR 017 §2).
//! A *disabled* scanner (empty allow-list) is a pure pass-through
//! and preserves the original provenance untouched.
//!
//! ## `flush()` — the never-silently-swallow contract
//!
//! Bytes held in a partial open/tag/close at end-of-stream are
//! released by `flush()` as a `Mutated` synthetic `Token`,
//! preserving the legacy guarantee that a truncated marker is
//! emitted verbatim rather than disappearing.
//!
//! Scope (ADR 017 §5 boundary): this is the *Filter* role only.
//! The `ContextEnhancer` role and the request pipeline are backlog
//! item 3; `flow_id` / timestamp stamping on side effects is
//! backlog item 4 (transforms emit the placeholder `0` values,
//! consistent with every other transform in the tree today).
//!
//! [`MarkerStripFilter`]: crate::filter::MarkerStripFilter
//! [`Filter`]: noodle_core::Filter
//! [015 §16]: ../../../../../docs/adrs/015-layered-codec-architecture.md

use noodle_core::event::{EventSource, NormalizedEvent};
use noodle_core::layered::{
    Artifact, AuditEvent, AuditKind, Hint, Layer, SideChannelTx, Transform, TransformAttachment,
    TransformInstance,
};
use noodle_core::{MarkerScanner, ScanOutput};
use smol_str::SmolStr;

/// Confidence assigned to Hints derived from a model-emitted
/// marker. Higher than UA-derived heuristic hints (0.95 in the
/// `tap_setup` UA table) because the model self-tagged the
/// content — the strongest attribution signal available short of
/// a cryptographic claim. Tied to ADR 004's max-confidence
/// resolution algorithm: when both a marker and a UA produce a
/// Hint for the same category, the marker wins.
const MARKER_HINT_CONFIDENCE: f32 = 0.99;

/// Source identifier on `Hint.source`. Lines up with the priority
/// order in `CategoryConfig::with_attribution_defaults`'s
/// `detectors: ["marker", "user_agent"]` — when two sources tie
/// on confidence, "marker" wins.
const MARKER_HINT_SOURCE: &str = "marker";

/// Factory: builds an independent [`MarkerStripInstance`] per
/// flow. Holds the tag-name allow-list; state is never shared
/// across requests.
#[derive(Clone, Debug, Default)]
pub struct MarkerStripTransform {
    tag_names: Vec<String>,
}

impl MarkerStripTransform {
    /// Public name used by [`Transform::name`], artifacts, and
    /// audit events.
    pub const NAME: &'static str = "marker-strip";

    /// Build a factory for the given tag-name allow-list.
    #[must_use]
    pub fn new<I, S>(tag_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            tag_names: tag_names
                .into_iter()
                .map(|n| n.as_ref().to_owned())
                .collect(),
        }
    }
}

impl Transform for MarkerStripTransform {
    type Event = NormalizedEvent;

    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn open(
        &self,
        _attachment: &TransformAttachment,
    ) -> Box<dyn TransformInstance<Event = NormalizedEvent>> {
        Box::new(MarkerStripInstance {
            scanner: MarkerScanner::new(&self.tag_names),
            last_index: None,
            placeholder_emitted: std::collections::HashSet::new(),
            real_output_emitted: std::collections::HashSet::new(),
        })
    }
}

/// Per-flow instance. Owns the marker FSM so a marker split
/// across SSE event boundaries (the common case) resolves
/// correctly.
pub struct MarkerStripInstance {
    scanner: MarkerScanner,
    /// Last `content_block` index we saw on a `Token` apply.
    /// Used to stamp the emitted mutated Token (and any bytes
    /// the scanner releases on `flush()`) at the correct block
    /// index — Anthropic SSE streams blocks sequentially, so the
    /// most recent block index is the right answer for the
    /// scanner's carry buffer. Without this, mutated tokens
    /// inherit the codec's fallback (index 0) and the client
    /// rejects them when the active block isn't at index 0
    /// (e.g. extended-thinking responses have the text block at
    /// index 1).
    last_index: Option<u32>,
    /// Content-block indices for which we've already emitted a
    /// single-space placeholder Token to prevent an empty content
    /// block reaching the client. Guard so we emit at most one
    /// placeholder per block.
    placeholder_emitted: std::collections::HashSet<u32>,
    /// Content-block indices where at least one non-empty Token
    /// has flowed through. Suppresses placeholder emission for
    /// blocks that already carry real content.
    real_output_emitted: std::collections::HashSet<u32>,
}

impl TransformInstance for MarkerStripInstance {
    type Event = NormalizedEvent;

    fn apply(
        &mut self,
        event: NormalizedEvent,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<NormalizedEvent> {
        // Only assistant text deltas carry markers. Every other
        // event (TurnStart/End, ToolCall, Metadata) and the
        // disabled-scanner case pass through with provenance
        // intact.
        let NormalizedEvent::Token { text, index, .. } = &event else {
            return vec![event];
        };
        if !self.scanner.enabled() {
            return vec![event];
        }

        // Remember the block index for the mutated re-emit. The
        // scanner may buffer suspect bytes across multiple
        // `apply()` calls; for a contiguous text block (which
        // Anthropic streams sequentially), the index is stable
        // within that block, so the last-seen index is correct.
        self.last_index = *index;
        let input_was_nonempty = !text.is_empty();

        let scanned = self.scanner.process(text.as_bytes());
        let result = Self::drain(scanned, *index, side);

        if !result.is_empty() {
            if let Some(idx) = *index {
                self.real_output_emitted.insert(idx);
            }
            return result;
        }

        // Empty result from non-empty input → the entire delta
        // was marker (or buffered prefix). Anthropic rejects
        // requests whose `messages` array contains a text content
        // block with an empty `text` field ("text content blocks
        // must be non-empty"), so a content block whose every
        // delta gets stripped triggers a 400 on the NEXT request
        // when the client re-submits the assistant message.
        // Emit a single-space placeholder Token the first time
        // we'd otherwise emit nothing for this block; subsequent
        // empty results in the same block silently drop. If real
        // content eventually arrives in the same block, that
        // content rides through on top of the leading space.
        if input_was_nonempty
            && let Some(idx) = *index
            && !self.real_output_emitted.contains(&idx)
            && !self.placeholder_emitted.contains(&idx)
        {
            self.placeholder_emitted.insert(idx);
            return vec![NormalizedEvent::Token {
                text: " ".to_owned(),
                index: Some(idx),
                source: EventSource::Mutated,
            }];
        }

        result
    }

    fn flush(&mut self, side: &mut SideChannelTx<'_>) -> Vec<NormalizedEvent> {
        let tail = self.scanner.flush();
        // Tail bytes (truncated marker on EOS) belong to the
        // last block we saw text on.
        Self::drain(tail, self.last_index, side)
    }
}

impl MarkerStripInstance {
    /// Turn one [`ScanOutput`] into the side effects + the
    /// (optional) cleaned `Token`. Shared by `apply` and `flush`
    /// so the streaming and end-of-stream paths are identical.
    fn drain(
        scanned: ScanOutput,
        index: Option<u32>,
        side: &mut SideChannelTx<'_>,
    ) -> Vec<NormalizedEvent> {
        for hit in &scanned.markers {
            let name = SmolStr::new(&hit.name);
            let value = SmolStr::new(String::from_utf8_lossy(&hit.value).as_ref());
            // Artifact = the captured value with full chain of
            // custody (downstream tools, viewer panels, audit
            // pipelines). The "evidence" record.
            side.emit_artifact(Artifact {
                name: name.clone(),
                value: value.clone(),
                source_layer: Layer::VendorSemantics,
                source_transform: SmolStr::new_static(MarkerStripTransform::NAME),
                // flow_id / timestamp stamped by the engine when
                // it drains the side channel — backlog item 4.
                flow_id: 0,
                captured_at_unix_ms: 0,
                // ADR 023 correlation: stamped by the engine drain
                // (`InspectionEngine::drain_to_sink`).
                correlation: None,
            });
            // Hint = the same capture, restated as input for the
            // Resolver. category = the marker's tag name (e.g.
            // "work_type", "tool"); source = "marker" so the
            // tie-break priority in `CategoryConfig` puts
            // marker-derived signals above heuristic ones (UA
            // headers etc). Without this Hint, the Artifact
            // surfaces on the sink but the Resolver never sees
            // the model's self-tag — the marker carries
            // attribution data but no attribution conclusion is
            // produced from it.
            side.emit_hint(Hint {
                category: name.clone(),
                value: value.clone(),
                confidence: MARKER_HINT_CONFIDENCE,
                source: SmolStr::new_static(MARKER_HINT_SOURCE),
                correlation: None,
            });
            // Redacted audit: §16 observability of "we changed
            // bytes the client would have seen."
            side.emit_audit(AuditEvent {
                kind: AuditKind::Redacted,
                layer: Layer::VendorSemantics,
                transform: SmolStr::new_static(MarkerStripTransform::NAME),
                flow_id: 0,
                at_unix_ms: 0,
                detail: serde_json::json!({ "marker": hit.name }),
                correlation: None,
            });
        }

        // Empty output means the scanner is holding a suspect
        // prefix or the token was entirely marker — emit no
        // `Token`. (A zero-length text delta is not a valid
        // `Token`; the held bytes release on a later call or by
        // `flush()`, preserving the never-silently-swallow
        // contract.)
        if scanned.bytes.is_empty() {
            return Vec::new();
        }
        // Marker is ASCII, surrounding content was UTF-8, so the
        // result is valid UTF-8. Lossy fallback defends against a
        // caller feeding non-UTF-8 into a text delta.
        let text = String::from_utf8(scanned.bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned());
        vec![NormalizedEvent::Token {
            text,
            // Preserve the original content-block index. The codec
            // uses it on encode to target the right block — see
            // anthropic_layered::encode (mutated_token arm). Without
            // this, the re-encode hardcodes index 0 and Claude
            // Code rejects the response when the active block was
            // at a non-zero index ("Content block is not a text
            // block").
            index,
            source: EventSource::Mutated,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodle_core::event::{ProviderChunk, RoundTripId};
    use noodle_core::layered::{Pipeline, SideEffect};

    fn instance(names: &[&str]) -> Box<dyn TransformInstance<Event = NormalizedEvent>> {
        MarkerStripTransform::new(names.iter().copied()).open(&TransformAttachment::new(
            Layer::VendorSemantics,
            Pipeline::Response,
            0,
        ))
    }

    fn token(text: &str) -> NormalizedEvent {
        token_at(text, Some(0))
    }

    fn token_at(text: &str, index: Option<u32>) -> NormalizedEvent {
        NormalizedEvent::Token {
            text: text.to_owned(),
            index,
            // Upstream input — the transform must flip it to
            // Mutated when it touches the text.
            source: ProviderChunk(bytes::Bytes::from_static(b"raw")).into(),
        }
    }

    fn text_of(ev: &NormalizedEvent) -> &str {
        match ev {
            NormalizedEvent::Token { text, .. } => text,
            _ => panic!("expected Token, got {ev:?}"),
        }
    }

    fn drive(
        inst: &mut dyn TransformInstance<Event = NormalizedEvent>,
        chunks: &[&str],
    ) -> (Vec<NormalizedEvent>, Vec<SideEffect>) {
        let mut buf = Vec::new();
        let mut events = Vec::new();
        {
            let mut side = SideChannelTx::new(&mut buf, 0, 0);
            for c in chunks {
                events.extend(inst.apply(token(c), &mut side));
            }
            events.extend(inst.flush(&mut side));
        }
        (events, buf)
    }

    #[test]
    fn no_marker_passes_text_through_but_marks_mutated() {
        let mut inst = instance(&["work_type"]);
        let (events, side) = drive(inst.as_mut(), &["plain text, no markers."]);
        assert_eq!(events.len(), 1);
        assert_eq!(text_of(&events[0]), "plain text, no markers.");
        // Enabled scanner is authoritative → Mutated (ADR 017).
        assert!(matches!(
            events[0],
            NormalizedEvent::Token {
                source: EventSource::Mutated,
                ..
            }
        ));
        assert!(side.is_empty(), "no markers ⇒ no side effects");
    }

    #[test]
    fn single_marker_stripped_text_mutated_artifact_and_audit() {
        let mut inst = instance(&["work_type"]);
        let (events, side) = drive(
            inst.as_mut(),
            &["before<noodle:work_type>build</noodle:work_type>after"],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(text_of(&events[0]), "beforeafter");
        assert!(matches!(
            events[0],
            NormalizedEvent::Token {
                source: EventSource::Mutated,
                ..
            }
        ));
        // One Artifact (the captured value) + one Hint (input to
        // the Resolver, source="marker") + one Redacted audit.
        let artifacts: Vec<_> = side
            .iter()
            .filter_map(|e| match e {
                SideEffect::Artifact(a) => Some(a),
                _ => None,
            })
            .collect();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name.as_str(), "work_type");
        assert_eq!(artifacts[0].value.as_str(), "build");

        // The Hint — load-bearing for the attribution loop.
        // Without this the Artifact lands on the sink but the
        // Resolver never sees the marker as input.
        let hints: Vec<_> = side
            .iter()
            .filter_map(|e| match e {
                SideEffect::Hint(h) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].category.as_str(), "work_type");
        assert_eq!(hints[0].value.as_str(), "build");
        assert_eq!(hints[0].source.as_str(), "marker");
        assert!(
            (hints[0].confidence - 0.99).abs() < 1e-6,
            "marker hints are the highest-confidence source (model self-tagged)",
        );

        assert!(side.iter().any(|e| matches!(
            e,
            SideEffect::Audit(a) if a.kind == AuditKind::Redacted
        )));
    }

    /// REGRESSION: when a token comes in at a non-zero
    /// content-block index (extended-thinking puts the text
    /// block at index 1, after a thinking block at index 0),
    /// the mutated emit MUST carry that same index so the
    /// codec's re-encode lands the synthetic frame on the right
    /// block. Without this, the live run failed with "Content
    /// block is not a text block."
    #[test]
    fn mutated_token_preserves_originating_block_index() {
        let mut inst = instance(&["work_type"]);
        let mut buf = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);

        let input = token_at(
            "before<noodle:work_type>build</noodle:work_type>after",
            Some(1),
        );
        let events = inst.apply(input, &mut side);

        assert_eq!(events.len(), 1);
        let NormalizedEvent::Token {
            index,
            source,
            text,
        } = &events[0]
        else {
            panic!("expected mutated Token");
        };
        assert_eq!(text, "beforeafter");
        assert!(
            matches!(source, EventSource::Mutated),
            "stripping flips provenance to Mutated"
        );
        assert_eq!(
            *index,
            Some(1),
            "the mutated re-emit must carry the original block index — \
             the live-run bug was that this got reset to 0 (or None) \
             and the codec then targeted the wrong content block."
        );
    }

    #[test]
    fn flush_uses_last_seen_block_index() {
        // A truncated marker on EOS releases bytes via flush().
        // Those bytes belong to the last block we processed.
        let mut inst = instance(&["work_type"]);
        let mut buf = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);

        // Feed a token at index 1 that ENDS mid-marker — the
        // scanner buffers the suspect prefix and emits nothing.
        let _ = inst.apply(token_at("before<noodle:work", Some(1)), &mut side);
        // EOS: flush releases the buffered "<noodle:work" verbatim
        // (truncated-marker contract). It must be tagged at the
        // last block index we saw — index 1.
        let flushed = inst.flush(&mut side);
        // The flush MAY emit one Token or zero, depending on
        // exactly what the scanner releases. If it emits anything,
        // the index must be 1.
        for ev in &flushed {
            if let NormalizedEvent::Token { index, .. } = ev {
                assert_eq!(
                    *index,
                    Some(1),
                    "flush must preserve the last block index seen during apply",
                );
            }
        }
    }

    #[test]
    fn multiple_markers_emit_multiple_hints() {
        // A stream with two markers produces two Hints — one per
        // capture — so the Resolver can run its max-confidence /
        // tie-break algorithm over the full set.
        let mut inst = instance(&["work_type", "tool"]);
        let (_events, side) = drive(
            inst.as_mut(),
            &[
                "<noodle:work_type>research</noodle:work_type> mid <noodle:tool>Claude Code</noodle:tool>",
            ],
        );
        let hints: Vec<_> = side
            .iter()
            .filter_map(|e| match e {
                SideEffect::Hint(h) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(hints.len(), 2);
        // Order of emission follows the FSM's capture order.
        assert_eq!(hints[0].category.as_str(), "work_type");
        assert_eq!(hints[0].value.as_str(), "research");
        assert_eq!(hints[1].category.as_str(), "tool");
        assert_eq!(hints[1].value.as_str(), "Claude Code");
        // Every marker Hint shares the same source identifier.
        assert!(hints.iter().all(|h| h.source.as_str() == "marker"));
    }

    #[test]
    fn marker_split_across_token_boundaries_resolves() {
        let mut inst = instance(&["work_type"]);
        let (events, side) = drive(
            inst.as_mut(),
            &[
                "tokens <noodle:work_",
                "type>research</noodle:wo",
                "rk_type> done",
            ],
        );
        let joined: String = events.iter().map(|e| text_of(e).to_owned()).collect();
        assert_eq!(joined, "tokens  done");
        assert!(side.iter().any(|e| matches!(e, SideEffect::Artifact(a)
            if a.value.as_str() == "research")));
    }

    #[test]
    fn fully_stripped_token_emits_single_space_placeholder() {
        // Anthropic rejects requests whose `messages` array carries a
        // text content block with an empty `text` field. If the
        // model's entire content for a block is a marker, the strip
        // would otherwise produce zero text bytes, the client would
        // reconstruct `{"text":"", "type":"text"}` in the next
        // request's history, and Anthropic would 400 with
        // `messages: text content blocks must be non-empty`. The
        // transform inserts a single-space placeholder once per
        // block so the block never lands empty in history.
        let mut inst = instance(&["work_type"]);
        let (events, side) = drive(
            inst.as_mut(),
            &["<noodle:work_type>build</noodle:work_type>"],
        );
        assert_eq!(events.len(), 1, "exactly one placeholder Token: {events:?}");
        let text = text_of(&events[0]);
        assert_eq!(text, " ", "placeholder is a single space");
        assert!(side.iter().any(|e| matches!(e, SideEffect::Artifact(_))));
    }

    #[test]
    fn fully_stripped_token_emits_placeholder_only_once_per_block() {
        // Two marker-only deltas in the same content block (same
        // `index`) yield exactly one placeholder space — not two.
        let mut inst = instance(&["work_type"]);
        let (events, _) = drive(
            inst.as_mut(),
            &[
                "<noodle:work_type>code</noodle:work_type>",
                "<noodle:work_type>code</noodle:work_type>",
            ],
        );
        let texts: Vec<&str> = events.iter().map(text_of).collect();
        assert_eq!(
            texts,
            vec![" "],
            "single placeholder across multiple deltas: {events:?}"
        );
    }

    #[test]
    fn placeholder_suppressed_when_real_content_already_emitted() {
        // If the block has real (non-marker) content before the
        // marker-only delta arrives, no placeholder is needed —
        // the block is already non-empty.
        let mut inst = instance(&["work_type"]);
        let (events, _) = drive(
            inst.as_mut(),
            &["hello ", "<noodle:work_type>code</noodle:work_type>"],
        );
        let joined: String = events.iter().map(|e| text_of(e).to_owned()).collect();
        assert_eq!(joined, "hello ", "real content alone: no placeholder added");
    }

    #[test]
    fn truncated_marker_released_verbatim_on_flush() {
        let mut inst = instance(&["work_type"]);
        let (events, side) = drive(inst.as_mut(), &["lead <noodle:work_ty"]);
        let joined: String = events.iter().map(|e| text_of(e).to_owned()).collect();
        assert_eq!(
            joined, "lead <noodle:work_ty",
            "held bytes must be released on flush, never swallowed",
        );
        assert!(side.is_empty());
    }

    #[test]
    fn disabled_scanner_is_pure_passthrough_preserving_provenance() {
        let mut inst = instance(&[]);
        let mut buf = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);
        let out = inst.apply(token("<noodle:work_type>x</noodle:work_type>"), &mut side);
        assert_eq!(out.len(), 1);
        // Untouched: original Upstream provenance preserved.
        assert!(matches!(
            out[0],
            NormalizedEvent::Token {
                source: EventSource::Upstream(_),
                ..
            }
        ));
        assert!(buf.is_empty());
    }

    #[test]
    fn non_token_events_pass_through_untouched() {
        let mut inst = instance(&["work_type"]);
        let mut buf = Vec::new();
        let mut side = SideChannelTx::new(&mut buf, 0, 0);
        let out = inst.apply(
            NormalizedEvent::TurnStart {
                round_trip_id: RoundTripId::new("t1"),
                role: noodle_core::event::Role::Assistant,
            },
            &mut side,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], NormalizedEvent::TurnStart { .. }));
        assert!(buf.is_empty());
    }
}
