#!/bin/sh
# Installs a .desktop entry so KDE/GNOME show TubeDesk in the app menu and its
# icon in the taskbar. On Wayland the taskbar looks the icon up via the
# desktop file whose name matches the window's app id, not the window itself.
#
#   install-desktop-entry.sh             launch the dev build (target/debug)
#   install-desktop-entry.sh --appimage  install the built AppImage to
#                                        ~/.local/bin and launch that
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
data="${XDG_DATA_HOME:-$HOME/.local/share}"
dir="$data/applications"
mkdir -p "$dir"

exec_path="$root/target/debug/tubedesk"
icon="$root/icons/icon.png"
if [ "${1:-}" = "--appimage" ]; then
  built=$(ls -t "$root"/target/release/bundle/appimage/*.AppImage 2>/dev/null | head -n1)
  [ -n "$built" ] || { echo "no AppImage built; run: cargo tauri build --bundles appimage" >&2; exit 1; }
  mkdir -p "$HOME/.local/bin"
  exec_path="$HOME/.local/bin/TubeDesk.AppImage"
  install -m 755 "$built" "$exec_path"
  # Copy the icon too, so the entry doesn't depend on this checkout.
  icon_dir="$data/icons/hicolor/512x512/apps"
  mkdir -p "$icon_dir"
  install -m 644 "$root/icons/icon.png" "$icon_dir/dev.tubedesk.app.png"
  icon=dev.tubedesk.app
fi

write_entry() {
  # $1 = file name, $2 = extra line
  cat > "$dir/$1.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=TubeDesk
Comment=Audio-only YouTube Music client
Exec=$exec_path
Icon=$icon
Terminal=false
Categories=AudioVideo;Audio;Player;
StartupWMClass=tubedesk
$2
DESKTOP
}

# The app id is the Tauri identifier; the binary name is a fallback some
# compositors use. The alias is hidden so the menu shows one entry.
write_entry dev.tubedesk.app ""
write_entry tubedesk "NoDisplay=true"

if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$dir" 2>/dev/null || true
fi
echo "installed $dir/dev.tubedesk.app.desktop -> $exec_path"
