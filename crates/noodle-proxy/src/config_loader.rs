//! `~/.noodle/noodle.toml` loader — the host-side I/O layer for the
//! pure types in `noodle_core::config`.
//!
//! Precedence:
//!
//! 1. The path the binary's caller explicitly supplies (e.g. via
//!    `--config <path>` in `main.rs`, when wired) — explicit beats
//!    everything.
//! 2. `~/.noodle/noodle.toml` if it exists.
//! 3. The shipped default config, embedded at compile time via
//!    `include_str!("../default-noodle.toml")`. **This is also the
//!    one place where the default tag set lives — edit
//!    `default-noodle.toml`, not a Rust array literal.**
//!
//! ADR 048 §11 item 1.

use noodle_core::config::{ConfigError, NoodleConfig, default_config_path};
use std::path::PathBuf;
use thiserror::Error;

/// The shipped default config text, embedded at compile time. The
/// noodle.toml schema is the source of truth for the default tag
/// set; no array literals in Rust code.
const DEFAULT_CONFIG_TEXT: &str = include_str!("../default-noodle.toml");

/// Resolved + parsed config plus a note on where it came from. The
/// `source` field is for startup logs / debug; nothing on the hot
/// path branches on it.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: NoodleConfig,
    pub source: ConfigSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded from a path the caller explicitly supplied.
    Explicit(PathBuf),
    /// Loaded from `~/.noodle/noodle.toml`.
    UserDefault(PathBuf),
    /// Embedded `default-noodle.toml` — either `$HOME` was unset, or
    /// the file did not exist at the resolved path.
    EmbeddedDefault,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Parse { path: PathBuf, source: ConfigError },
    #[error("embedded default config: {0}")]
    EmbeddedDefault(ConfigError),
}

/// Load + parse the config per the precedence rules in the module
/// docs. The embedded default is always available as the final
/// fallback, so this only errors when:
///
/// - An explicit `path` was supplied but couldn't be read or parsed.
/// - The embedded default itself failed to parse (compile-time bug —
///   the unit tests catch this).
pub fn load(explicit: Option<&std::path::Path>) -> Result<LoadedConfig, LoadError> {
    load_with(explicit, default_config_path().as_deref())
}

/// Internal seam — same as [`load`] but takes the user-default path
/// explicitly so tests don't need to mutate `$HOME` to exercise the
/// "no user file present" fallback. The public API hardcodes
/// `default_config_path()` as the lookup.
pub(crate) fn load_with(
    explicit: Option<&std::path::Path>,
    user_default: Option<&std::path::Path>,
) -> Result<LoadedConfig, LoadError> {
    if let Some(p) = explicit {
        return read_required(p.to_owned()).map(|c| LoadedConfig {
            config: c,
            source: ConfigSource::Explicit(p.to_owned()),
        });
    }

    if let Some(p) = user_default
        && p.exists()
    {
        return read_required(p.to_owned()).map(|c| LoadedConfig {
            config: c,
            source: ConfigSource::UserDefault(p.to_owned()),
        });
    }

    let cfg =
        NoodleConfig::from_toml_str(DEFAULT_CONFIG_TEXT).map_err(LoadError::EmbeddedDefault)?;
    Ok(LoadedConfig {
        config: cfg,
        source: ConfigSource::EmbeddedDefault,
    })
}

fn read_required(path: PathBuf) -> Result<NoodleConfig, LoadError> {
    let text = std::fs::read_to_string(&path).map_err(|e| LoadError::Io {
        path: path.clone(),
        source: e,
    })?;
    NoodleConfig::from_toml_str(&text).map_err(|source| LoadError::Parse { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn embedded_default_parses_and_validates() {
        // Compile-time guard: if someone edits default-noodle.toml
        // into something invalid, this fires before a release.
        let cfg = NoodleConfig::from_toml_str(DEFAULT_CONFIG_TEXT)
            .expect("shipped default-noodle.toml must parse + validate");
        let ie = cfg.context.expect("default ships with context");
        assert!(ie.enabled);
        // The shipped tag set is what shows up at the wire. Locking
        // it down here means changing the default is a deliberate
        // edit to the TOML AND this test.
        assert_eq!(
            ie.declared_tag_names(),
            vec![
                "work_type",
                "project",
                "repo",
                "branch",
                "issue",
                "customer"
            ]
        );
    }

    #[test]
    fn missing_file_falls_back_to_embedded() {
        let tmp = TempDir::new().unwrap();
        let non_existent = tmp.path().join("does-not-exist.toml");
        let loaded = load_with(None, Some(&non_existent)).unwrap();
        assert!(matches!(loaded.source, ConfigSource::EmbeddedDefault));
        assert!(loaded.config.context.unwrap().enabled);
    }

    #[test]
    fn user_default_file_loads_when_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("noodle.toml");
        std::fs::write(&path, "[context]\nenabled = false\n").unwrap();
        let loaded = load_with(None, Some(&path)).unwrap();
        assert!(matches!(loaded.source, ConfigSource::UserDefault(_)));
        assert!(!loaded.config.context.unwrap().enabled);
    }

    #[test]
    fn explicit_path_overrides_user_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("custom.toml");
        std::fs::write(&path, "[context]\nenabled = false\n").unwrap();
        let loaded = load(Some(&path)).unwrap();
        assert!(matches!(loaded.source, ConfigSource::Explicit(_)));
        assert!(!loaded.config.context.unwrap().enabled);
    }

    #[test]
    fn invalid_explicit_config_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.toml");
        std::fs::write(
            &path,
            "[context]\nenabled = true\nenhancements = []\n[context.discovery]\n",
        )
        .unwrap();
        let err = load(Some(&path)).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }), "got {err:?}");
    }
}
