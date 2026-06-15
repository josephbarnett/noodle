//! Family 2 — `content_category`: what the bytes of a content
//! block contain.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentCategory {
    Code,
    Command,
    Credential,
    Pii,
    Secret,
    Prose,
    StructuredData,
    Path,
    Url,
    Reasoning,
    Plan,
    VendorSpecific(VendorContentCategory),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorContentCategory {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
