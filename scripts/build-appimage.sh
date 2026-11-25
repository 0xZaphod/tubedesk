#!/bin/sh
# Builds the AppImage with only the GStreamer plugins TubeDesk plays with.
# Tauri's bundler would otherwise copy every plugin on this machine (and
# their codec libraries), which makes the image several times larger and
# slower to start.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
src=$(pkg-config --variable=pluginsdir gstreamer-1.0)
helpers=$(pkg-config --variable=pluginscannerdir gstreamer-1.0)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/plugins" "$work/helpers"

# playbin over HTTP (souphttpsrc) → matroska/webm → opus → audio sink.
# autodetect/alsa/audioparsers are fallbacks for other systems.
for p in coreelements playback typefindfunctions soup matroska opus \
         audioconvert audioresample volume pulseaudio autodetect alsa audioparsers; do
  if [ -e "$src/libgst$p.so" ]; then
    ln -s "$src/libgst$p.so" "$work/plugins/"
  else
    echo "warning: GStreamer plugin $p not installed" >&2
  fi
done
ln -s "$helpers/gst-plugin-scanner" "$work/helpers/"

cd "$root"
GSTREAMER_PLUGINS_DIR="$work/plugins" GSTREAMER_HELPERS_DIR="$work/helpers" \
  NO_STRIP=true cargo tauri build --bundles appimage "$@"
