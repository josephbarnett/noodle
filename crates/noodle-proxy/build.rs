//! Minimal build script emitting `VERGEN_GIT_SHA` and
//! `VERGEN_BUILD_DATE` env vars at compile time so the
//! `CollectorApp` envelope field (ADR 029 §2.4 / refactor slice
//! S6) carries real build provenance — no external crate
//! dependency required.
//!
//! Compile-time embedding rather than runtime resolution is the
//! load-bearing property: by the time a collected `tap.jsonl`
//! reaches downstream tooling, the binary may have moved (or
//! disappeared). Embedding the SHA + date at link time keeps the
//! envelope self-describing.
//!
//! Failure modes:
//! - `git` not on PATH → emit `"unknown"`, build still succeeds.
//! - Detached HEAD / dirty tree → SHA is still meaningful (it's
//!   the HEAD commit); dirty state is not surfaced here (a
//!   future iteration may add `-dirty` suffix if we need it).
//! - Build outside a git checkout (e.g. crates.io tarball) →
//!   emit `"unknown"`.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves so the embedded SHA stays fresh.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-env-changed=NOODLE_BUILD_OVERRIDE_SHA");
    println!("cargo:rerun-if-env-changed=NOODLE_BUILD_OVERRIDE_DATE");

    // Allow overrides for reproducible-build environments that
    // don't have git available (e.g. crates.io / vendored
    // tarball).
    let git_sha = std::env::var("NOODLE_BUILD_OVERRIDE_SHA").ok().or_else(|| {
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    });
    let build_date = std::env::var("NOODLE_BUILD_OVERRIDE_DATE")
        .ok()
        .or_else(|| {
            // RFC3339-ish UTC date so the envelope's `OffsetDateTime`
            // parser can consume it. Format: `YYYY-MM-DDTHH:MM:SSZ`.
            Command::new("date")
                .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|s| !s.is_empty())
        });

    println!(
        "cargo:rustc-env=VERGEN_GIT_SHA={}",
        git_sha.as_deref().unwrap_or("unknown"),
    );
    println!(
        "cargo:rustc-env=VERGEN_BUILD_DATE={}",
        build_date.as_deref().unwrap_or("1970-01-01T00:00:00Z"),
    );
}
