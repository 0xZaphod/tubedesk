#!/bin/sh
# Installs a .desktop entry for the local TubeDesk build, so KDE/GNOME show
# its icon in the taskbar. On Wayland the taskbar looks the icon up via the
# desktop file whose name matches the window's app id, not the window itself.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$dir"

write_entry() {
  # $1 = file name, $2 = extra line
  cat > "$dir/$1.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=TubeDesk
Comment=Audio-only YouTube Music client
Exec=$root/target/debug/tubedesk
Icon=$root/icons/icon.png
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
echo "installed $dir/dev.tubedesk.app.desktop (plus a hidden tubedesk.desktop alias)"
