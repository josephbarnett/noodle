//! Test-side fixture loader for `tests/fixtures/adr_048/*.fixture.json`
//! — the sanitized projection of real `claude -p` traffic recorded
//! with mitmproxy.
//!
//! The `.mitm` files are gitignored (they carry live Bearer tokens
//! and user prompt text). `tools/extract_capture_fixture.py`
//! distills each `.mitm` into a fixture JSON containing only
//! structural facts: per-turn `canonical_system_hash`, `stop_reason`,
//! tool-use names, message counts. The fixture is what tests load —
//! committed alongside the tests so CI can run them without the
//! `.mitm` files.
//!
//! Capture acquisition + extraction is documented in
//! `docs/guides/capture-acquisition.md`.

use std::path::PathBuf;

use noodle_core::SystemHash;
use serde_json::Value;

pub fn fixtures_dir() -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    PathBuf::from(manifest).join("tests/fixtures/adr_048")
}

pub fn load_fixture(name: &str) -> Value {
    let path = fixtures_dir().join(format!("{name}.fixture.json"));
    let bytes = std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "read fixture {}: {err} — re-extract from .mitm with `mitmdump -nq -r captures/max/{name}.mitm -s tools/extract_capture_fixture.py --set fixture_out={}`",
            path.display(),
            path.display(),
        )
    });
    serde_json::from_slice(&bytes).expect("fixture json parse")
}

/// Turn the captured hex `canonical_system_hash` into a
/// [`SystemHash`]. Returns `None` when the turn has no system
/// prompt at all (e.g. title-gen side-calls).
pub fn turn_system_hash(turn: &Value) -> Option<SystemHash> {
    hex_to_system_hash(turn["canonical_system_hash"].as_str()?)
}

/// Turn a captured `sha256(text)` hex digest into a [`SystemHash`].
///
/// The hash captured by the extractor is already
/// `sha256(text)` — but `SystemHash::from_bytes` salts with a
/// domain separator. Reconstructing the exact `SystemHash` would
/// require running canonicalization in Rust on the original text,
/// which we deliberately don't ship. Instead we round-trip the
/// *hash bytes* via a domain-separated input that yields a
/// deterministic distinct `SystemHash` per captured hash — that's
/// sufficient for testing the *equality* relation the detector
/// keys on (same text → same captured hex → same `SystemHash`).
pub fn hex_to_system_hash(hex: &str) -> Option<SystemHash> {
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let lo = hex.as_bytes().get(2 * i + 1)?;
        let hi = hex.as_bytes().get(2 * i)?;
        *byte = (hex_nybble(*hi)? << 4) | hex_nybble(*lo)?;
    }
    Some(SystemHash::from_bytes(&bytes))
}

/// The turn's `first_user_text_sha256s` (fixture v4) as
/// [`SystemHash`]es — the pending-children lineage match keys
/// (ADR 048 gap review §6.R2). Empty for turns captured before
/// fixture v4 or with no first user message.
pub fn turn_first_user_hashes(turn: &Value) -> Vec<SystemHash> {
    turn["first_user_text_sha256s"]
        .as_array()
        .map(|hashes| {
            hashes
                .iter()
                .filter_map(|h| h.as_str())
                .filter_map(hex_to_system_hash)
                .collect()
        })
        .unwrap_or_default()
}

/// The spawn's `prompt_sha256` (fixture v4) as a [`SystemHash`] —
/// the fingerprint pushed with the pending child. `None` for
/// non-spawn tool uses or pre-v4 fixtures.
pub fn tool_use_prompt_hash(tool_use: &Value) -> Option<SystemHash> {
    hex_to_system_hash(tool_use["prompt_sha256"].as_str()?)
}

fn hex_nybble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}
