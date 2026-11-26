#!/bin/sh
# Builds the AppImage. NO_STRIP: linuxdeploy's own strip fails on binaries
# from recent toolchains (e.g. on Arch).
#
# Update files (.sig) are signed with the key in ~/.tauri/tubedesk.key when
# it exists (password in tubedesk.key.password); without it they are skipped.
set -eu
cd "$(dirname "$0")/.."
key="$HOME/.tauri/tubedesk.key"
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ] && [ -r "$key" ]; then
  TAURI_SIGNING_PRIVATE_KEY=$(cat "$key")
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD=$(cat "$key.password" 2>/dev/null || true)
  export TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
fi
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  set -- --config '{"bundle":{"createUpdaterArtifacts":false}}' "$@"
fi
NO_STRIP=true cargo tauri build --bundles appimage "$@"
