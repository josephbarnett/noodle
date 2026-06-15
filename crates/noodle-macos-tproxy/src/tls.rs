//! TLS MITM setup for the macOS system extension.
//!
//! Generates a self-signed root CA at sysext startup, persists the
//! cert PEM to a known on-disk location so HTTPS clients with their
//! own trust stores (Node.js via `NODE_EXTRA_CA_CERTS`, Python via
//! `REQUESTS_CA_BUNDLE`, etc.) can be pointed at it, and wraps the
//! key pair in [`TlsMitmRelay`] for the intercept service.
//!
//! ## On-disk CA layout (iteration 3b interlude)
//!
//! The sysext writes its CA cert to
//! `/Library/Application Support/noodle/macos-tproxy-ca.pem`.
//!
//! `NETransparentProxyProvider` system extensions run as **root**,
//! and the `storage_dir` the rama Swift bindings pass at init
//! resolves to `/var/root/Library/Application Support/rama/tproxy/`
//! — root's home, which is mode `0700`. Files written there can't
//! be read by the user without `sudo`, defeating the whole point of
//! exposing the CA path for `NODE_EXTRA_CA_CERTS`-style env vars.
//!
//! Instead we write to the system-wide `/Library/Application Support/`
//! tree, which the sysext (as root) can create + write, and which
//! defaults to world-readable (`0755` parent, `0644` file). We also
//! explicitly chmod the cert file `0644` so any future tightening of
//! umask doesn't break user-side `cat`.
//!
//! Iteration 5 persists the CA to the System Keychain and wires
//! `OSSystemExtensionRequest`-style installation through the
//! container app's existing "Install CA" menu route.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use rama::{
    error::BoxError,
    net::{address::Domain, tls::server::SelfSignedData},
    tls::boring::{
        proxy::{
            TlsMitmRelay,
            cert_issuer::{CachedBoringMitmCertIssuer, InMemoryBoringMitmCertIssuer},
        },
        server::utils::self_signed_server_auth_gen_ca,
    },
};

/// Concrete type alias for the in-memory + cached MITM relay we use
/// in the sysext.
pub type NoodleMitmRelay = TlsMitmRelay<CachedBoringMitmCertIssuer<InMemoryBoringMitmCertIssuer>>;

/// World-readable path the sysext (running as root) writes its
/// root CA cert PEM to. Hardcoded because the rama-provided
/// `storage_dir` lives under `/var/root` (mode 0700) and is
/// therefore not readable by the user who needs to point
/// `NODE_EXTRA_CA_CERTS` at it.
pub const CA_PEM_PATH: &str = "/Library/Application Support/noodle/macos-tproxy-ca.pem";

/// Result of [`build_mitm_relay`]: the relay plus the on-disk path
/// where its CA cert was written (or `None` if the write failed).
pub struct MitmSetup {
    pub relay: NoodleMitmRelay,
    pub ca_path: Option<PathBuf>,
}

/// Generate a fresh self-signed root CA, persist the cert PEM to
/// the world-readable system path defined by [`CA_PEM_PATH`], and
/// build the cached MITM relay from the key pair.
///
/// The private key never leaves memory — only the public cert PEM
/// is written to disk, which is what HTTPS clients need to trust
/// the leaves the sysext mints. Iteration 5 may write the key to
/// the System Keychain so the CA survives restarts.
///
/// The `_storage_dir` parameter is accepted for forward-compatibility
/// (iteration 5 may use it for the keychain-backed CA storage) but
/// is currently ignored — see module-level comment for the
/// `/var/root` mode-0700 problem.
pub fn build_mitm_relay(_storage_dir: Option<&Path>) -> Result<MitmSetup, BoxError> {
    // CN must parse as a DNS-style domain (rama's Domain enforces
    // this and panics on spaces). Use a DNS-like CN.
    let data = SelfSignedData {
        organisation_name: Some("noodle MITM root CA".into()),
        common_name: Some(Domain::from_static("ca.noodleproxy.macos")),
        subject_alternative_names: None,
        ..Default::default()
    };

    let (ca_cert, ca_key) = self_signed_server_auth_gen_ca(&data)?;

    let ca_path = match write_ca_pem(Path::new(CA_PEM_PATH), &ca_cert) {
        Ok(path) => {
            tracing::info!(
                path = %path.display(),
                "wrote noodle root CA cert to disk; set NODE_EXTRA_CA_CERTS / REQUESTS_CA_BUNDLE / SSL_CERT_FILE to this path"
            );
            Some(path)
        }
        Err(err) => {
            tracing::warn!(
                path = CA_PEM_PATH,
                error = %err,
                "failed to write noodle CA cert to disk — CA will only exist in memory (use curl --insecure to test)"
            );
            None
        }
    };

    let issuer = InMemoryBoringMitmCertIssuer::new(ca_cert, ca_key);
    let relay = TlsMitmRelay::new_with_cached_issuer(issuer);

    Ok(MitmSetup { relay, ca_path })
}

fn write_ca_pem(
    target: &Path,
    cert: &rama::tls::boring::core::x509::X509,
) -> Result<PathBuf, BoxError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
        // World-readable directory so non-root users can traverse
        // into it; root-owned is fine, traverse permission is what
        // matters.
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }
    let pem = cert.to_pem()?;
    fs::write(target, &pem)?;
    // World-readable file so `cat $(make macos-ca-path)` works
    // without sudo, and so non-root processes pointed at this path
    // via `NODE_EXTRA_CA_CERTS` etc. can actually open it.
    fs::set_permissions(target, fs::Permissions::from_mode(0o644))?;
    Ok(target.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercises the same write path as production but against a
    /// tempdir target so the test can run without root.
    fn write_to_tempdir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let target = tmp.path().join("noodle-ca.pem");
        let data = SelfSignedData {
            organisation_name: Some("noodle MITM root CA".into()),
            common_name: Some(Domain::from_static("ca.noodleproxy.macos")),
            subject_alternative_names: None,
            ..Default::default()
        };
        let (cert, _key) = self_signed_server_auth_gen_ca(&data).expect("gen ca");
        let path = write_ca_pem(&target, &cert).expect("write pem");
        (tmp, path)
    }

    #[test]
    fn relay_constructs_via_public_api() {
        // `build_mitm_relay` writes to the hardcoded production path;
        // when not running as root that write fails and `ca_path`
        // returns `None`, but the relay itself still constructs.
        let setup = build_mitm_relay(None).expect("self-signed CA generation succeeds");
        let _clone = setup.relay.clone();
    }

    #[test]
    fn write_ca_pem_creates_dir_and_writes_valid_pem() {
        let (_tmp, path) = write_to_tempdir();
        assert!(path.exists(), "PEM file written");
        let pem = fs::read_to_string(&path).expect("read pem");
        assert!(
            pem.starts_with("-----BEGIN CERTIFICATE-----"),
            "valid PEM header"
        );
        assert!(
            pem.trim_end().ends_with("-----END CERTIFICATE-----"),
            "valid PEM footer"
        );
    }

    #[test]
    fn write_ca_pem_chmods_file_to_world_readable() {
        let (_tmp, path) = write_to_tempdir();
        let mode = fs::metadata(&path).expect("stat pem").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "file should be world-readable");
    }
}
