//! Family 5 — `citation_ref`: references to external sources / files /
//! URLs the content cites.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CitationRef {
    FilePath {
        path: String,
    },
    UrlReference {
        url: String,
    },
    LineRange {
        path: String,
        start: u64,
        end: u64,
    },
    CommitHash {
        hash: String,
    },
    IssueRef {
        repository: Option<String>,
        number: u64,
    },
    VendorSpecific(VendorCitationRef),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorCitationRef {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
    pub payload: Option<serde_json::Value>,
}
