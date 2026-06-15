//! Family 11 — `principal_identity`: non-PII identifiers for the
//! actor / device / role context.
//!
//! Resolving these to humans or organisations is the embellishment
//! plane's job. This crate exposes only the keys.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PrincipalIdentity {
    pub device_id: Option<DeviceId>,
    pub machine_tag: Option<String>,
    pub account_role: Option<AccountRole>,
}

/// Stable opaque device identifier. Not PII.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DeviceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for DeviceId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    Admin,
    StandardUser,
    ServiceAccount,
    Unknown,
    VendorSpecific(String),
}
