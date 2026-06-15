//! Family 3 — `capability`: the kind of action a tool call performs.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadFile,
    WriteFile,
    Execute,
    NetworkRequest,
    NetworkListen,
    SpawnAgent,
    SystemQuery,
    EnvironmentRead,
    VendorSpecific(VendorCapability),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorCapability {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
