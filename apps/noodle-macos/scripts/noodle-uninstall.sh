#!/usr/bin/env bash
# Standalone noodle uninstaller.
#
# SIP blocks `systemextensionsctl uninstall`, so NO script can
# deactivate the system extension directly — only the container
# app can, via OSSystemExtensionRequest. This script removes
# everything else (CA trust + certs looped until none remain,
# trust-env vars, CA files, manager-independent state, the app
# bundle) and deletes /Applications/Noodle.app. With the owning
# app gone, a reboot orphans + GCs the sysext copies (the ones
# already "waiting to uninstall on reboot" and the active one).
#
# Usage:  sudo bash apps/noodle-macos/scripts/noodle-uninstall.sh
# Then:   REBOOT.  Then: systemextensionsctl list | grep noodle  (empty)
set -euo pipefail

CA_CN="ca.noodleproxy.macos"
SYS_KC="/Library/Keychains/System.keychain"
CA_DIR="/Library/Application Support/noodle"
APP="/Applications/Noodle.app"
ENV_VARS=(NODE_EXTRA_CA_CERTS REQUESTS_CA_BUNDLE SSL_CERT_FILE CURL_CA_BUNDLE AWS_CA_BUNDLE)

[ "$(id -u)" -eq 0 ] || { echo "run with sudo: sudo bash $0" >&2; exit 1; }

echo "==> unsetting CA-bundle trust env vars (root + user domain)"
for v in "${ENV_VARS[@]}"; do
  launchctl unsetenv "$v" 2>/dev/null || true
  [ -n "${SUDO_USER:-}" ] && sudo -u "$SUDO_USER" launchctl unsetenv "$v" 2>/dev/null || true
done

echo "==> removing noodle CA trust + certs from System keychain (loop until gone)"
for _ in $(seq 1 30); do
  SHA=$(security find-certificate -a -c "$CA_CN" -Z "$SYS_KC" 2>/dev/null \
        | awk '/SHA-1 hash:/{print $3; exit}')
  [ -z "$SHA" ] && break
  security delete-certificate -Z "$SHA" "$SYS_KC" 2>/dev/null || true
done
# drop any lingering admin trust settings via the on-disk PEM, if present
[ -f "$CA_DIR/macos-tproxy-ca.pem" ] \
  && security remove-trusted-cert -d "$CA_DIR/macos-tproxy-ca.pem" 2>/dev/null || true

echo "==> deleting CA files + app bundle"
rm -rf "$CA_DIR" 2>/dev/null || true
rm -rf "$APP" 2>/dev/null || true

echo "==> remaining noodle sysext state (cleared at reboot):"
systemextensionsctl list 2>/dev/null | grep -i noodle || echo "  (none registered)"

cat <<'EOF'

DONE (script-side cleanup complete).

SIP prevents a script from deactivating the sysext — but the app
bundle is now deleted, so the OS can no longer validate/own it.

  >>> REBOOT NOW. <<<

After reboot:
  systemextensionsctl list | grep -i noodle    # expect: empty
Do NOT `make install` again, or it re-stages a fresh copy.
EOF
