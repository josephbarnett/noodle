//! Family 9 — `envelope_metadata`: per-record dispatch facts
//! indexed on by every consumer.

use serde::{Deserialize, Serialize};

/// Canonical provider identifier carried on every record. Open enum
/// so unknown providers round-trip through `Other`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    Anthropic,
    Openai,
    Google,
    AwsBedrock,
    AzureOpenai,
    Cohere,
    Mistral,
    Other(String),
}

/// Direction of a wire record relative to the agent → provider arrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Request,
    Response,
}

/// Zero-based index of a round-trip within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoundTripIndex(pub u32);

impl RoundTripIndex {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Path component of the endpoint a record was observed against —
/// e.g. `/v1/messages`. Carried alongside `ProviderId` for dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointPath(pub String);

impl EndpointPath {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for EndpointPath {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for EndpointPath {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
