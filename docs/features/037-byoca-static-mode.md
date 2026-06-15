# 037 — BYOCA static mode (operator-supplied CA on disk)

**Status:** shipped (slice S18; exec-claude e2e green
2026-05-25: 67 tap records, 11 on `api.anthropic.com`).
**Depends on:** 036
**Design refs:**
[`docs/adrs/034-enterprise-ca-and-external-signing.md`](../adrs/034-enterprise-ca-and-external-signing.md) §2.1, §4,
[`docs/adrs/025-dispatch-table.md`](../adrs/025-dispatch-table.md)

---

## 1. Value delivered

An enterprise IT operator can pre-place a CA cert and key at the
noodle CA path (provided via MDM / package install / Group
Policy) and noodle will use that CA to sign leaves instead of
generating its own. Devices that already trust the enterprise's
internal CA (the common case in regulated environments)
immediately trust noodle-minted leaves with no per-device
operator-installed root. This is the "BYOCA-static" mode named
in ADR 011 §4b but never specified end-to-end.

## 2. Acceptance criteria

1. Config selects CA mode: `mode = "local"` (default — story 036
   behaviour) or `mode = "byoca-static"`.
2. In `byoca-static` mode, noodle loads `ca.pem` + `ca.key` from
   the configured directory (default `~/.config/noodle/ca/` on
   Linux/macOS, `%APPDATA%\noodle\ca\` on Windows). If
   `chain.pem` is present, it is included in the leaf chain.
3. If the CA files are missing in `byoca-static` mode, the proxy
   fails to start with a clear error pointing at the configured
   path. It does NOT silently fall back to generating a local CA.
4. `LocalCertMintService` (from story 036) is reused with the
   loaded `Ca` material — no new mint service implementation.
5. A leaf minted in BYOCA-static mode chains to the operator's CA
   (verifiable via `openssl verify -CAfile ca.pem chain.pem
   leaf.pem`).
6. File permissions are enforced: `ca.key` must be mode 0600 (or
   ACL-equivalent on Windows). Looser permissions cause startup
   to fail with a security warning.
7. Existing tests for story 036 still pass; new tests cover the
   BYOCA-static load path.

## 3. Abstractions introduced or refined

- **Refined:** `Ca::generate_or_load` (ADR 011 §4b) becomes
  `Ca::load(mode, path)` with explicit mode dispatch. No new
  trait.
- **New:** `CaLoadError` enum covering missing files, bad
  permissions, malformed PEM, key/cert mismatch.
- **Config:** `[ca]` section in the noodle config file (TOML)
  with `mode` and `[ca.byoca_static]` subsection.

## 4. Patterns applied

None warranted — this is configuration plumbing on top of an
existing seam.

## 5. Test plan

- **Unit:** load valid CA + key, load mismatched cert/key (should
  fail), load missing files (should fail), load with 0644
  permissions on the key (should fail with permission warning).
- **Integration:** start proxy with BYOCA-static config pointing
  at a test-generated enterprise CA; mint a leaf for
  `api.anthropic.com`; verify the chain validates against the
  test enterprise CA.
- **Capture-driven:** re-run TLS-MITM acceptance capture with
  BYOCA-static config; client-visible bytes match the local-CA
  baseline modulo the cert chain.

## 6. PR scope

One PR. ~80 LOC config plumbing + ~120 LOC load/validate logic
+ ~60 LOC tests. Reviewable in 30 minutes.

## 7. Out of scope

- External signing (no local key) → story 038.
- MDM distribution mechanics (Configuration Profile / Group
  Policy authoring) → operations runbook, not engineering.
- CA rotation (replacing CA material on a running proxy) → ADR
  034 §9 open question; deferred.
- Per-cell CA selection (different CA per host group) → ADR 034
  §9 multi-CA; deferred.
