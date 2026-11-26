# TubeDesk

A lightweight, audio-only YouTube Music desktop client built with Tauri 2 and
Rust. It has its own UI and plays audio natively; it does not wrap the YouTube
Music website.

## Requirements (Arch)

```sh
sudo pacman -S webkit2gtk-4.1 libayatana-appindicator \
  gstreamer gst-plugins-base gst-plugins-good gst-libav yt-dlp deno
```

Keep `yt-dlp` up to date: it is the part that breaks when YouTube changes.

## Run

```sh
cargo run                                   # RUST_LOG=tubedesk=debug for details
./scripts/install-desktop-entry.sh          # taskbar icon / app menu entry (Wayland)
```

AppImage (yt-dlp and deno still come from the system):

```sh
cargo install tauri-cli --version "^2"
./scripts/build-appimage.sh                     # bundles only the GStreamer plugins it needs
./scripts/install-desktop-entry.sh --appimage   # copies it to ~/.local/bin
```

## Sign-in

Use **Sign in** in the sidebar, which opens a private Google login window. The
session is stored in
`~/.local/share/dev.tubedesk.app/` (mode `0700`); treat it like a password.

## Configuration

`~/.config/dev.tubedesk.app/config.json`: `high_quality`, `autoplay_radio`,
`volume`, `media_controls`, `hide_on_close`, `login_user_agent`, `ytdlp`.

## How it works

- **Data** (search, pages, library, home feed) comes from
  [rustypipe](https://codeberg.org/ThetaDev/rustypipe), YouTube Music's
  internal API; likes and playlist edits use the same API's raw endpoints.
- **Audio** is played by GStreamer through a local proxy (`src/proxy.rs`):
  - Full stream URLs come from yt-dlp. YouTube rejects requests without a byte
    range and refuses new URLs for ~3 s, so the proxy fetches ranged chunks
    and retries.
  - To start instantly, the first ~450 KB come from rustypipe's iOS URL for
    the same file, which works immediately but only for that first part.
  - Upcoming tracks are resolved and their start cached in advance.
- `--play "<query>"` and `TUBEDESK_AUDIO_SINK="fakesink sync=true"` help with
  testing.

The earlier webview-wrapper version is tagged `wrapper-final`.

## License

GPL-3.0-or-later (rustypipe is GPL-3.0). The bundled icon is rendered from an
icon theme's YouTube Music logo (YouTube's trademark): replace it before
publishing.
