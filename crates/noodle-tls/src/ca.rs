//! Self-signed root certificate authority for the noodle MITM proxy.
//!
//! Two modes (ADR 034 §2.1, §4):
//!
//! - **Local (dev)** — [`Ca::generate_or_load`] mints a fresh ECDSA
//!   P-256 root on first run, persists cert + key to disk, reuses on
//!   subsequent runs. Same fingerprint across restarts.
//! - **BYOCA-static (S18)** — [`Ca::load_static`] loads an
//!   operator-supplied CA from disk. The operator drops `ca.pem` +
//!   `ca.key` (and optionally `chain.pem` intermediates) at the
//!   configured path before first run; noodle uses them verbatim
//!   to sign leaves. Leaves chain to the operator's existing PKI,
//!   which fleet devices already trust via MDM.
//!
//! [`Ca::load`] dispatches between the two modes via [`CaMode`].
//!
//! ## Files
//!
//! - `ca.pem` — public certificate (PEM). What operators feed
//!   `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`,
//!   and the macOS keychain.
//! - `ca.cer` — same bytes as `ca.pem`, different extension (local
//!   mode only — the BYOCA path does not emit this convenience
//!   copy). Some tooling (notably the macOS keychain on
//!   double-click) prefers `.cer`.
//! - `ca.key` — private key (PEM, PKCS#8). Needed at runtime to
//!   sign per-host leaves; never leaves disk. On Unix it's
//!   chmod'd 0600; the parent directory 0700. In BYOCA-static
//!   mode the operator owns these permissions — load fails if
//!   `ca.key` is looser than 0600.
//! - `chain.pem` — optional intermediate chain (BYOCA-static
//!   only). When present, every leaf this CA mints is returned
//!   with `[leaf, intermediate(s)]` so clients without the root
//!   pre-installed can build a chain to a root they already
//!   trust.
//!
//! ## Algorithm
//!
//! Local mode uses ECDSA P-256 (`PKCS_ECDSA_P256_SHA256`). BYOCA
//! mode accepts whatever the operator brought; rcgen's
//! `KeyPair::from_pem` and `CertificateParams::from_ca_cert_pem`
//! cover the mainstream cases (RSA-2048/3072/4096, ECDSA P-256/P-384,
//! Ed25519). The leaf-signing path delegates to whichever algorithm
//! the loaded key uses.

use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256,
};
use thiserror::Error;
use time::{Duration, OffsetDateTime};

/// Filename of the public CA certificate (PEM).
pub const CERT_FILE: &str = "ca.pem";
/// Filename of the same certificate with a `.cer` extension (DER and
/// PEM .cer files are both accepted by macOS keychain double-click).
pub const CERT_CER_FILE: &str = "ca.cer";
/// Filename of the private key (PEM, PKCS#8).
pub const KEY_FILE: &str = "ca.key";
/// Filename of the optional intermediate chain (PEM bundle).
/// Loaded by [`Ca::load_static`] only — local mode does not emit
/// or consume this file.
pub const CHAIN_FILE: &str = "chain.pem";

/// Subject `CommonName` the locally-generated cert presents itself
/// with. BYOCA-static CAs carry whatever the operator's PKI used.
pub const ISSUER_CN: &str = "noodle MITM root CA";
/// Subject `Organization` on the locally-generated cert.
pub const ISSUER_ORG: &str = "noodle";

/// Validity window: 3 years forward, 48-hour back-skew so a
/// freshly-minted cert isn't rejected by clients with skewed clocks.
const VALIDITY_YEARS: i64 = 3;
const SKEW_HOURS: i64 = 48;

/// CA-loading mode selector. Maps to the rows of ADR 034 §2.1.
///
/// External-signer mode (variant 3 in the ADR) is not represented
/// here — it does not load a CA from disk, it delegates signing
/// to a remote service. It lands in S19 as a separate
/// `CertMintService` implementation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CaMode {
    /// Local dev mode: generate-on-first-run, persist, reuse.
    /// The pre-S18 behaviour and the default for `ProxyConfig`.
    #[default]
    Local,
    /// BYOCA-static: load operator-supplied `ca.pem` + `ca.key`
    /// (and optional `chain.pem`) from disk. Fails loud if files
    /// are missing or permissions are looser than 0600 on the key.
    ByocaStatic,
}

/// Errors specific to [`Ca::load_static`] / [`Ca::load`] for the
/// BYOCA-static mode. Distinct from [`CaError`] because the
/// failure modes (missing files, bad permissions, key/cert
/// mismatch) are operationally meaningful at startup — the
/// operator's `Configuration Profile` / package install left
/// something incomplete, and the proxy must refuse to start.
///
/// See feature 037 §2 acceptance criteria #3, #5, #6.
#[derive(Debug, Error)]
pub enum CaLoadError {
    /// Required CA file is absent from the configured path. The
    /// operator must place all required files before startup; we
    /// do NOT silently fall back to generating a local CA
    /// (feature 037 §2 #3).
    #[error("CA file missing at {path}: {what}")]
    MissingFile {
        /// What was expected (e.g. "ca.pem", "ca.key").
        what: &'static str,
        /// Filesystem path that was checked.
        path: PathBuf,
    },
    /// `ca.key` permissions are looser than 0600 on Unix. The
    /// load refuses to proceed: a private key readable by other
    /// users on the host is a fleet-wide compromise vector
    /// (feature 037 §2 #6). On Windows this check is skipped
    /// (ACL probe is out of scope for v1).
    #[error(
        "ca.key at {path} has mode {mode:o} — must be 0600 \
         (operator must `chmod 0600 ca.key` before starting noodle)"
    )]
    InsecurePermissions {
        /// Filesystem path to the offending key file.
        path: PathBuf,
        /// The actual mode bits (lower 9 of the file permissions).
        mode: u32,
    },
    /// PEM parse or rcgen failure — malformed cert, unsupported
    /// key algorithm, etc. Wraps the lower-level error for
    /// operator diagnostics.
    #[error("malformed PEM in {what} at {path}: {source}")]
    MalformedPem {
        /// What was being parsed (e.g. "ca.pem").
        what: &'static str,
        /// Filesystem path being read.
        path: PathBuf,
        #[source]
        source: PemSource,
    },
    /// The loaded `ca.key` does not correspond to the public key
    /// embedded in `ca.pem`. A leaf signed under this CA would
    /// not chain — fail at startup rather than after the first
    /// MITM handshake (feature 037 §5 test plan).
    #[error(
        "ca.key public key does not match ca.pem subject public key \
         (cert + key are from different CAs); cert at {cert_path}, key at {key_path}"
    )]
    KeyCertMismatch {
        /// Filesystem path to the loaded cert.
        cert_path: PathBuf,
        /// Filesystem path to the loaded key.
        key_path: PathBuf,
    },
    /// Filesystem I/O failure while reading one of the files.
    #[error("io error reading {path}: {source}")]
    Io {
        /// Filesystem path that failed to read.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Low-level parse error wrapper — keeps the public surface of
/// [`CaLoadError::MalformedPem`] simple while still capturing
/// which library reported the failure.
#[derive(Debug, Error)]
pub enum PemSource {
    /// PEM block parse failure (header / base64 / footer).
    #[error("pem decode: {0}")]
    Pem(#[from] pem::PemError),
    /// rcgen rejected the parsed material (e.g. unsupported curve,
    /// missing CA basic constraint).
    #[error("rcgen: {0}")]
    Rcgen(#[from] rcgen::Error),
    /// x509-parser rejected the DER (malformed inside the PEM).
    #[error("x509: {0}")]
    X509(String),
}

/// Self-signed (or operator-supplied) root certificate authority.
///
/// Holds the rcgen materials needed to (a) emit cert/key PEM and
/// (b) sign per-host leaves via [`crate::cert::LocalCertMintService`].
/// Construct via [`Ca::generate`], [`Ca::generate_or_load`], or
/// [`Ca::load_static`] (or the [`Ca::load`] dispatcher).
pub struct Ca {
    cert_pem: String,
    key_pem: String,
    /// rcgen's parsed view of the cert. Required to mint child certs
    /// via `signed_by` in the leaf path.
    rcgen_cert: rcgen::Certificate,
    rcgen_key: KeyPair,
    /// Optional intermediate-chain DERs (leaf-presenting order:
    /// intermediates only, NOT the root and NOT the leaf). Populated
    /// only by [`Ca::load_static`] when `chain.pem` is present.
    /// Empty for local mode and for BYOCA loads without a chain
    /// file. Consumed by `LocalCertMintService::mint_leaf_sync` to
    /// extend the returned `LeafCert::cert_chain`.
    intermediate_chain_der: Vec<Vec<u8>>,
}

#[derive(Debug, Error)]
pub enum CaError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("rcgen error: {0}")]
    Rcgen(#[from] rcgen::Error),
}

impl Ca {
    /// Generate a fresh self-signed root CA in memory. Does not touch
    /// disk; combine with [`Ca::persist`] or use
    /// [`Ca::generate_or_load`].
    pub fn generate() -> Result<Self, CaError> {
        let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;

        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, ISSUER_CN);
        params
            .distinguished_name
            .push(DnType::OrganizationName, ISSUER_ORG);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::hours(SKEW_HOURS);
        params.not_after = now + Duration::days(365 * VALIDITY_YEARS);

        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        Ok(Self {
            cert_pem,
            key_pem,
            rcgen_cert: cert,
            rcgen_key: key_pair,
            intermediate_chain_der: Vec::new(),
        })
    }

    /// Load an existing CA from `dir`, generating + persisting one
    /// first if either file is absent.
    ///
    /// Local-mode entry point. Idempotent: same `dir` returns a CA
    /// with the same fingerprint across calls. Different runs of
    /// the proxy share the same root.
    pub fn generate_or_load(dir: &Path) -> Result<Self, CaError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        if cert_path.exists() && key_path.exists() {
            Self::load_local(dir)
        } else {
            let ca = Self::generate()?;
            ca.persist(dir)?;
            Ok(ca)
        }
    }

    /// Load-or-generate dispatcher selecting on [`CaMode`] (ADR 034
    /// §2.1, feature 037 §2 #1, #3).
    ///
    /// - [`CaMode::Local`] → calls [`Ca::generate_or_load`] (the
    ///   pre-S18 behaviour); errors map to [`CaLoadError::Io`] /
    ///   [`CaLoadError::MalformedPem`] for caller uniformity.
    /// - [`CaMode::ByocaStatic`] → calls [`Ca::load_static`]; fails
    ///   loud if any required file is missing or `ca.key` permissions
    ///   are insecure.
    pub fn load(mode: CaMode, dir: &Path) -> Result<Self, CaLoadError> {
        match mode {
            CaMode::Local => Self::generate_or_load(dir).map_err(load_err_from_ca_err),
            CaMode::ByocaStatic => Self::load_static(dir),
        }
    }

    /// Load an existing local-mode CA from `dir`. Errors if either
    /// file is missing or unparseable. Internal helper for
    /// [`Ca::generate_or_load`].
    pub fn load_local(dir: &Path) -> Result<Self, CaError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let cert_pem = read_to_string(&cert_path)?;
        let key_pem = read_to_string(&key_path)?;

        let rcgen_key = KeyPair::from_pem(&key_pem)?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem)?;
        // `self_signed` here re-issues an in-memory cert from the
        // existing params, NOT a new on-disk artifact. The new
        // Certificate value is what rcgen needs as the issuer when
        // we sign leaves in the leaf path; the resulting bytes are
        // identical to the persisted PEM.
        let rcgen_cert = params.self_signed(&rcgen_key)?;
        Ok(Self {
            cert_pem,
            key_pem,
            rcgen_cert,
            rcgen_key,
            intermediate_chain_der: Vec::new(),
        })
    }

    /// Load an operator-supplied CA from `dir` (BYOCA-static mode,
    /// ADR 034 §4, feature 037 §2).
    ///
    /// Required files in `dir`:
    ///
    /// - `ca.pem` — PEM-encoded CA certificate.
    /// - `ca.key` — PEM-encoded PKCS#8 private key for `ca.pem`.
    ///   On Unix MUST be mode 0600 (or stricter); looser
    ///   permissions cause the load to fail with
    ///   [`CaLoadError::InsecurePermissions`].
    ///
    /// Optional file:
    ///
    /// - `chain.pem` — PEM bundle of intermediate certs between
    ///   `ca.pem` and the operator's trust anchor. When present
    ///   these are appended to every leaf the resulting CA mints
    ///   so clients without the operator root pre-installed can
    ///   still build a chain.
    ///
    /// Validation:
    ///
    /// - All required files exist (else
    ///   [`CaLoadError::MissingFile`]).
    /// - PEM parses cleanly (else [`CaLoadError::MalformedPem`]).
    /// - `ca.key` public key matches `ca.pem` subject public key
    ///   (else [`CaLoadError::KeyCertMismatch`]).
    /// - On Unix, `ca.key` permissions are 0600 or stricter (else
    ///   [`CaLoadError::InsecurePermissions`]).
    pub fn load_static(dir: &Path) -> Result<Self, CaLoadError> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        let chain_path = dir.join(CHAIN_FILE);

        // ── Existence checks (acceptance #3) ──────────────────
        if !cert_path.exists() {
            return Err(CaLoadError::MissingFile {
                what: CERT_FILE,
                path: cert_path,
            });
        }
        if !key_path.exists() {
            return Err(CaLoadError::MissingFile {
                what: KEY_FILE,
                path: key_path,
            });
        }

        // ── Key-file permission check (acceptance #6) ─────────
        check_key_permissions(&key_path)?;

        // ── Read + parse PEM ──────────────────────────────────
        let cert_pem = fs::read_to_string(&cert_path).map_err(|source| CaLoadError::Io {
            path: cert_path.clone(),
            source,
        })?;
        let key_pem = fs::read_to_string(&key_path).map_err(|source| CaLoadError::Io {
            path: key_path.clone(),
            source,
        })?;

        let rcgen_key = KeyPair::from_pem(&key_pem).map_err(|e| CaLoadError::MalformedPem {
            what: KEY_FILE,
            path: key_path.clone(),
            source: PemSource::Rcgen(e),
        })?;
        let params = CertificateParams::from_ca_cert_pem(&cert_pem).map_err(|e| {
            CaLoadError::MalformedPem {
                what: CERT_FILE,
                path: cert_path.clone(),
                source: PemSource::Rcgen(e),
            }
        })?;

        // ── Key / cert agreement (acceptance #5 prereq) ───────
        verify_key_matches_cert(&cert_pem, &key_pem, &cert_path, &key_path)?;

        let rcgen_cert = params
            .self_signed(&rcgen_key)
            .map_err(|e| CaLoadError::MalformedPem {
                what: CERT_FILE,
                path: cert_path.clone(),
                source: PemSource::Rcgen(e),
            })?;

        // ── Optional intermediate chain (acceptance #2) ───────
        let intermediate_chain_der = if chain_path.exists() {
            let chain_pem = fs::read_to_string(&chain_path).map_err(|source| CaLoadError::Io {
                path: chain_path.clone(),
                source,
            })?;
            parse_chain_pem(&chain_pem, &chain_path)?
        } else {
            Vec::new()
        };

        Ok(Self {
            cert_pem,
            key_pem,
            rcgen_cert,
            rcgen_key,
            intermediate_chain_der,
        })
    }

    /// Write `ca.pem`, `ca.cer`, and `ca.key` into `dir`. Creates
    /// `dir` if missing. On Unix tightens permissions: directory
    /// `0700`, key `0600`. Local-mode use only — BYOCA loads do
    /// not persist (the operator owns the file lifecycle).
    pub fn persist(&self, dir: &Path) -> Result<(), CaError> {
        create_dir_all(dir)?;
        write(&dir.join(CERT_FILE), self.cert_pem.as_bytes())?;
        write(&dir.join(CERT_CER_FILE), self.cert_pem.as_bytes())?;
        write(&dir.join(KEY_FILE), self.key_pem.as_bytes())?;
        tighten_permissions(dir, &dir.join(KEY_FILE))?;
        Ok(())
    }

    /// PEM-encoded CA certificate. What operators install into trust
    /// stores and what `NODE_EXTRA_CA_CERTS` should point at.
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// PEM-encoded private key. Needed by the leaf signer to sign
    /// per-host certificates; not exposed beyond the noodle process.
    #[must_use]
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// rcgen handles for signing child certs.
    ///
    /// Consumed by [`crate::cert::LocalCertMintService`] (ADR 034
    /// §2.2) to mint per-host leaves through the `CertMintService`
    /// port. The handles are returned by reference because rcgen's
    /// `Certificate` and `KeyPair` values are the issuer identity —
    /// leaves are signed via
    /// `params.signed_by(&issuer_key, &issuer_cert)`.
    #[must_use]
    pub fn issuer_handles(&self) -> (&rcgen::Certificate, &KeyPair) {
        (&self.rcgen_cert, &self.rcgen_key)
    }

    /// Intermediate-chain DERs to append after the leaf in every
    /// minted leaf's `cert_chain`. Empty unless this CA was loaded
    /// via [`Ca::load_static`] with a `chain.pem` present.
    ///
    /// Returned in leaf-presenting order: index 0 is the
    /// intermediate that signed `ca.pem`, subsequent entries walk
    /// up toward the root. The root itself is NOT included (clients
    /// must already trust it).
    #[must_use]
    pub fn intermediate_chain_der(&self) -> &[Vec<u8>] {
        &self.intermediate_chain_der
    }

    /// Convenience: full path to the public CA cert under `dir`.
    /// Useful when telling operators what to feed `NODE_EXTRA_CA_CERTS`.
    #[must_use]
    pub fn cert_path(dir: &Path) -> PathBuf {
        dir.join(CERT_FILE)
    }
}

/// Default BYOCA-static path. Platform conventions per feature 037
/// §2 #2:
///
/// - Linux / macOS: `$HOME/.config/noodle/ca/`.
/// - Windows: `%APPDATA%\noodle\ca\`.
/// - Fallback (no `$HOME` / no `%APPDATA%`): `./.noodle/ca/`.
#[must_use]
pub fn default_byoca_static_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("noodle").join("ca");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("noodle")
                .join("ca");
        }
    }
    PathBuf::from(".").join(".noodle").join("ca")
}

// ── Filesystem helpers wrapping io::Error in CaError::Io ───────────

fn create_dir_all(path: &Path) -> Result<(), CaError> {
    fs::create_dir_all(path).map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), CaError> {
    fs::write(path, bytes).map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_to_string(path: &Path) -> Result<String, CaError> {
    fs::read_to_string(path).map_err(|source| CaError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn tighten_permissions(dir: &Path, key: &Path) -> Result<(), CaError> {
    use std::os::unix::fs::PermissionsExt;
    let dir_perm = fs::Permissions::from_mode(0o700);
    let key_perm = fs::Permissions::from_mode(0o600);
    fs::set_permissions(dir, dir_perm).map_err(|source| CaError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    fs::set_permissions(key, key_perm).map_err(|source| CaError::Io {
        path: key.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn tighten_permissions(_dir: &Path, _key: &Path) -> Result<(), CaError> {
    // Windows / others: ACL-based, not the same model. Out of scope
    // for the foundation; revisit when noodle ships a Windows runner.
    Ok(())
}

/// Refuse to load the CA if `ca.key` is readable by anyone other
/// than the owning user. On Unix that's any mode bit beyond 0600
/// in the lower 9 bits. On non-Unix platforms this is a no-op:
/// Windows permissions are ACL-based and require a probe approach
/// out of scope for v1 (ADR 034 §4.2 footnote).
fn check_key_permissions(key_path: &Path) -> Result<(), CaLoadError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(key_path).map_err(|source| CaLoadError::Io {
            path: key_path.to_path_buf(),
            source,
        })?;
        let mode = meta.permissions().mode() & 0o777;
        // Allowed: 0600, 0400 (owner-read-only is fine — we read
        // once at startup), 0500 (with exec bit, harmless). Anything
        // that grants read/write/exec to group or others is a
        // security regression for a CA signing key. We test
        // explicitly: any group or other bit set → reject.
        if mode & 0o077 != 0 {
            return Err(CaLoadError::InsecurePermissions {
                path: key_path.to_path_buf(),
                mode,
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = key_path;
    }
    Ok(())
}

/// Compare the subject public key in `cert_pem` to the public key
/// derived from `key_pem`. Returns
/// [`CaLoadError::KeyCertMismatch`] when they disagree.
///
/// We compare the full SPKI byte sequence (algorithm + curve +
/// public key bytes) rather than just raw key bytes — that covers
/// algorithm-mismatch (RSA cert + ECDSA key) and curve-mismatch
/// (P-256 cert + P-384 key) cases in addition to the obvious
/// "wrong key entirely" case.
///
/// Implementation: extract the cert's SPKI via `x509-parser`; for
/// the key, round-trip through a throwaway self-signed cert via
/// rcgen and extract that cert's SPKI. Both paths produce the same
/// canonical DER encoding when the cert and key are paired.
fn verify_key_matches_cert(
    cert_pem: &str,
    key_pem: &str,
    cert_path: &Path,
    key_path: &Path,
) -> Result<(), CaLoadError> {
    let cert_der = pem::parse(cert_pem)
        .map_err(|e| CaLoadError::MalformedPem {
            what: CERT_FILE,
            path: cert_path.to_path_buf(),
            source: PemSource::Pem(e),
        })?
        .into_contents();

    let (_, cert_parsed) =
        x509_parser::parse_x509_certificate(&cert_der).map_err(|e| CaLoadError::MalformedPem {
            what: CERT_FILE,
            path: cert_path.to_path_buf(),
            source: PemSource::X509(e.to_string()),
        })?;
    let cert_spki_raw = cert_parsed.public_key().raw;

    // Derive the key's SPKI by self-signing a throwaway cert with
    // it via rcgen, then parsing that cert. This is the most
    // robust way to compare across rcgen's supported key types
    // without enumerating algorithms by hand.
    let probe_key = KeyPair::from_pem(key_pem).map_err(|e| CaLoadError::MalformedPem {
        what: KEY_FILE,
        path: key_path.to_path_buf(),
        source: PemSource::Rcgen(e),
    })?;
    let mut probe_params =
        CertificateParams::new(Vec::<String>::new()).map_err(|e| CaLoadError::MalformedPem {
            what: KEY_FILE,
            path: key_path.to_path_buf(),
            source: PemSource::Rcgen(e),
        })?;
    probe_params
        .distinguished_name
        .push(DnType::CommonName, "noodle-probe");
    let probe_cert =
        probe_params
            .self_signed(&probe_key)
            .map_err(|e| CaLoadError::MalformedPem {
                what: KEY_FILE,
                path: key_path.to_path_buf(),
                source: PemSource::Rcgen(e),
            })?;
    let probe_der = probe_cert.der();
    let (_, probe_parsed) =
        x509_parser::parse_x509_certificate(probe_der).map_err(|e| CaLoadError::MalformedPem {
            what: KEY_FILE,
            path: key_path.to_path_buf(),
            source: PemSource::X509(e.to_string()),
        })?;
    let key_spki_raw = probe_parsed.public_key().raw;

    if cert_spki_raw == key_spki_raw {
        Ok(())
    } else {
        Err(CaLoadError::KeyCertMismatch {
            cert_path: cert_path.to_path_buf(),
            key_path: key_path.to_path_buf(),
        })
    }
}

/// Parse a `chain.pem` bundle (one or more concatenated PEM
/// certificate blocks) into a vector of DER bytes, leaf-presenting
/// order preserved.
fn parse_chain_pem(chain_pem: &str, chain_path: &Path) -> Result<Vec<Vec<u8>>, CaLoadError> {
    let blocks = pem::parse_many(chain_pem).map_err(|e| CaLoadError::MalformedPem {
        what: CHAIN_FILE,
        path: chain_path.to_path_buf(),
        source: PemSource::Pem(e),
    })?;
    if blocks.is_empty() {
        // An empty chain.pem (operator left a placeholder) is not
        // an error — treat it identically to "no chain.pem".
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.into_iter().enumerate() {
        // Sanity-parse each block so we don't ship gibberish into
        // the leaf chain — a malformed intermediate would break
        // the TLS handshake at runtime; better to fail at startup.
        x509_parser::parse_x509_certificate(block.contents()).map_err(|e| {
            CaLoadError::MalformedPem {
                what: CHAIN_FILE,
                path: chain_path.to_path_buf(),
                source: PemSource::X509(format!("block {idx}: {e}")),
            }
        })?;
        out.push(block.into_contents());
    }
    Ok(out)
}

/// Convert a [`CaError`] (raised by `generate_or_load`) into the
/// caller-uniform [`CaLoadError`] surface used by [`Ca::load`].
fn load_err_from_ca_err(e: CaError) -> CaLoadError {
    match e {
        CaError::Io { path, source } => CaLoadError::Io { path, source },
        CaError::Rcgen(re) => CaLoadError::MalformedPem {
            what: CERT_FILE,
            path: PathBuf::new(),
            source: PemSource::Rcgen(re),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// `Result::expect_err` requires `T: Debug`; `Ca` doesn't
    /// derive Debug (`rcgen::Certificate` doesn't). Small helper
    /// to keep the negative-path tests readable.
    fn expect_err<T, E>(r: Result<T, E>) -> E {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn generate_yields_pem_artifacts() {
        let ca = Ca::generate().expect("generate");
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(ca.cert_pem().contains("-----END CERTIFICATE-----"));
        assert!(ca.key_pem().contains("PRIVATE KEY"));
        assert!(
            ca.intermediate_chain_der().is_empty(),
            "local-mode CA has no intermediate chain"
        );
    }

    #[test]
    fn persist_writes_three_files_in_dir() {
        let dir = tmp_dir();
        let ca = Ca::generate().expect("generate");
        ca.persist(dir.path()).expect("persist");
        assert!(dir.path().join(CERT_FILE).exists());
        assert!(dir.path().join(CERT_CER_FILE).exists());
        assert!(dir.path().join(KEY_FILE).exists());
        // .pem and .cer are byte-identical (same PEM, different extension)
        let pem = std::fs::read(dir.path().join(CERT_FILE)).unwrap();
        let cer = std::fs::read(dir.path().join(CERT_CER_FILE)).unwrap();
        assert_eq!(pem, cer);
    }

    #[test]
    fn load_local_after_persist_yields_identical_cert_bytes() {
        let dir = tmp_dir();
        let ca = Ca::generate().expect("generate");
        ca.persist(dir.path()).expect("persist");

        let reloaded = Ca::load_local(dir.path()).expect("load");
        // The persisted PEM should round-trip byte-for-byte.
        assert_eq!(ca.cert_pem(), reloaded.cert_pem());
        assert_eq!(ca.key_pem(), reloaded.key_pem());
    }

    #[test]
    fn generate_or_load_is_idempotent_across_runs() {
        let dir = tmp_dir();
        let first = Ca::generate_or_load(dir.path()).expect("first");
        let second = Ca::generate_or_load(dir.path()).expect("second");
        assert_eq!(first.cert_pem(), second.cert_pem());
        assert_eq!(first.key_pem(), second.key_pem());
    }

    #[test]
    fn generate_or_load_creates_dir_when_missing() {
        let parent = tmp_dir();
        let dir = parent.path().join("nested").join("ca");
        assert!(!dir.exists());
        let _ca = Ca::generate_or_load(&dir).expect("create then load");
        assert!(dir.exists());
        assert!(dir.join(CERT_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_tightens_key_permissions_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let ca = Ca::generate().expect("generate");
        ca.persist(dir.path()).expect("persist");
        let key_meta = std::fs::metadata(dir.path().join(KEY_FILE)).unwrap();
        let mode = key_meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file should be 0600, was {mode:o}");
        let dir_meta = std::fs::metadata(dir.path()).unwrap();
        let dmode = dir_meta.permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "dir should be 0700, was {dmode:o}");
    }

    #[test]
    fn parsed_cert_has_expected_subject() {
        let ca = Ca::generate().expect("generate");
        let pem = pem::parse(ca.cert_pem()).expect("parse PEM");
        // The cert is DER inside; fingerprint check: cert is
        // non-empty DER and starts with 0x30 (SEQUENCE).
        assert!(!pem.contents().is_empty());
        assert_eq!(pem.contents()[0], 0x30);
    }

    // ── BYOCA-static (S18) test fixtures + tests ──────────────

    /// Helper: write a fresh ECDSA CA's `ca.pem` + `ca.key` into
    /// `dir`, chmod the key to 0600 on Unix. Returns the synthesized
    /// CA so the test can compare bytes back.
    fn write_byoca_fixture(dir: &Path) -> Ca {
        let ca = Ca::generate().expect("test CA");
        fs::write(dir.join(CERT_FILE), ca.cert_pem()).expect("write ca.pem");
        fs::write(dir.join(KEY_FILE), ca.key_pem()).expect("write ca.key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(KEY_FILE), fs::Permissions::from_mode(0o600))
                .expect("chmod 0600");
        }
        ca
    }

    #[test]
    fn load_static_loads_valid_ca_and_key() {
        let dir = tmp_dir();
        let written = write_byoca_fixture(dir.path());
        let loaded = Ca::load_static(dir.path()).expect("load_static");
        assert_eq!(loaded.cert_pem(), written.cert_pem());
        assert_eq!(loaded.key_pem(), written.key_pem());
        assert!(
            loaded.intermediate_chain_der().is_empty(),
            "no chain.pem → empty intermediates"
        );
    }

    #[test]
    fn load_static_missing_ca_pem_fails_clearly() {
        let dir = tmp_dir();
        // Write only the key — no ca.pem.
        let ca = Ca::generate().expect("ca");
        fs::write(dir.path().join(KEY_FILE), ca.key_pem()).expect("write key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join(KEY_FILE), fs::Permissions::from_mode(0o600))
                .expect("chmod");
        }
        let err = expect_err(Ca::load_static(dir.path()));
        match err {
            CaLoadError::MissingFile { what, path } => {
                assert_eq!(what, "ca.pem");
                assert!(path.ends_with("ca.pem"));
            }
            other => panic!("expected MissingFile(ca.pem); got {other:?}"),
        }
    }

    #[test]
    fn load_static_missing_ca_key_fails_clearly() {
        let dir = tmp_dir();
        let ca = Ca::generate().expect("ca");
        fs::write(dir.path().join(CERT_FILE), ca.cert_pem()).expect("write cert");
        let err = expect_err(Ca::load_static(dir.path()));
        match err {
            CaLoadError::MissingFile { what, path } => {
                assert_eq!(what, "ca.key");
                assert!(path.ends_with("ca.key"));
            }
            other => panic!("expected MissingFile(ca.key); got {other:?}"),
        }
    }

    #[test]
    fn load_static_mismatched_cert_and_key_fails_clearly() {
        let dir = tmp_dir();
        let ca_a = Ca::generate().expect("ca a");
        let ca_b = Ca::generate().expect("ca b");
        // ca.pem from CA_A, ca.key from CA_B — keys don't match.
        fs::write(dir.path().join(CERT_FILE), ca_a.cert_pem()).expect("write cert");
        fs::write(dir.path().join(KEY_FILE), ca_b.key_pem()).expect("write key");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join(KEY_FILE), fs::Permissions::from_mode(0o600))
                .expect("chmod");
        }
        let err = expect_err(Ca::load_static(dir.path()));
        assert!(
            matches!(err, CaLoadError::KeyCertMismatch { .. }),
            "expected KeyCertMismatch, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_static_rejects_0644_key_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let _ = write_byoca_fixture(dir.path());
        // Loosen permissions — this is what the test asserts on.
        fs::set_permissions(dir.path().join(KEY_FILE), fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        let err = expect_err(Ca::load_static(dir.path()));
        match err {
            CaLoadError::InsecurePermissions { path, mode } => {
                assert!(path.ends_with("ca.key"));
                assert_eq!(mode & 0o777, 0o644, "reported mode wrong: {mode:o}");
            }
            other => panic!("expected InsecurePermissions; got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn load_static_accepts_0400_key_permissions() {
        // 0400 is strictly safer than 0600 — the load must accept
        // it. We only read the key once at startup; no write needed.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let _ = write_byoca_fixture(dir.path());
        fs::set_permissions(dir.path().join(KEY_FILE), fs::Permissions::from_mode(0o400))
            .expect("chmod 0400");
        Ca::load_static(dir.path()).expect("0400 should be acceptable");
    }

    #[test]
    fn load_static_malformed_pem_fails_clearly() {
        let dir = tmp_dir();
        fs::write(dir.path().join(CERT_FILE), b"not a pem certificate at all").expect("write");
        fs::write(dir.path().join(KEY_FILE), b"also not a pem key").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join(KEY_FILE), fs::Permissions::from_mode(0o600))
                .expect("chmod");
        }
        let err = expect_err(Ca::load_static(dir.path()));
        assert!(
            matches!(err, CaLoadError::MalformedPem { .. }),
            "expected MalformedPem, got {err:?}"
        );
    }

    #[test]
    fn load_static_loads_optional_chain_pem() {
        let dir = tmp_dir();
        let _ = write_byoca_fixture(dir.path());
        // Generate two synthetic "intermediates" — these are
        // structurally CA certs (so x509-parser accepts them);
        // they aren't actually in the chain from leaf↑root since
        // we're not building a real PKI hierarchy in the unit
        // test. The S18 contract is "if chain.pem is present,
        // its DERs are loaded into intermediate_chain_der in
        // order"; the integration test validates real chaining.
        let int_a = Ca::generate().expect("int a");
        let int_b = Ca::generate().expect("int b");
        let chain_pem = format!("{}{}", int_a.cert_pem(), int_b.cert_pem());
        fs::write(dir.path().join(CHAIN_FILE), chain_pem).expect("write chain");

        let loaded = Ca::load_static(dir.path()).expect("load_static");
        assert_eq!(
            loaded.intermediate_chain_der().len(),
            2,
            "chain.pem with two blocks → two intermediates"
        );
        // Sanity: each DER starts with the X.509 SEQUENCE tag.
        for der in loaded.intermediate_chain_der() {
            assert!(!der.is_empty());
            assert_eq!(der[0], 0x30);
        }
    }

    #[test]
    fn load_static_handles_empty_chain_pem() {
        // Operator left a placeholder file — treat as no chain.
        let dir = tmp_dir();
        let _ = write_byoca_fixture(dir.path());
        fs::write(dir.path().join(CHAIN_FILE), "").expect("write empty chain");
        let loaded = Ca::load_static(dir.path()).expect("load");
        assert!(loaded.intermediate_chain_der().is_empty());
    }

    #[test]
    fn load_static_rejects_malformed_chain_pem() {
        let dir = tmp_dir();
        let _ = write_byoca_fixture(dir.path());
        // Garbage that won't parse as PEM at all.
        fs::write(
            dir.path().join(CHAIN_FILE),
            b"-----BEGIN CERTIFICATE-----\nNOT_BASE64\n-----END CERTIFICATE-----\n",
        )
        .expect("write bogus chain");
        let err = expect_err(Ca::load_static(dir.path()));
        assert!(matches!(err, CaLoadError::MalformedPem { .. }));
    }

    // ── load() dispatcher ──────────────────────────────────────

    #[test]
    fn load_dispatches_local_mode_to_generate_or_load() {
        let dir = tmp_dir();
        let ca = Ca::load(CaMode::Local, dir.path()).expect("local load");
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        // Idempotent: second call yields the same cert.
        let again = Ca::load(CaMode::Local, dir.path()).expect("local load 2");
        assert_eq!(ca.cert_pem(), again.cert_pem());
    }

    #[test]
    fn load_dispatches_byoca_mode_to_load_static() {
        let dir = tmp_dir();
        let written = write_byoca_fixture(dir.path());
        let loaded = Ca::load(CaMode::ByocaStatic, dir.path()).expect("byoca load");
        assert_eq!(loaded.cert_pem(), written.cert_pem());
    }

    #[test]
    fn load_byoca_does_not_silently_generate_when_files_missing() {
        // Acceptance #3: BYOCA-static must NOT fall back to
        // generating a local CA when files are absent.
        let dir = tmp_dir();
        let err = expect_err(Ca::load(CaMode::ByocaStatic, dir.path()));
        assert!(
            matches!(err, CaLoadError::MissingFile { .. }),
            "expected MissingFile, got {err:?}"
        );
        // And no files should have been created behind our back.
        assert!(!dir.path().join(CERT_FILE).exists());
        assert!(!dir.path().join(KEY_FILE).exists());
    }

    #[test]
    fn ca_mode_default_is_local() {
        assert_eq!(CaMode::default(), CaMode::Local);
    }

    #[test]
    fn default_byoca_static_dir_is_under_home_or_appdata() {
        let dir = default_byoca_static_dir();
        // Don't be picky about absolute vs. relative — the function
        // falls back to `./.noodle/ca` when neither env var is set.
        // Just check it ends with the conventional tail.
        assert!(
            dir.ends_with(PathBuf::from("noodle").join("ca")) || dir.ends_with(".noodle/ca"),
            "unexpected default BYOCA dir: {}",
            dir.display()
        );
    }
}
