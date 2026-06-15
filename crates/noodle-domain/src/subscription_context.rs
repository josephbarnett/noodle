//! Family 13 — `subscription_context`: identifiers that let
//! downstream consumers reconcile observed traffic against billed
//! traffic.
//!
//! `ApiKeyFingerprint` is the only fully wire-observable type here;
//! the others typically require embellishment-plane enrichment.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApiKeyFingerprint {
    /// Operator-configured visible-prefix length. Default 12 chars.
    pub prefix: String,
    pub kind: ApiKeyKind,
    pub source: ApiKeySource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
    ApiKey,
    Session,
    Oauth,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeySource {
    AuthorizationHeader,
    XApiKey,
    SessionCookie,
    UrlParam,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OrganizationContext {
    pub organization_id: Option<String>,
    pub parent_organization_id: Option<String>,
    pub account_type: AccountType,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    Enterprise,
    Personal,
    Api,
    Team,
    Free,
    Pro,
    Other(String),
    Unknown,
    VendorSpecific(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SubscriptionTier {
    pub tier: Option<TierLabel>,
    pub source: SubscriptionTierSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierLabel {
    Free,
    Pro,
    Team,
    Enterprise,
    Custom(String),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionTierSource {
    Header,
    UrlPath,
    ResponseMetadata,
    EmbellishmentPlane,
    Unknown,
}
