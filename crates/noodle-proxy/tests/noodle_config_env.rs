//! `NOODLE_CONFIG` resolution in the proxy binary (ADR 048 §11 item 1).
//!
//! When `NOODLE_CONFIG` names a file that can't be read or parsed, the
//! proxy fails loud — exits non-zero with a clear message — instead of
//! silently degrading to the embedded default. This guards the
//! operator-supplied-config path (docker `-v` bind mount / k8s
//! `ConfigMap`) so a typo in a mounted file can't quietly no-op while the
//! operator thinks their edits are live.
//!
//! The config load happens before any listener binds or CA is
//! generated, so this runs hermetically without network or a user CA.

use std::process::Command;

#[test]
fn missing_noodle_config_file_exits_nonzero() {
    let out = Command::new(env!("CARGO_BIN_EXE_noodle"))
        .env("NOODLE_CONFIG", "/noodle/this-path-does-not-exist.toml")
        .env("NOODLE_LISTEN", "127.0.0.1:0")
        .output()
        .expect("spawn noodle binary");

    assert!(
        !out.status.success(),
        "expected non-zero exit on missing NOODLE_CONFIG, got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NOODLE_CONFIG load failed"),
        "expected a clear NOODLE_CONFIG error on stderr, got:\n{stderr}"
    );
}

#[test]
fn invalid_noodle_config_toml_exits_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("noodle.toml");
    // Schema-invalid: `enabled = true` with an empty `enhancements`
    // list fails NoodleConfig validation (see config_loader unit tests).
    std::fs::write(
        &path,
        "[context]\nenabled = true\nenhancements = []\n[context.discovery]\n",
    )
    .expect("write bad config");

    let out = Command::new(env!("CARGO_BIN_EXE_noodle"))
        .env("NOODLE_CONFIG", &path)
        .env("NOODLE_LISTEN", "127.0.0.1:0")
        .output()
        .expect("spawn noodle binary");

    assert!(
        !out.status.success(),
        "expected non-zero exit on invalid NOODLE_CONFIG, got {:?}",
        out.status.code()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NOODLE_CONFIG load failed"),
        "expected a clear NOODLE_CONFIG error on stderr, got:\n{stderr}"
    );
}
