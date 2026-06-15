//! `~/.noodle/noodle.toml` — one config file, sectional layout.
//!
//! `NoodleConfig` is the top-level struct deserialized from the TOML
//! file. Every section is optional so the file is incrementally
//! adoptable: a config with only `[context]` is valid; an
//! absent file is valid (everything falls back to compiled defaults).
//!
//! The loader (file read + path resolution) lives in `noodle-proxy`.
//! This module is pure data + validation so it stays portable per
//! ADR 039 (host-independent core).
//!
//! Section ownership:
//!
//! - `[context]` — ADR 048. The directive enhancement + marker
//!   extraction shape: which placement, which tags, which namespace.
//! - future: `[ca]`, `[shipper]`, `[proxy]`, `[viewer]` as those
//!   subsystems extract their hardcoded defaults.
//!
//! One file. No sprawl.

use serde::Deserialize;
use thiserror::Error;

pub mod context;

pub use context::{ContextConfig, Discovery, Enhancement, MarkerFormat, Placement, Tag};

/// Default config path: `$HOME/.noodle/noodle.toml`. Returns `None`
/// when `$HOME` is unset (CI sandboxes, containers) — caller decides
/// whether to substitute `./noodle.toml` or skip the load.
#[must_use]
pub fn default_config_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| {
        let mut p = std::path::PathBuf::from(home);
        p.push(".noodle");
        p.push("noodle.toml");
        p
    })
}

/// Whole `noodle.toml` shape. Every section is optional so the file
/// is grown one section at a time as subsystems opt in.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoodleConfig {
    /// ADR 048 — LLM self-classification configuration. `None` =
    /// section omitted from the TOML; the feature falls back to
    /// disabled (no enhancement, no extraction).
    pub context: Option<ContextConfig>,
}

impl NoodleConfig {
    /// Parse the TOML text into a validated config. Each section's
    /// `validate` method runs after deserialization; failures map to
    /// [`ConfigError::Validate`].
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Parse`] — TOML syntax error or unknown field.
    /// - [`ConfigError::Validate`] — a section's `validate` rejected
    ///   the loaded values.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Run every section's validation. Sections that are `None`
    /// (omitted) are skipped — that is always valid.
    ///
    /// # Errors
    ///
    /// Propagates the first section validation error.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ie) = &self.context {
            ie.validate().map_err(ConfigError::Validate)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid TOML: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Validate(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_default_config() {
        let cfg = NoodleConfig::from_toml_str("").unwrap();
        assert!(cfg.context.is_none());
    }

    #[test]
    fn enhance_extract_section_parses() {
        let toml = r#"
[context]
enabled = true

[[context.enhancements]]
as = "user_prepend"
text = "directive body"

[[context.enhancements.tags]]
name = "work_type"
default = "unknown"

[context.discovery]
namespace = "noodle"
format = "xml"
"#;
        let cfg = NoodleConfig::from_toml_str(toml).unwrap();
        let ie = cfg.context.unwrap();
        assert!(ie.enabled);
        assert_eq!(ie.enhancements.len(), 1);
        assert_eq!(ie.enhancements[0].tags.len(), 1);
        assert_eq!(ie.enhancements[0].tags[0].name, "work_type");
        assert_eq!(ie.discovery.namespace, "noodle");
    }

    #[test]
    fn unknown_top_level_section_rejected() {
        // `deny_unknown_fields` on NoodleConfig: typos at the
        // section level fail fast rather than silently no-op.
        let err = NoodleConfig::from_toml_str("[noodle_extract]\nenabled = true\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn validation_failure_propagates() {
        // Empty enhancements list when enabled is rejected by the
        // section validator — surfaced as Validate, not Parse.
        let toml = r"
[context]
enabled = true
enhancements = []

[context.discovery]
";
        let err = NoodleConfig::from_toml_str(toml).unwrap_err();
        assert!(matches!(err, ConfigError::Validate(_)), "got {err:?}");
    }
}
