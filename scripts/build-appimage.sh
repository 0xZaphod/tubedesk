#!/bin/sh
# Builds the AppImage. NO_STRIP: linuxdeploy's own strip fails on binaries
# from recent toolchains (e.g. on Arch).
set -eu
cd "$(dirname "$0")/.."
NO_STRIP=true cargo tauri build --bundles appimage "$@"
