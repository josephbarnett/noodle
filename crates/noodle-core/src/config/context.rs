//! `[context]` section of `noodle.toml` — ADR 048 §8.
//!
//! Operator-authored configuration for LLM self-classification:
//! which placement, which tags, which directive text, which marker
//! namespace. Pure data + validation; the runtime wiring lives in
//! `noodle-proxy::lib.rs` and the realising adapters in
//! `noodle-adapters` (`MarkerStripTransform`,
//! `OpenAiAttributionEnhancer` / `AttributionEnhancer`).
//!
//! See `docs/guides/enhance-extract-config.md` for the operator
//! reference (every field documented + worked examples).

use serde::Deserialize;

/// `[context]` section. Disabled section / absent file →
/// feature inert, response path byte-for-byte the un-instrumented
/// behavior.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// Master gate. Absent file or `enabled = false` keeps the
    /// feature completely off.
    #[serde(default)]
    pub enabled: bool,
    /// Ordered list of verbatim payloads to enhance. ≥1 required
    /// when `enabled = true`.
    #[serde(default)]
    pub enhancements: Vec<Enhancement>,
    /// Marker namespace + format the extractor harvests on responses.
    #[serde(default)]
    pub discovery: Discovery,
}

impl ContextConfig {
    /// Returns the union of every enhancement's declared tag names —
    /// the list the proxy threads into `MarkerStripFilterFactory`
    /// (response strip) and `OpenAiAttributionEnhancer::with_default_directive`
    /// (request enhance directive). Preserves first-appearance order
    /// across enhancements; de-duplicates.
    #[must_use]
    pub fn declared_tag_names(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for inj in &self.enhancements {
            for tag in &inj.tags {
                if seen.insert(tag.name.clone()) {
                    out.push(tag.name.clone());
                }
            }
        }
        out
    }

    /// Fail-fast validation per `docs/guides/enhance-extract-config.md`
    /// § Validation. Disabled / zero-value configs are always valid.
    ///
    /// # Errors
    ///
    /// Returns a string naming the offending field path.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.enhancements.is_empty() {
            return Err("context.enhancements is empty when enabled = true".into());
        }
        for (idx, inj) in self.enhancements.iter().enumerate() {
            if inj.text.trim().is_empty() {
                return Err(format!("context.enhancements[{idx}].text is empty"));
            }
            for (jdx, tag) in inj.tags.iter().enumerate() {
                if tag.name.trim().is_empty() {
                    return Err(format!(
                        "context.enhancements[{idx}].tags[{jdx}].name is empty"
                    ));
                }
            }
        }
        self.discovery.validate()
    }
}

/// One enhancement — a verbatim directive text + its placement + the
/// declared tag set the model is asked to emit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enhancement {
    /// Where the directive lands. Defaults to [`Placement::System`]
    /// (matches the internal defaults default; current
    /// shipped placement is `user_prepend` per ADR 048 §5.1).
    #[serde(default)]
    pub r#as: Placement,
    /// The directive prompt, authored verbatim by the operator.
    pub text: String,
    /// Declared categories. Drives the session-scoped carry
    /// (ADR 048 §7 + operations doc § Session-scoped carry). Empty
    /// vec is legal — the extractor still harvests every marker in
    /// the namespace generically, but the aggregator will not
    /// session-stabilize undeclared categories.
    #[serde(default)]
    pub tags: Vec<Tag>,
}

/// One declared category. `name` is the marker NAME (the part after
/// the namespace: `work_type` → `<noodle:work_type>`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    pub name: String,
    /// "not-determined" sentinel. **Non-sticky**: a real value seen
    /// elsewhere in the session supersedes a turn that emitted only
    /// the default. Defaults to `"unknown"`.
    #[serde(default = "default_default_value")]
    pub default: String,
}

fn default_default_value() -> String {
    "unknown".into()
}

/// Where the directive attaches. The destination codec realizes
/// each abstract placement per-provider (ADR 048 §5.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    System,
    /// Alias of [`Self::System`] — backward-compat.
    Raw,
    Prompt,
    UserPrepend,
    UserAppend,
    /// Alias of [`Self::UserAppend`] — backward-compat.
    User,
    UserNew,
    AssistantPrefill,
    /// Experimental, NOT model-visible — Anthropic does not surface
    /// request metadata to the model.
    Metadata,
}

impl Placement {
    /// Canonical string form (after alias collapsing). Useful for
    /// audit records and logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System | Self::Raw => "system",
            Self::Prompt => "prompt",
            Self::UserPrepend => "user_prepend",
            Self::UserAppend | Self::User => "user_append",
            Self::UserNew => "user_new",
            Self::AssistantPrefill => "assistant_prefill",
            Self::Metadata => "metadata",
        }
    }
}

/// `[context.discovery]` — marker namespace + format.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    /// Marker prefix; markers take the form
    /// `<namespace:NAME>VALUE</namespace:NAME>`. Defaults to
    /// `"noodle"`. Must be a bare XML tag-name fragment (no `:`,
    /// whitespace, `<>&/"'=`).
    #[serde(default = "default_namespace")]
    pub namespace: String,
    /// Marker syntax. Only `xml` is supported in v1.
    #[serde(default)]
    pub format: MarkerFormat,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            namespace: default_namespace(),
            format: MarkerFormat::default(),
        }
    }
}

impl Discovery {
    fn validate(&self) -> Result<(), String> {
        const ILLEGAL: &[char] = &[':', '<', '>', '&', '/', '"', '\'', '='];
        if self.namespace.trim().is_empty() {
            return Err("context.discovery.namespace is empty".into());
        }
        for c in self.namespace.chars() {
            if c.is_whitespace() || ILLEGAL.contains(&c) {
                return Err(format!(
                    "context.discovery.namespace contains illegal character {c:?}"
                ));
            }
        }
        Ok(())
    }
}

fn default_namespace() -> String {
    "noodle".into()
}

/// Marker syntax. Only `xml` in v1; the loader rejects anything
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerFormat {
    #[default]
    Xml,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_tag_names_preserves_first_seen_order() {
        let cfg = ContextConfig {
            enabled: true,
            enhancements: vec![
                Enhancement {
                    r#as: Placement::UserPrepend,
                    text: "x".into(),
                    tags: vec![
                        Tag {
                            name: "work_type".into(),
                            default: "unknown".into(),
                        },
                        Tag {
                            name: "project".into(),
                            default: "unknown".into(),
                        },
                    ],
                },
                Enhancement {
                    r#as: Placement::System,
                    text: "y".into(),
                    tags: vec![
                        Tag {
                            name: "project".into(), // duplicate, must dedupe
                            default: "unknown".into(),
                        },
                        Tag {
                            name: "issue".into(),
                            default: "unknown".into(),
                        },
                    ],
                },
            ],
            discovery: Discovery::default(),
        };
        assert_eq!(
            cfg.declared_tag_names(),
            vec!["work_type", "project", "issue"]
        );
    }

    #[test]
    fn placement_aliases_collapse_to_canonical_str() {
        assert_eq!(Placement::Raw.as_str(), "system");
        assert_eq!(Placement::System.as_str(), "system");
        assert_eq!(Placement::User.as_str(), "user_append");
        assert_eq!(Placement::UserAppend.as_str(), "user_append");
    }

    #[test]
    fn disabled_with_empty_enhancements_is_valid() {
        let cfg = ContextConfig {
            enabled: false,
            enhancements: vec![],
            discovery: Discovery::default(),
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn enabled_with_empty_enhancements_is_invalid() {
        let cfg = ContextConfig {
            enabled: true,
            enhancements: vec![],
            discovery: Discovery::default(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_text_rejected() {
        let cfg = ContextConfig {
            enabled: true,
            enhancements: vec![Enhancement {
                r#as: Placement::System,
                text: "  \n  ".into(),
                tags: vec![],
            }],
            discovery: Discovery::default(),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("text"));
    }

    #[test]
    fn empty_tag_name_rejected() {
        let cfg = ContextConfig {
            enabled: true,
            enhancements: vec![Enhancement {
                r#as: Placement::System,
                text: "x".into(),
                tags: vec![Tag {
                    name: String::new(),
                    default: "unknown".into(),
                }],
            }],
            discovery: Discovery::default(),
        };
        assert!(cfg.validate().unwrap_err().contains("tags[0].name"));
    }

    #[test]
    fn illegal_namespace_rejected() {
        for bad in [":colons", "spa ces", "lt<", "amp&", "quote\""] {
            let cfg = ContextConfig {
                enabled: true,
                enhancements: vec![Enhancement {
                    r#as: Placement::System,
                    text: "x".into(),
                    tags: vec![],
                }],
                discovery: Discovery {
                    namespace: bad.into(),
                    format: MarkerFormat::Xml,
                },
            };
            let err = cfg
                .validate()
                .unwrap_err_or_else_msg(|| format!("namespace {bad:?} must be rejected"));
            assert!(err.contains("namespace"), "namespace {bad:?} got {err}");
        }
    }

    trait ResultExt<T, E> {
        fn unwrap_err_or_else_msg(self, msg: impl FnOnce() -> String) -> E;
    }
    impl<T, E> ResultExt<T, E> for Result<T, E> {
        fn unwrap_err_or_else_msg(self, msg: impl FnOnce() -> String) -> E {
            match self {
                Err(e) => e,
                Ok(_) => panic!("{}", msg()),
            }
        }
    }

    #[test]
    fn empty_namespace_rejected() {
        let cfg = ContextConfig {
            enabled: true,
            enhancements: vec![Enhancement {
                r#as: Placement::System,
                text: "x".into(),
                tags: vec![],
            }],
            discovery: Discovery {
                namespace: "  ".into(),
                format: MarkerFormat::Xml,
            },
        };
        assert!(cfg.validate().unwrap_err().contains("namespace"));
    }

    #[test]
    fn tag_default_defaults_to_unknown_when_omitted() {
        let toml = r#"
enabled = true

[[enhancements]]
text = "x"

[[enhancements.tags]]
name = "work_type"

[discovery]
"#;
        let cfg: ContextConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.enhancements[0].tags[0].default, "unknown");
    }
}
