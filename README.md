# TubeDesk

A lightweight, audio-only YouTube Music desktop client built with Tauri 2 and
Rust. It has its own UI and plays audio natively; it does not wrap the YouTube
Music website.

> **Disclaimer:** TubeDesk is an unofficial, independent project. It is not
> affiliated with, endorsed by or sponsored by Google LLC or YouTube.
> "YouTube" and "YouTube Music" are trademarks of Google LLC. Use it at your
> own risk and in line with YouTube's terms of service.

## Requirements

[yt-dlp](https://github.com/yt-dlp/yt-dlp) and [deno](https://deno.com) (which
yt-dlp uses for YouTube's JavaScript challenges) must be installed:

| | |
|---|---|
| Arch | `sudo pacman -S yt-dlp deno` |
| Debian / Ubuntu | `pipx install yt-dlp`, deno from deno.com |
| Windows | `winget install yt-dlp.yt-dlp DenoLand.Deno` |
| macOS | `brew install yt-dlp deno` |

Keep `yt-dlp` up to date: it is the part that breaks when YouTube changes.

Building from source on Arch also needs `webkit2gtk-4.1 libayatana-appindicator
opus alsa-lib`; on Windows and macOS, CMake (libopus is built from source).

## Run

```sh
cargo run                                   # RUST_LOG=tubedesk=debug for details
./scripts/install-desktop-entry.sh          # taskbar icon / app menu entry (Wayland)
```

AppImage (yt-dlp and deno still come from the system):

```sh
cargo install tauri-cli --version "^2"
./scripts/build-appimage.sh
./scripts/install-desktop-entry.sh --appimage   # copies it to ~/.local/bin
```

Installed copies update themselves (Windows, macOS, AppImage; a `.deb`/`.rpm`
install only gets a link to the new release).

Windows and macOS builds are made by GitHub Actions (`.github/workflows/build.yml`):
push a `v*` tag for a draft release, or run the workflow by hand. The builds
are unsigned, so Windows SmartScreen and macOS Gatekeeper warn on first start
(macOS: right-click the app → Open).

## Sign-in

Use **Sign in** in the sidebar, which opens a private Google login window. The
session is stored in
`~/.local/share/dev.tubedesk.app/` (mode `0700`); treat it like a password.

## Configuration

The Settings page (keyboard shortcuts are listed there too). Everything is stored
in `~/.config/dev.tubedesk.app/config.json`; `login_user_agent` is only set
there.

## How it works

- **Data** (search, pages, library, home feed) comes from
  [rustypipe](https://codeberg.org/ThetaDev/rustypipe), YouTube Music's
  internal API; likes and playlist edits use the same API's raw endpoints.
- **Audio** is decoded in Rust (WebM via symphonia, Opus via libopus, output
  via cpal) from a local proxy (`src/proxy.rs`):
  - Full stream URLs come from yt-dlp. YouTube rejects requests without a byte
    range and refuses new URLs for ~3 s, so the proxy fetches ranged chunks
    and retries.
  - To start instantly, the first ~450 KB come from rustypipe's iOS URL for
    the same file, which works immediately but only for that first part.
  - Upcoming tracks are resolved and their start cached in advance.
- `--play "<query>"` and `TUBEDESK_AUDIO_SINK=null` (silent output) help with
  testing.

The earlier webview-wrapper version is tagged `wrapper-final`.

## License

GPL-3.0-or-later (rustypipe is GPL-3.0). The icon (`icons/icon.svg`) is
original to this project.
