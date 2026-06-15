//! Classification trait surface (ADR 029 §1).
//!
//! `noodle-domain` ships **no** classifier implementations — those
//! live in the consumer that needs them. This trait defines the
//! shape every classifier promises to provide.

use serde::{Deserialize, Serialize};

use crate::{
    citation_ref::CitationRef, content_category::ContentCategory, speech_act::SpeechAct,
    task_plan::TodoItem,
};

pub trait Classifier: Send + Sync {
    fn classify_text(&self, text: &str, context: &ClassificationContext) -> ClassificationResult;
}

/// Inputs a classifier may consult beyond the raw text — e.g. which
/// content block this text came from, the trust level of the source,
/// or upstream context the classifier should be aware of.
///
/// Intentionally open: fields are additive; consumers ignore unknown
/// fields via serde's default behaviour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ClassificationContext {
    pub block_index: Option<u32>,
    pub source_hint: Option<String>,
    pub upstream_text: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ClassificationResult {
    pub speech_act: Option<SpeechAct>,
    pub category: Option<ContentCategory>,
    pub citations: Vec<CitationRef>,
    pub plan_items: Vec<TodoItem>,
}

/// Trivial classifier that returns an empty [`ClassificationResult`].
/// Useful as a default in tests and as a worked example of the trait
/// shape. Production classifiers ship in consumer crates.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullClassifier;

impl Classifier for NullClassifier {
    fn classify_text(&self, _text: &str, _context: &ClassificationContext) -> ClassificationResult {
        ClassificationResult::default()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClassificationContext, ClassificationResult, Classifier, NullClassifier};

    #[test]
    fn null_classifier_returns_empty() {
        let result = NullClassifier.classify_text("hello", &ClassificationContext::default());
        assert_eq!(result, ClassificationResult::default());
    }
}
