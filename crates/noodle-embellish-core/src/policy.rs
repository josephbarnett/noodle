//! Watchtower D2 — observe-mode policy classifier port (ADR 045
//! §2.2 / §2.4 / §2.5).
//!
//! Pure: no I/O, no clock, no enforcement. The classifier inspects a
//! [`DecodedPair`] and returns an optional [`PolicyDecision`] that
//! the caller stamps onto the OTLP record as `policy.*` attributes
//! beside `brain.*`. Observation only — D2.1 ships
//! [`AllowAllClassifier`] so the rails are live with zero risk; the
//! first real rule lands in D2.2 (ADR 045 §2.4 observe-first).

use noodle_domain::decoders::DecodedEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::decoded::DecodedPair;

/// One of the five Watchtower verbs (ADR 045 §2.2). Observation-mode
/// classifiers emit `Allow` or `Flag`; `Annotate`, `Redact`, `Block`
/// land with enforcement (post-D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allow,
    Flag,
    Annotate,
    Redact,
    Block,
}

impl PolicyVerdict {
    /// Lower-case canonical form for OTLP attribute values (matches
    /// ADR 045 §2.5: `policy.decision = "allow" | "flag" | …`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Flag => "flag",
            Self::Annotate => "annotate",
            Self::Redact => "redact",
            Self::Block => "block",
        }
    }
}

/// Severity of an enforcement verb (ADR 045 §2.2). Only meaningful
/// when [`PolicyVerdict`] is `Block` or `Redact`; absent on
/// observation-only verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Hard,
    Soft,
}

impl PolicyMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

/// The decision surface the classifier examined (ADR 045 §2.5
/// `policy.surface`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySurface {
    /// Request-side: system prompt, message history, tool definitions,
    /// `tool_result` payloads (ADR 045 §2.1 request-side bullet).
    Request,
    /// Response-side: a `tool_use` action the model proposed (ADR 045
    /// §2.1 response-side bullet).
    ResponseToolUse,
}

impl PolicySurface {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::ResponseToolUse => "response.tool_use",
        }
    }
}

/// Per-pair policy verdict — the payload that becomes `policy.*`
/// attributes on the OTLP record (ADR 045 §2.5).
///
/// `Serialize`/`Deserialize` so the viewer's brain-aware row
/// rendering and the shipper's OTLP mapper consume the same shape
/// without an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub decision: PolicyVerdict,
    /// Set only on `Block` / `Redact` enforcement verbs; `None` for
    /// observation-mode `Allow` / `Flag` / `Annotate`.
    pub mode: Option<PolicyMode>,
    /// Classifier-reported risk score in `[0.0, 1.0]`. `Allow` from
    /// the observe-mode stub is `0.0`; future judge-model classifiers
    /// emit calibrated scores.
    pub risk: f64,
    /// Stable identifier of the rule or plugin that produced this
    /// verdict (`policy.rule` on the OTLP record).
    pub rule: String,
    /// Short, human-readable explanation (`policy.rationale`).
    pub rationale: String,
    pub surface: PolicySurface,
}

/// A classifier adapter (ADR 045 §2.2). Pure: no I/O, no mutation of
/// the pair, no enforcement. Returns `None` when the classifier
/// chose not to score this pair — the row's `policy.*` columns stay
/// NULL, which is distinct from `Some(Allow)` (classifier ran, said
/// allow).
pub trait PolicyClassifier: Send + Sync {
    fn classify(&self, pair: &DecodedPair) -> Option<PolicyDecision>;
}

/// Observation-mode stub for D2.1 — emits `Allow` for every paired
/// round-trip with `rule = "default"`, `risk = 0.0`. The rails are
/// live but no real signal fires. D2.2's [`ChainClassifier`] uses it
/// as the fallback after the real classifiers decline (ADR 045 §2.4
/// observe-first).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllClassifier;

impl PolicyClassifier for AllowAllClassifier {
    fn classify(&self, _pair: &DecodedPair) -> Option<PolicyDecision> {
        Some(PolicyDecision {
            decision: PolicyVerdict::Allow,
            mode: None,
            risk: 0.0,
            rule: "default".to_owned(),
            rationale: "no policy active".to_owned(),
            surface: PolicySurface::Request,
        })
    }
}

/// D2.2 — first real Watchtower rule. Scans the decoded response
/// stream for `DecodedEvent::ToolUse { tool_name == "Bash" }` whose
/// `input.command` matches a destructive shell pattern, emits a
/// `Flag` verdict naming the matched pattern. Returns `None` when no
/// Bash `tool_use` is present or no pattern matches — the
/// [`ChainClassifier`] then falls through to the next classifier
/// (typically [`AllowAllClassifier`]) so every row still gets a
/// verdict per ADR 045 §2.5.
///
/// Observation-only by construction: emits `Flag`, never `Block` or
/// `Redact`. Promotion to enforcement waits on D7 once observed
/// precision against live traffic justifies it (ADR 045 §2.4).
#[derive(Debug, Clone, Copy, Default)]
pub struct BashDestructiveClassifier;

/// Match style for a destructive pattern:
/// - `Contains` — naive substring match (`mkfs` anywhere)
/// - `RootedAt` — the literal followed by whitespace, EOL, or `;`
///   (`rm -rf /` matches `rm -rf / --no-preserve-root` but NOT
///   `rm -rf /tmp/x`)
#[derive(Debug, Clone, Copy)]
enum MatchKind {
    Contains,
    RootedAt,
}

fn matches(command: &str, needle: &str, kind: MatchKind) -> bool {
    match kind {
        MatchKind::Contains => command.contains(needle),
        MatchKind::RootedAt => {
            let mut search = command;
            while let Some(idx) = search.find(needle) {
                let after = &search[idx + needle.len()..];
                match after.chars().next() {
                    None | Some(' ' | '\t' | '\n' | ';') => return true,
                    _ => search = &search[idx + needle.len()..],
                }
            }
            false
        }
    }
}

/// Destructive shell patterns the D2.2 classifier flags on. Order is
/// significant — the first match wins so the rationale names the
/// most specific pattern available. Each tuple is
/// `(needle, match_kind, rule_id, risk, rationale_template)`.
const BASH_DESTRUCTIVE_PATTERNS: &[(&str, MatchKind, &str, f64, &str)] = &[
    (
        "rm -rf /",
        MatchKind::RootedAt,
        "bash.rm_rf_root",
        1.0,
        "rm -rf rooted at / — unrecoverable filesystem wipe",
    ),
    (
        "rm -rf",
        MatchKind::Contains,
        "bash.rm_rf",
        0.8,
        "rm -rf — recursive force-delete; user intent must be explicit",
    ),
    (
        "git push --force",
        MatchKind::Contains,
        "bash.git_force_push",
        0.7,
        "git push --force — rewrites upstream history; reviewers cannot see overwritten commits",
    ),
    (
        "git push -f",
        MatchKind::Contains,
        "bash.git_force_push",
        0.7,
        "git push -f — rewrites upstream history; reviewers cannot see overwritten commits",
    ),
    (
        "git reset --hard",
        MatchKind::Contains,
        "bash.git_reset_hard",
        0.6,
        "git reset --hard — discards uncommitted work in the working tree",
    ),
    (
        "mkfs",
        MatchKind::Contains,
        "bash.mkfs",
        1.0,
        "mkfs — formats a filesystem; existing data is unrecoverable",
    ),
    (
        "dd if=",
        MatchKind::Contains,
        "bash.dd_overwrite",
        0.9,
        "dd if= — raw block write; mistargeting a device wipes it",
    ),
    (
        ":(){",
        MatchKind::Contains,
        "bash.fork_bomb",
        1.0,
        "fork-bomb pattern — exhausts the process table",
    ),
];

impl PolicyClassifier for BashDestructiveClassifier {
    fn classify(&self, pair: &DecodedPair) -> Option<PolicyDecision> {
        for ev in &pair.events {
            let DecodedEvent::ToolUse {
                tool_name, input, ..
            } = ev
            else {
                continue;
            };
            if tool_name != "Bash" {
                continue;
            }
            let command = input.get("command").and_then(Value::as_str)?;
            for &(needle, kind, rule, risk, rationale) in BASH_DESTRUCTIVE_PATTERNS {
                if matches(command, needle, kind) {
                    return Some(PolicyDecision {
                        decision: PolicyVerdict::Flag,
                        mode: None,
                        risk,
                        rule: rule.to_owned(),
                        rationale: rationale.to_owned(),
                        surface: PolicySurface::ResponseToolUse,
                    });
                }
            }
        }
        None
    }
}

/// Composes classifiers in declared order: the first
/// [`PolicyClassifier`] to return `Some(_)` wins. ADR 045 §2.2's
/// adapter model — the chain is the production classifier and each
/// link is independently swappable (rules now, judge-model later,
/// WASM plugin per ADR 039 eventually).
///
/// Per ADR 045 §2.5 every row should carry a `policy.*` shape, so
/// callers should terminate the chain with [`AllowAllClassifier`].
pub struct ChainClassifier {
    classifiers: Vec<Box<dyn PolicyClassifier>>,
}

impl ChainClassifier {
    #[must_use]
    pub fn new(classifiers: Vec<Box<dyn PolicyClassifier>>) -> Self {
        Self { classifiers }
    }

    /// D2.2 production chain: bash destructive → allow-all fallback.
    #[must_use]
    pub fn d2_default() -> Self {
        Self::new(vec![
            Box::new(BashDestructiveClassifier),
            Box::new(AllowAllClassifier),
        ])
    }
}

impl PolicyClassifier for ChainClassifier {
    fn classify(&self, pair: &DecodedPair) -> Option<PolicyDecision> {
        self.classifiers.iter().find_map(|c| c.classify(pair))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::reader::TapEntryView;

    fn pair() -> DecodedPair {
        DecodedPair {
            request: TapEntryView::from_value(json!({
                "direction": "request",
                "event_id": "evt-1",
                "provider": "anthropic",
                "session_hash": "sess-1",
                "body": {"max_tokens": 64000},
            })),
            response: TapEntryView::from_value(json!({
                "direction": "response",
                "event_id": "evt-1",
                "provider": "anthropic",
                "status": 200,
            })),
            events: Vec::new(),
        }
    }

    #[test]
    fn allow_all_emits_allow_default_zero_risk() {
        let c = AllowAllClassifier;
        let d = c.classify(&pair()).expect("stub always emits");
        assert_eq!(d.decision, PolicyVerdict::Allow);
        assert_eq!(d.mode, None);
        assert!((d.risk - 0.0).abs() < f64::EPSILON);
        assert_eq!(d.rule, "default");
        assert_eq!(d.surface, PolicySurface::Request);
    }

    #[test]
    fn verdict_str_matches_adr_045_section_2_5() {
        assert_eq!(PolicyVerdict::Allow.as_str(), "allow");
        assert_eq!(PolicyVerdict::Flag.as_str(), "flag");
        assert_eq!(PolicyVerdict::Annotate.as_str(), "annotate");
        assert_eq!(PolicyVerdict::Redact.as_str(), "redact");
        assert_eq!(PolicyVerdict::Block.as_str(), "block");
    }

    #[test]
    fn mode_and_surface_str_match_adr_045() {
        assert_eq!(PolicyMode::Hard.as_str(), "hard");
        assert_eq!(PolicyMode::Soft.as_str(), "soft");
        assert_eq!(PolicySurface::Request.as_str(), "request");
        assert_eq!(PolicySurface::ResponseToolUse.as_str(), "response.tool_use");
    }

    fn pair_with_bash(command: &str) -> DecodedPair {
        use noodle_domain::capability::Capability;
        use noodle_domain::envelope_metadata::ProviderId;
        let mut p = pair();
        p.events.push(DecodedEvent::ToolUse {
            request_id: "evt-1".to_owned(),
            provider: ProviderId::Anthropic,
            block_index: 0,
            tool_use_id: "toolu_abc".to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({"command": command}),
            capability: Capability::Execute,
        });
        p
    }

    #[test]
    fn bash_classifier_flags_rm_rf() {
        let d = BashDestructiveClassifier
            .classify(&pair_with_bash("rm -rf /tmp/x"))
            .expect("rm -rf must flag");
        assert_eq!(d.decision, PolicyVerdict::Flag);
        assert_eq!(d.rule, "bash.rm_rf");
        assert!(d.risk > 0.5);
        assert_eq!(d.surface, PolicySurface::ResponseToolUse);
    }

    #[test]
    fn bash_classifier_flags_rm_rf_root_over_generic_rm_rf() {
        // Order matters — the more specific pattern wins.
        let d = BashDestructiveClassifier
            .classify(&pair_with_bash("sudo rm -rf / --no-preserve-root"))
            .expect("rm -rf / must flag");
        assert_eq!(d.rule, "bash.rm_rf_root");
        assert!((d.risk - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bash_classifier_flags_git_force_push_variants() {
        for cmd in ["git push --force origin main", "git push -f origin main"] {
            let d = BashDestructiveClassifier
                .classify(&pair_with_bash(cmd))
                .unwrap_or_else(|| panic!("{cmd} must flag"));
            assert_eq!(d.rule, "bash.git_force_push", "cmd={cmd}");
        }
    }

    #[test]
    fn bash_classifier_returns_none_on_safe_command() {
        // Safe Bash commands must yield None so the chain falls
        // through to AllowAllClassifier — distinguishing
        // "classifier ran, said safe" (chain) from "classifier
        // didn't fire" (returned None) is the load-bearing
        // invariant for D7 enforcement promotion.
        assert!(
            BashDestructiveClassifier
                .classify(&pair_with_bash("ls -la"))
                .is_none()
        );
    }

    #[test]
    fn bash_classifier_returns_none_when_no_bash_tool_use() {
        // No tool_use events at all — empty `events` vec.
        assert!(BashDestructiveClassifier.classify(&pair()).is_none());
    }

    #[test]
    fn chain_default_flags_bash_destructive_pair() {
        let chain = ChainClassifier::d2_default();
        let d = chain
            .classify(&pair_with_bash("rm -rf /tmp"))
            .expect("chain emits on every pair");
        assert_eq!(d.decision, PolicyVerdict::Flag);
        assert_eq!(d.rule, "bash.rm_rf");
    }

    #[test]
    fn chain_default_falls_through_to_allow_when_no_rule_fires() {
        let chain = ChainClassifier::d2_default();
        let d = chain
            .classify(&pair_with_bash("ls -la"))
            .expect("chain emits on every pair");
        assert_eq!(d.decision, PolicyVerdict::Allow);
        assert_eq!(d.rule, "default");
    }

    #[test]
    fn decision_roundtrips_through_serde() {
        let d = PolicyDecision {
            decision: PolicyVerdict::Flag,
            mode: None,
            risk: 0.42,
            rule: "bash.rm_rf".to_owned(),
            rationale: "destructive shell pattern".to_owned(),
            surface: PolicySurface::ResponseToolUse,
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: PolicyDecision = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
