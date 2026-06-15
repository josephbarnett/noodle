//! Hint resolution — collapse ranked `ContextHint`s into a `Resolved`
//! map of `category -> value`.
//!
//! Algorithm:
//!
//! 1. Group hints by category.
//! 2. Per category, pick the max-confidence hint; tie-break by
//!    detector priority order (configured per-category).
//! 3. If the category declares a non-empty `values:` allow-list:
//!    case-insensitive match → emit canonical form. No match falls
//!    through to default (or omitted).
//! 4. Defaults pass: any declared category with a `default:` and no
//!    surviving entry gets the default value.

use std::collections::HashMap;

use smol_str::SmolStr;

use crate::ContextHint;

/// Resolution config — what categories exist, what values are
/// allowed, what tie-break order to use, what default to fall back to.
#[derive(Debug, Clone, Default)]
pub struct CategoryConfig {
    pub categories: HashMap<SmolStr, CategoryDef>,
}

impl CategoryConfig {
    /// The hardcoded default attribution categories shipped in
    /// `noodle-core` (ADR 020 §2.5). Distinct from the derived
    /// `Default` which is empty — `Default::default()` keeps its
    /// existing meaning for callers that want an empty config to
    /// build up programmatically.
    ///
    /// V1 ships two open-list categories matched to what the
    /// shipped Hint sources actually produce:
    ///
    /// - **`tool`** — the LLM client (Claude Code, Cursor, …).
    ///   Sourced from the `user_agent` Hint (wirelog `user_agent_hint`).
    ///   Markers with `<noodle:tool>NAME</noodle:tool>` also
    ///   feed this category; when both fire, the marker source
    ///   wins on tie-break.
    /// - **`work_type`** — the per-turn classification the
    ///   default `AttributionEnhancer` directive asks the model
    ///   to emit (`code` / `research` / `writing` / `analysis` /
    ///   `other`). Sourced from the marker
    ///   `<noodle:work_type>VALUE</noodle:work_type>`.
    ///
    /// Both categories are open-list for v1 — accept any
    /// detected value verbatim. Closed allow-lists (e.g. pinning
    /// `work_type` to the directive's five values) land when
    /// item 5 / story 032 fills in the canonical sets. YAML
    /// loading is its own follow-on story (034).
    ///
    /// `detectors` lists the priority-ordered tie-break sources.
    /// `marker` first (model self-tagged — strongest signal);
    /// `user_agent` second (header-derived heuristic).
    #[must_use]
    pub fn with_attribution_defaults() -> Self {
        let mut categories: HashMap<SmolStr, CategoryDef> = HashMap::new();
        let priority = vec![
            SmolStr::new_static("marker"),
            SmolStr::new_static("user_agent"),
        ];
        categories.insert(
            SmolStr::new_static("tool"),
            CategoryDef {
                values: vec![],
                detectors: priority.clone(),
                default: None,
            },
        );
        categories.insert(
            SmolStr::new_static("work_type"),
            CategoryDef {
                values: vec![],
                detectors: priority,
                default: None,
            },
        );
        Self { categories }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CategoryDef {
    /// Empty list → open (accept any detected value verbatim).
    /// Non-empty → closed allow-list; values are canonicalized to
    /// the form in this list, case-insensitively.
    pub values: Vec<SmolStr>,
    /// Tie-break ordering when multiple detectors emit the same
    /// confidence. Earlier in the list wins.
    pub detectors: Vec<SmolStr>,
    /// Fallback when no detected hint survives resolution.
    pub default: Option<SmolStr>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolved(pub HashMap<SmolStr, SmolStr>);

impl Resolved {
    #[must_use]
    pub fn get(&self, category: &str) -> Option<&str> {
        self.0.get(category).map(SmolStr::as_str)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Resolve hints against a category configuration.
#[must_use]
pub fn resolve(hints: &[ContextHint], config: &CategoryConfig) -> Resolved {
    let mut out: HashMap<SmolStr, SmolStr> = HashMap::new();

    let mut grouped: HashMap<&str, Vec<&ContextHint>> = HashMap::new();
    for h in hints {
        grouped.entry(&h.category).or_default().push(h);
    }

    for (category, group) in grouped {
        let cat_def = config.categories.get(category);
        let Some(best) = pick_best(&group, cat_def) else {
            continue;
        };

        // Closed allow-list: must match. If miss, fall through to
        // the defaults pass (don't emit anything for this category here).
        if let Some(def) = cat_def
            && !def.values.is_empty()
        {
            if let Some(canonical) = canonicalize(&best.value, &def.values) {
                out.insert(SmolStr::new(category), canonical);
            }
            continue;
        }

        // Open list (or undeclared category): accept verbatim.
        out.insert(SmolStr::new(category), best.value.clone());
    }

    // Defaults pass.
    for (category, def) in &config.categories {
        let Some(default) = &def.default else {
            continue;
        };
        if out.contains_key(category) {
            continue;
        }
        out.insert(category.clone(), default.clone());
    }

    Resolved(out)
}

fn pick_best<'a>(
    hints: &[&'a ContextHint],
    cat_def: Option<&CategoryDef>,
) -> Option<&'a ContextHint> {
    let priority = |source: &str| -> usize {
        let Some(def) = cat_def else {
            return usize::MAX;
        };
        def.detectors
            .iter()
            .position(|d| d.as_str() == source)
            .unwrap_or(usize::MAX)
    };

    let mut best: Option<&ContextHint> = None;
    let mut best_priority = usize::MAX;

    for h in hints {
        match best {
            None => {
                best = Some(*h);
                best_priority = priority(&h.source);
            }
            Some(b) => {
                if h.confidence > b.confidence {
                    best = Some(*h);
                    best_priority = priority(&h.source);
                } else if (h.confidence - b.confidence).abs() < f32::EPSILON {
                    let p = priority(&h.source);
                    if p < best_priority {
                        best = Some(*h);
                        best_priority = p;
                    }
                }
            }
        }
    }

    best
}

fn canonicalize(detected: &str, allow: &[SmolStr]) -> Option<SmolStr> {
    let lower = detected.to_ascii_lowercase();
    allow
        .iter()
        .find(|v| v.eq_ignore_ascii_case(&lower))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint(category: &str, value: &str, conf: f32, source: &str) -> ContextHint {
        ContextHint {
            category: category.into(),
            value: value.into(),
            confidence: conf,
            source: source.into(),
        }
    }

    #[test]
    fn empty_yields_empty() {
        let r = resolve(&[], &CategoryConfig::default());
        assert!(r.is_empty());
    }

    #[test]
    fn open_list_accepts_verbatim() {
        let hints = vec![hint("tool", "Claude Code", 0.95, "user_agent")];
        let cfg = CategoryConfig {
            categories: [("tool".into(), CategoryDef::default())].into(),
        };
        let r = resolve(&hints, &cfg);
        assert_eq!(r.get("tool"), Some("Claude Code"));
    }

    #[test]
    fn higher_confidence_wins() {
        let hints = vec![
            hint("tool", "Cursor", 0.6, "system_prompt"),
            hint("tool", "Claude Code", 0.95, "user_agent"),
        ];
        let r = resolve(&hints, &CategoryConfig::default());
        assert_eq!(r.get("tool"), Some("Claude Code"));
    }

    #[test]
    fn tie_break_by_priority_order() {
        // Two detectors emit the same confidence; the one listed
        // first in `detectors:` wins.
        let hints = vec![
            hint("tool", "Cursor", 0.9, "system_prompt"),
            hint("tool", "Claude Code", 0.9, "user_agent"),
        ];
        let cfg = CategoryConfig {
            categories: [(
                "tool".into(),
                CategoryDef {
                    detectors: vec!["user_agent".into(), "system_prompt".into()],
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&hints, &cfg);
        assert_eq!(r.get("tool"), Some("Claude Code"));
    }

    #[test]
    fn closed_list_canonicalizes_case() {
        let hints = vec![hint("tool", "claude code", 0.9, "user_agent")];
        let cfg = CategoryConfig {
            categories: [(
                "tool".into(),
                CategoryDef {
                    values: vec!["Claude Code".into(), "Cursor".into()],
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&hints, &cfg);
        assert_eq!(r.get("tool"), Some("Claude Code"));
    }

    #[test]
    fn closed_list_drops_unknown_value() {
        let hints = vec![hint("tool", "Vim", 0.9, "user_agent")];
        let cfg = CategoryConfig {
            categories: [(
                "tool".into(),
                CategoryDef {
                    values: vec!["Claude Code".into()],
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&hints, &cfg);
        assert!(r.get("tool").is_none());
    }

    #[test]
    fn default_fills_when_no_hint_survives() {
        let hints = vec![hint("tool", "Vim", 0.9, "user_agent")];
        let cfg = CategoryConfig {
            categories: [(
                "tool".into(),
                CategoryDef {
                    values: vec!["Claude Code".into()],
                    default: Some("unknown".into()),
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&hints, &cfg);
        assert_eq!(r.get("tool"), Some("unknown"));
    }

    #[test]
    fn default_fills_when_no_hint_at_all() {
        let cfg = CategoryConfig {
            categories: [(
                "team".into(),
                CategoryDef {
                    default: Some("platform".into()),
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&[], &cfg);
        assert_eq!(r.get("team"), Some("platform"));
    }

    #[test]
    fn unknown_priority_source_loses_to_known() {
        // Both confidence 0.5; the source with no priority entry
        // is ranked behind the source that's listed.
        let hints = vec![
            hint("tool", "Cursor", 0.5, "fallback"),
            hint("tool", "Claude Code", 0.5, "user_agent"),
        ];
        let cfg = CategoryConfig {
            categories: [(
                "tool".into(),
                CategoryDef {
                    detectors: vec!["user_agent".into()],
                    ..Default::default()
                },
            )]
            .into(),
        };
        let r = resolve(&hints, &cfg);
        assert_eq!(r.get("tool"), Some("Claude Code"));
    }
}
