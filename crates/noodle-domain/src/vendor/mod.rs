//! Vendor subtypes (ADR 029 §3.1).
//!
//! Each family's `VendorSpecific` variant carries a `VendorId` (this
//! module) plus a vendor-defined tag. Per-vendor modules under this
//! folder export tag constants and any vendor-shaped payloads.

use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod google;
pub mod openai;

/// Canonical vendor identifier. Open enum — unknown vendors round-
/// trip through `Other`. Distinct from [`crate::envelope_metadata::ProviderId`]:
/// `ProviderId` names a service the proxy observes; `VendorId` names
/// the originator of a classification taxonomy. The two often align
/// but are not identical.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VendorId {
    Anthropic,
    Openai,
    Google,
    AwsBedrock,
    AzureOpenai,
    Cohere,
    Mistral,
    Other(String),
}

/// Verbatim vendor tag — the vendor's own term for a classification
/// not yet promoted to canonical (ADR 029 §3).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VendorTag(pub String);

impl VendorTag {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for VendorTag {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VendorTag {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
