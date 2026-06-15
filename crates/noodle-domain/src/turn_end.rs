//! Family 8 — `turn_end`: wire-level turn-termination signals
//! normalised across vendors.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    EndTurn,
    MaxTokens,
    ToolUsePending,
    StopSequence,
    ContentFiltered,
    VendorSpecific(VendorTurnEnd),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorTurnEnd {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
