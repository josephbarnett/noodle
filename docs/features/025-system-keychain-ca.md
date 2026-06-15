# 025 — System Keychain CA install + ops-doc rewrite

**Status:** open
**Depends on:** 011 iterations 1–3b
**Design refs:**
[`docs/adrs/014-transparent-mode-and-quic-mitm.md`](../adrs/014-transparent-mode-and-quic-mitm.md)
§7 (Story 011 remaining work),
[`docs/adrs/011-tls-mitm-and-ca.md`](../adrs/011-tls-mitm-and-ca.md)

---

## 1. Value delivered

After this story, the operator installs noodle's MITM CA into the
macOS System Keychain via a single menu action in the noodle.app —
no shell snippets, no `security add-trusted-cert` runbook, no
`launchctl setenv` ceremony for GUI apps. Uninstall is equally
single-action. Combined with the in-flight extension install, this
reduces the operator runbook for inspecting Claude Desktop from
five sections to one paragraph: "install the .app, approve the
system extension, click Install CA, done." Closes Story 011.

## 2. Acceptance criteria

1. "Install CA" menu item in the noodle.app adds the noodle CA
   certificate to `/Library/Keychains/System.keychain` as a
   trusted root for SSL.
2. "Uninstall CA" removes the noodle CA from the System Keychain
   cleanly — no orphaned trust settings, no zombie entries.
3. Install is idempotent: clicking "Install CA" on a system where
   the CA is already trusted produces no error, no duplicate
   entry, no spurious dialog.
4. Status of the CA (Installed / NotInstalled / Mismatch) is
   visible in the menu — the operator can tell at a glance
   whether trust is active.
5. The install action requires admin authentication via the
   macOS standard authorization prompt (System Keychain is
   root-owned); this is delegated to the OS, not bypassed.
6. `docs/guides/inspecting-claude-desktop.md` is rewritten to
   the one-paragraph install runbook and updated to reference the
   menu actions.
7. Uninstall of the .app's containing system extension (via
   `OSSystemExtensionRequest.deactivationRequest`, already
   present) also offers to remove the CA — the operator can
   leave the system clean in one flow.

## 3. Abstractions introduced or refined

A `CaTrustStore` trait in `crates/noodle-macos-tproxy`:

```rust
pub trait CaTrustStore: Send + Sync {
    fn install(&self, ca_pem: &[u8]) -> Result<(), TrustError>;
    fn uninstall(&self, ca_fingerprint: &CaFingerprint)
        -> Result<(), TrustError>;
    fn status(&self, ca_fingerprint: &CaFingerprint)
        -> TrustStatus;
}

pub enum TrustStatus { Installed, NotInstalled, Mismatch }
```

Implementations:

- `SystemKeychainTrustStore` — production impl using the
  `security_framework` crate's `SecKeychain` /
  `SecTrustSettings` APIs against
  `/Library/Keychains/System.keychain`.
- `InMemoryTrustStore` — test double; holds a `HashSet<CaFingerprint>`
  and obeys the same idempotency rules.

DI seam: the Swift menu actions call into Rust via the FFI macro
with a `Box<dyn CaTrustStore>` parameter. Production wiring picks
`SystemKeychainTrustStore`; unit tests pick `InMemoryTrustStore`.

## 4. Patterns applied

- **Strategy** — `CaTrustStore` is the strategy; backends are
  interchangeable.
- **Command** — each menu action is a `TrustCommand` (Install /
  Uninstall / Refresh) — uniform invocation, undo-friendly
  shape.
- **State** — `TrustStatus` is the externally-visible state; the
  menu UI's enabled/disabled affordances are a function of the
  current state.

## 5. Test plan

- Unit: `InMemoryTrustStore::install` then `status` returns
  `Installed`; calling `install` twice does not duplicate and
  does not error.
- Unit: `InMemoryTrustStore::uninstall` after `install` returns
  `status == NotInstalled`. Uninstall on a never-installed CA
  is a no-op (does not error).
- Unit: `InMemoryTrustStore::install` of one fingerprint then
  `status` of a different fingerprint returns `NotInstalled`,
  not `Mismatch` — `Mismatch` is reserved for "a CA *with this
  subject* is installed but its bytes differ from ours."
- Integration (manual on a developer machine until macOS CI is
  in place): run install, verify with `security
  find-certificate -c noodle /Library/Keychains/System.keychain`;
  run uninstall, verify it's gone; re-run install to confirm
  idempotency.

## 6. PR scope

Two PRs:

- **025.a — Rust `CaTrustStore` trait + `SystemKeychainTrustStore`
  + `InMemoryTrustStore` + tests + FFI plumbing.** ~400 lines
  Rust, ~50 lines FFI declarations. Mergeable independently
  because the trait is exercised entirely with
  `InMemoryTrustStore` in tests.
- **025.b — Swift menu actions + status indicator + ops-doc
  rewrite.** Menu wiring in the .app, status polling, manual
  authorization prompt handling, deactivation-flow CA-removal
  prompt, full rewrite of
  `docs/guides/inspecting-claude-desktop.md`. ~200 lines
  Swift + ~150 lines markdown.

## 7. Out of scope

- **User Keychain support.** This story targets System Keychain
  only — broader trust, single install, simpler runbook. User
  Keychain would be a separate story if a use case ever
  surfaces.
- **CA rotation / multi-CA support.** The .app installs and
  removes one CA; the rotation case (revoke and replace) is
  deferred.
- **Cross-platform trust store support** (Linux, Windows).
  Different stories per platform.
- **Removing the existing `make ca-trust-macos`-style shell
  flow.** It can stay as a developer-only fallback; the
  user-facing flow is the .app menu.
- **DNS extension install/uninstall flow** — that lands as part
  of story 024.
