//! Family 7 — `task_plan`: primitives the agent's planning channel
//! emits.
//!
//! Unlike the other families, `task_plan` carries multiple struct
//! shapes rather than a single open enum. Each shape is a small,
//! evidence-driven primitive lifted from observed planning channels
//! (e.g. Claude's `TodoWrite`, Cursor's plan steps, `OpenAI`'s planning).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TodoItem {
    pub id: Option<String>,
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanStep {
    pub order: u32,
    pub description: String,
    pub depends_on: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Goal {
    pub statement: String,
    pub success_criteria: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Constraint {
    pub statement: String,
    pub kind: ConstraintKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    MustDo,
    MustNotDo,
    Preference,
    Unknown,
}
