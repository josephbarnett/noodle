//! External-CA configuration glue (ADR 034 §2.4 / §7, S19).
//!
//! [`ExternalCaConfig`] maps the `[ca.external]` TOML block from
//! ADR 034 §7 onto an `Arc<dyn DynCertMintService>` ready to
//! hand to [`crate::ProxyConfig::external_signer`].
//!
//! ## Supported backends
//!
//! v1 ships the **Vault PKI** backend. AWS ACM PCA, Azure Key
//! Vault, SCEP/EST, and a custom webhook adapter are open work
//! per ADR 034 §2.4 — they'd add new arms to
//! [`ExternalBackend`] without changing this module's surface.
//!
//! ## Environment-variable surface
//!
//! For the binary entry point, [`ExternalCaConfig::from_env`]
//! reads the same TOML-shaped values from `NOODLE_*` env vars
//! so a single config can live in either form (TOML for ops,
//! env for container deployments).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use noodle_cert_external::ExternalCertMintService;
use noodle_cert_external::vault::{VaultBuildError, VaultPkiSigner};
use noodle_core::DynCertMintService;
use thiserror::Error;

/// Backend selector under `[ca.external]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalBackend {
    /// `HashiCorp` Vault PKI (ADR 034 §2.4, S19).
    VaultPki,
}

/// Auth selector under `[ca.external]`.
#[derive(Debug, Clone)]
pub enum ExternalAuth {
    /// Bearer-token auth. `token_path` points at a file
    /// containing the Vault token; must be 0600 on Unix.
    Token { token_path: PathBuf },
    /// mTLS auth. `client_cert` is the PEM client cert,
    /// `client_key` is the PEM private key. Key file must be
    /// 0600 on Unix.
    Mtls {
        client_cert: PathBuf,
        client_key: PathBuf,
    },
}

/// Materialised form of the `[ca.external]` TOML block (ADR 034
/// §7).
///
/// Construct directly for in-process tests; build from process
/// env via [`ExternalCaConfig::from_env`] for the binary path.
#[derive(Debug, Clone)]
pub struct ExternalCaConfig {
    /// Which backend to use. v1 has one option.
    pub backend: ExternalBackend,
    /// Endpoint URL the backend POSTs CSRs to (e.g.
    /// `https://vault.corp/v1/pki/sign/noodle-leaf`).
    pub endpoint: String,
    /// Auth credentials.
    pub auth: ExternalAuth,
    /// Optional PEM CA bundle used to verify the signer's TLS
    /// (e.g. enterprise CA for the Vault server's TLS cert).
    pub ca_cert_path: Option<PathBuf>,
    /// Per-mint timeout (ADR 034 §3.3). Defaults to 2s.
    pub signer_timeout: Duration,
    /// When `true`, the proxy pre-warms the leaf cache at
    /// startup for [`procurement_hosts`]. ADR 034 §2.5.
    pub procurement_on_startup: bool,
    /// Hosts to pre-warm. Empty when procurement is disabled.
    pub procurement_hosts: Vec<String>,
}

/// Errors raised while building the mint service from the
/// config block.
#[derive(Debug, Error)]
pub enum ExternalConfigError {
    /// The backend-specific signer rejected the supplied
    /// credentials / files / endpoint.
    #[error("backend init failed: {0}")]
    Backend(#[from] VaultBuildError),
}

impl ExternalCaConfig {
    /// Build the mint service for this config.
    ///
    /// The returned `Arc<dyn DynCertMintService>` is plumbed
    /// into [`crate::ProxyConfig::external_signer`].
    pub fn build_mint_service(&self) -> Result<Arc<dyn DynCertMintService>, ExternalConfigError> {
        match self.backend {
            ExternalBackend::VaultPki => {
                let ca_pem: Option<Vec<u8>> = match &self.ca_cert_path {
                    Some(p) => Some(std::fs::read(p).map_err(|source| VaultBuildError::Io {
                        path: p.clone(),
                        source,
                    })?),
                    None => None,
                };
                let signer = match &self.auth {
                    ExternalAuth::Token { token_path } => VaultPkiSigner::with_token(
                        self.endpoint.clone(),
                        token_path,
                        ca_pem.as_deref(),
                        Some(self.signer_timeout),
                    )?,
                    ExternalAuth::Mtls {
                        client_cert,
                        client_key,
                    } => VaultPkiSigner::with_mtls(
                        self.endpoint.clone(),
                        client_cert,
                        client_key,
                        ca_pem.as_deref(),
                        Some(self.signer_timeout),
                    )?,
                };
                let svc =
                    ExternalCertMintService::with_timeout(Arc::new(signer), self.signer_timeout);
                Ok(Arc::new(svc) as Arc<dyn DynCertMintService>)
            }
        }
    }

    /// Build an [`ExternalCaConfig`] from process environment.
    ///
    /// Recognised vars (matching ADR 034 §7's TOML):
    ///
    /// - `NOODLE_CA_EXTERNAL_BACKEND` — `"vault-pki"` (default
    ///   if unset and external mode is requested).
    /// - `NOODLE_CA_EXTERNAL_ENDPOINT` — required.
    /// - `NOODLE_CA_EXTERNAL_AUTH` — `"token"` (default) or
    ///   `"mtls"`.
    /// - `NOODLE_CA_EXTERNAL_TOKEN_PATH` — required when
    ///   `auth=token`.
    /// - `NOODLE_CA_EXTERNAL_CLIENT_CERT` /
    ///   `NOODLE_CA_EXTERNAL_CLIENT_KEY` — required when
    ///   `auth=mtls`.
    /// - `NOODLE_CA_EXTERNAL_CA_CERT` — optional PEM bundle.
    /// - `NOODLE_CA_EXTERNAL_SIGNER_TIMEOUT_MS` — optional;
    ///   default 2000.
    /// - `NOODLE_CA_EXTERNAL_PROCUREMENT_ON_STARTUP` —
    ///   `"true"` / `"false"`; default `true`.
    /// - `NOODLE_CA_EXTERNAL_PROCUREMENT_HOSTS` — comma-
    ///   separated host list. Empty / unset disables
    ///   procurement.
    pub fn from_env() -> Result<Self, String> {
        let backend = match std::env::var("NOODLE_CA_EXTERNAL_BACKEND")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            None | Some("" | "vault-pki") => ExternalBackend::VaultPki,
            Some(other) => {
                return Err(format!(
                    "unknown NOODLE_CA_EXTERNAL_BACKEND={other:?}; expected \"vault-pki\""
                ));
            }
        };
        let endpoint = std::env::var("NOODLE_CA_EXTERNAL_ENDPOINT")
            .map_err(|_| "NOODLE_CA_EXTERNAL_ENDPOINT must be set".to_string())?;
        let auth_kind = std::env::var("NOODLE_CA_EXTERNAL_AUTH")
            .ok()
            .unwrap_or_else(|| "token".to_string());
        let auth = match auth_kind.as_str() {
            "token" => {
                let p = std::env::var("NOODLE_CA_EXTERNAL_TOKEN_PATH")
                    .map_err(|_| "NOODLE_CA_EXTERNAL_TOKEN_PATH required with auth=token")?;
                ExternalAuth::Token {
                    token_path: PathBuf::from(p),
                }
            }
            "mtls" => {
                let cert = std::env::var("NOODLE_CA_EXTERNAL_CLIENT_CERT")
                    .map_err(|_| "NOODLE_CA_EXTERNAL_CLIENT_CERT required with auth=mtls")?;
                let key = std::env::var("NOODLE_CA_EXTERNAL_CLIENT_KEY")
                    .map_err(|_| "NOODLE_CA_EXTERNAL_CLIENT_KEY required with auth=mtls")?;
                ExternalAuth::Mtls {
                    client_cert: PathBuf::from(cert),
                    client_key: PathBuf::from(key),
                }
            }
            other => {
                return Err(format!(
                    "unknown NOODLE_CA_EXTERNAL_AUTH={other:?}; expected \"token\" or \"mtls\""
                ));
            }
        };
        let ca_cert_path = std::env::var_os("NOODLE_CA_EXTERNAL_CA_CERT").map(PathBuf::from);
        let signer_timeout = std::env::var("NOODLE_CA_EXTERNAL_SIGNER_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(2), Duration::from_millis);
        let procurement_on_startup =
            match std::env::var("NOODLE_CA_EXTERNAL_PROCUREMENT_ON_STARTUP")
                .ok()
                .as_deref()
                .map(str::trim)
            {
                None => true,
                Some(s) => !matches!(s.to_ascii_lowercase().as_str(), "false" | "0" | "no" | ""),
            };
        let procurement_hosts: Vec<String> = std::env::var("NOODLE_CA_EXTERNAL_PROCUREMENT_HOSTS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            backend,
            endpoint,
            auth,
            ca_cert_path,
            signer_timeout,
            procurement_on_startup,
            procurement_hosts,
        })
    }
}

/// Permissions check helper exposed for tests + the binary.
///
/// Mirrors the policy in [`noodle_tls::ca::Ca::load_static`]:
/// Unix files holding secrets must be `0600` or stricter.
pub fn check_secure_permissions(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "{} has mode {mode:o}; required 0600 or stricter (operator: chmod 0600)",
                    path.display()
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_ca_config_round_trip_token() {
        let cfg = ExternalCaConfig {
            backend: ExternalBackend::VaultPki,
            endpoint: "https://vault.example/v1/pki/sign/r".to_string(),
            auth: ExternalAuth::Token {
                token_path: PathBuf::from("/tmp/no-such-file"),
            },
            ca_cert_path: None,
            signer_timeout: Duration::from_secs(2),
            procurement_on_startup: true,
            procurement_hosts: vec!["api.anthropic.com".to_string()],
        };
        // Build will fail because the token file doesn't exist —
        // we just verify the shape parses + we get the expected
        // error variant.
        match cfg.build_mint_service() {
            Err(ExternalConfigError::Backend(VaultBuildError::Io { .. })) => {}
            Err(other) => panic!("expected Backend(Io); got {other:?}"),
            Ok(_) => panic!("build_mint_service should fail for missing token file"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_secure_permissions_rejects_0644() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_secure_permissions(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn check_secure_permissions_accepts_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret");
        std::fs::write(&path, "x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        check_secure_permissions(&path).expect("0600 acceptable");
    }
}
