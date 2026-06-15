//! Extensibility tests — `VendorSpecific` is the open-enum hatch
//! that lets unknown vendors and unpromoted patterns flow through
//! consumers without panic (ADR 029 §3, §4.1).
//!
//! These tests assert two properties:
//! 1. A `VendorSpecific` value round-trips losslessly.
//! 2. A consumer with a `_` fallback handles unknown vendors
//!    deterministically — the contract every consumer signs.

use noodle_domain::{
    speech_act::{SpeechAct, VendorSpeechAct},
    vendor::VendorId,
};

#[test]
fn vendor_specific_round_trips_unknown_vendor() {
    let value = SpeechAct::VendorSpecific(VendorSpeechAct {
        vendor: VendorId::Other("brand-new-vendor".into()),
        tag: "brand-new-vendor.proprietary.x".into(),
        closest_canonical: None,
    });
    let json = serde_json::to_string(&value).unwrap();
    let decoded: SpeechAct = serde_json::from_str(&json).unwrap();
    assert_eq!(value, decoded);
}

/// Consumer-side contract: a `_` arm handles unknown variants
/// gracefully. This test serves as the worked example consumers
/// can copy.
#[test]
fn consumer_fallback_pattern_compiles() {
    let value = SpeechAct::VendorSpecific(VendorSpeechAct {
        vendor: VendorId::Other("v".into()),
        tag: "v.tag".into(),
        closest_canonical: Some("claim".into()),
    });

    let label: &'static str = match &value {
        SpeechAct::Instruction => "instruction",
        SpeechAct::Claim | SpeechAct::HedgedClaim => "claim",
        SpeechAct::Question => "question",
        SpeechAct::Suggestion => "suggestion",
        SpeechAct::Acknowledgement => "acknowledgement",
        SpeechAct::Refusal => "refusal",
        SpeechAct::Clarification => "clarification",
        // The required fallback arm. Consumers MUST include this.
        SpeechAct::VendorSpecific(vs) => match vs.closest_canonical.as_deref() {
            Some("claim") => "claim",
            _ => "unknown",
        },
    };
    assert_eq!(label, "claim");
}

#[test]
fn unknown_canonical_falls_back_to_unknown() {
    let value = SpeechAct::VendorSpecific(VendorSpeechAct {
        vendor: VendorId::Anthropic,
        tag: "anthropic.unmappable".into(),
        closest_canonical: None,
    });
    let label = match value {
        SpeechAct::VendorSpecific(vs) => vs.closest_canonical.unwrap_or_else(|| "unknown".into()),
        _ => unreachable!(),
    };
    assert_eq!(label, "unknown");
}
