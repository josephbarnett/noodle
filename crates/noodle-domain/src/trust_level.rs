//! Family 4 — `trust_level`: how much the harness trusts the
//! source of a content block.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    SystemTrusted,
    UserTrusted,
    ModelOutput,
    ToolOutput,
    InjectedReminder,
    VendorSpecific(VendorTrustLevel),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorTrustLevel {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
