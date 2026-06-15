//! Family 6 — `reminder_subtype`: the kind of system / system-reminder
//! enhancement.

use serde::{Deserialize, Serialize};

use crate::vendor::VendorId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderSubtype {
    SkillCatalogue,
    ToolAvailability,
    ContextRefresh,
    WorkingDirState,
    SafetyClassifier,
    LongConversation,
    VendorSpecific(VendorReminderSubtype),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorReminderSubtype {
    pub vendor: VendorId,
    pub tag: String,
    pub closest_canonical: Option<String>,
}
