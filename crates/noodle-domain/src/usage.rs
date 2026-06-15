//! Family 12 — `usage`: vendor-emitted quantitative facts about
//! a round-trip.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cached_read: Option<u64>,
    pub cached_creation: Option<u64>,
    pub reasoning: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vendor_extras: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Latency {
    pub time_to_first_byte_ms: Option<u64>,
    pub total_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RetryCount {
    pub attempts: u32,
    pub last_error_kind: Option<String>,
}
