//! Family 1 — `speech_act`: the pragmatic intent of a text block.
//!
//! Open enum. Canonical variants follow from the cross-vendor
//! recurrence rule (ADR 029 §3). Vendor-only patterns are carried
//! via [`VendorSpeechAct`] under [`SpeechAct::VendorSpecific`].

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechAct {
    Instruction,
    Claim,
    HedgedClaim,
    Question,
    Suggestion,
    Acknowledgement,
    Refusal,
    Clarification,
    VendorSpecific(VendorSpeechAct),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorSpeechAct {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
