//! Family 10 — `observation_context`: where and by what this
//! round-trip was observed.
//!
//! Three structs:
//! - [`AgentApp`] — the agent harness in the field
//! - [`Machine`] — the host the agent ran on
//! - [`CollectorApp`] — the noodle build that observed the
//!   round-trip

use semver::Version;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentApp {
    pub name: AgentAppName,
    pub version: Option<Version>,
    pub build_hash: Option<String>,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub build_date: Option<OffsetDateTime>,
    pub source: AgentAppSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAppName {
    ClaudeCode,
    OpenCode,
    Cursor,
    ChatGptDesktop,
    ClaudeDesktop,
    CodexCli,
    Warp,
    Zed,
    VendorSpecific(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAppSource {
    UserAgentHeader,
    BillingHeader,
    InferredFromPath,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Machine {
    pub hostname: Option<String>,
    pub os_family: OsFamily,
    pub os_version: Option<String>,
    pub architecture: Architecture,
    pub locale: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsFamily {
    Macos,
    Linux,
    Windows,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CollectorApp {
    pub name: String,
    pub version: Version,
    pub build_hash: String,
    #[serde(with = "time::serde::rfc3339")]
    pub build_date: OffsetDateTime,
    pub features: Vec<String>,
}
