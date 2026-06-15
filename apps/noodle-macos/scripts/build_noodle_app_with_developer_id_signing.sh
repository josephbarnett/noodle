#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SPEC_PATH="$APP_DIR/Project.dist.yml"
# noodle workspace root, two levels above apps/noodle-macos/
WORKSPACE_CARGO_TOML="$APP_DIR/../../Cargo.toml"
DERIVED_DATA_PATH="${NOODLE_TPROXY_DERIVED_DATA_PATH:-$APP_DIR/.xcode-derived/noodle-app-dist}"
TEAM_ID="${NOODLE_TPROXY_DEVELOPMENT_TEAM:?set NOODLE_TPROXY_DEVELOPMENT_TEAM to your Developer ID team ID}"
CONTAINER_SIGNING_IDENTITY="${NOODLE_TPROXY_CONTAINER_SIGNING_IDENTITY:-Developer ID Application}"
EXT_SIGNING_IDENTITY="${NOODLE_TPROXY_EXTENSION_SIGNING_IDENTITY:-$CONTAINER_SIGNING_IDENTITY}"
CONTAINER_PROFILE_SPECIFIER="${NOODLE_TPROXY_CONTAINER_PROFILE_SPECIFIER:-Noodle Proxy (Host)}"
EXT_PROFILE_SPECIFIER="${NOODLE_TPROXY_EXTENSION_PROFILE_SPECIFIER:-Noodle Proxy (Extension)}"
CONTAINER_PROFILE_PATH="${NOODLE_TPROXY_CONTAINER_PROFILE_PATH:-}"
EXT_PROFILE_PATH="${NOODLE_TPROXY_EXTENSION_PROFILE_PATH:-}"
PROFILE_INSTALL_DIR="${HOME}/Library/MobileDevice/Provisioning Profiles"

workspace_version="$(
  sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$WORKSPACE_CARGO_TOML" | head -n1
)"
git_short_sha="${NOODLE_TPROXY_GIT_SHORT_SHA:-$(git -C "$APP_DIR/../.." rev-parse --short=12 HEAD 2>/dev/null || true)}"

if [ -z "$workspace_version" ]; then
  echo "Failed to read workspace.package.version from $WORKSPACE_CARGO_TOML" >&2
  exit 1
fi

if [ -n "$git_short_sha" ]; then
  default_marketing_version="${workspace_version}+${git_short_sha}"
else
  default_marketing_version="$workspace_version"
fi

marketing_version="${NOODLE_TPROXY_MARKETING_VERSION:-$default_marketing_version}"
current_project_version="${NOODLE_TPROXY_CURRENT_PROJECT_VERSION:-$(
  printf '%s' "$workspace_version" | sed -E '
    s/^[^0-9]*//
    s/[-.]?(alpha|beta|rc)[.-]?([0-9]+)$/.\2/
    s/[^0-9.].*$//
  '
)}"

if [ -z "$current_project_version" ]; then
  echo "Failed to derive CURRENT_PROJECT_VERSION from workspace version: $workspace_version" >&2
  exit 1
fi

install_profile_if_needed() {
  local profile_path="$1"
  if [ -z "$profile_path" ]; then
    return 0
  fi
  if [ ! -f "$profile_path" ]; then
    echo "Provisioning profile not found: $profile_path" >&2
    exit 1
  fi

  mkdir -p "$PROFILE_INSTALL_DIR"

  local decoded_plist
  decoded_plist="$(mktemp)"
  /usr/bin/openssl smime -inform der -verify -noverify -in "$profile_path" > "$decoded_plist" 2>/dev/null

  local uuid
  uuid="$(/usr/libexec/PlistBuddy -c 'Print :UUID' "$decoded_plist")"
  cp "$profile_path" "$PROFILE_INSTALL_DIR/$uuid.provisionprofile"
  rm -f "$decoded_plist"
}

install_profile_if_needed "$CONTAINER_PROFILE_PATH"
install_profile_if_needed "$EXT_PROFILE_PATH"

cd "$APP_DIR"
xcodegen generate --spec "$SPEC_PATH"
xcodebuild   -project Noodle.xcodeproj   -scheme Noodle   -configuration Release   -derivedDataPath "$DERIVED_DATA_PATH"   NOODLE_TPROXY_DEVELOPMENT_TEAM="$TEAM_ID"   NOODLE_TPROXY_CONTAINER_SIGNING_IDENTITY="$CONTAINER_SIGNING_IDENTITY"   NOODLE_TPROXY_EXTENSION_SIGNING_IDENTITY="$EXT_SIGNING_IDENTITY"   NOODLE_TPROXY_CONTAINER_PROFILE_SPECIFIER="$CONTAINER_PROFILE_SPECIFIER"   NOODLE_TPROXY_EXTENSION_PROFILE_SPECIFIER="$EXT_PROFILE_SPECIFIER"   NOODLE_TPROXY_MARKETING_VERSION="$marketing_version"   NOODLE_TPROXY_CURRENT_PROJECT_VERSION="$current_project_version"   clean build
