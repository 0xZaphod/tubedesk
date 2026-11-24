# TubeDesk

A lightweight, audio-only YouTube Music desktop player built on Tauri 2 / Rust.
It uses the system webview (WebKitGTK on Linux, WebView2 on Windows, WKWebView on
macOS) instead of a bundled Chromium.

## Features

- **YouTube Music, audio-only**: music videos switch to their "Song" version when
  YouTube Music has one. Otherwise the video is hidden behind the album art and
  pinned to the lowest quality.
- **uBlock-Origin-style ad blocking without the extension** (Brave's `adblock` crate):
  - uBO + EasyList + EasyPrivacy filter lists, downloaded and cached, refreshed every 4 days
  - uBO scriptlets (`##+js(...)`): strip ads out of YouTube's player responses
  - cosmetic CSS and procedural filters (`:has-text`, `:upward`, `:remove()`, …)
  - network blocking via WebKit content filters (Linux), compiled once and cached
  - a last-resort skipper that mutes and fast-forwards any ad that gets through
- **Media controls**: MPRIS on Linux (media keys, desktop widgets), with title,
  artist and artwork
- **Tray**: play/pause, next, previous, show/hide, update filters, quit. Closing
  the window hides it to the tray.
- Single instance: launching it again just shows the window

## Setup

System packages (Arch):

```sh
sudo pacman -S webkit2gtk-4.1 libayatana-appindicator gst-plugins-good gst-plugins-bad gst-libav
```

### 1. Generate the uBO scriptlets (required for ad stripping)

```sh
node scripts/build-scriptlets.mjs                # clones uBO at a pinned tag
node scripts/build-scriptlets.mjs ~/src/uBlock   # or use a local checkout
```

This imports uBlock Origin's scriptlet modules with Node and writes
`resources/ubo-scriptlets.json`, which gets compiled into the binary. Until you
run it, the app starts with a warning and only network, cosmetic and fallback
blocking work. Re-run it (bumping `UBO_TAG`) when uBO releases scriptlet fixes.

### 2. Build and run

```sh
cargo run                # debug
cargo build --release    # optimized, ~10 MB binary
RUST_LOG=tubedesk=debug cargo run   # log player state / blocking details
```

For installable bundles (`.deb`, `.rpm`, AppImage), install the Tauri CLI
(`cargo install tauri-cli --version "^2"`) and run `cargo tauri build`.

## Configuration

`~/.config/dev.tubedesk.app/config.json` (created on first run):

| key | default | |
|---|---|---|
| `audio_only` | `true` | never show video |
| `adblock` | `true` | master switch for all blocking |
| `network_filtering` | `true` | WebKit content filters (Linux) |
| `media_controls` | `true` | MPRIS; disable if your desktop shows a duplicate player |
| `close_to_tray` | `true` | close button hides the window |
| `filter_max_age_hours` | `96` | filter list refresh interval |
| `user_agent` | `null` | override the webview user agent |

Filter lists are cached in `~/.cache/dev.tubedesk.app/filters/`. Compiled WebKit
filters are in `~/.local/share/dev.tubedesk.app/content-filters/`.

## How it works

```
src/lib.rs              app setup, window, tray, IPC command
src/filters.rs          filter list download/cache
src/adblock.rs          adblock-rust engine → init script + WebKit rules
src/netfilter_linux.rs  WebKitUserContentFilterStore FFI
src/media.rs            souvlaki media controls thread
js/bridge.js            injected: state reporting, controls, audio-only, ad fallback
js/procedural.js        injected: procedural cosmetic filter executor
scripts/build-scriptlets.mjs  uBO scriptlets → adblock-rust resources
```

The injected scripts reach Rust only through the `player_state` command.
`capabilities/music.json` grants that one command to `https://music.youtube.com/*`
and nothing else.

## Known limitations / TODO

- Network blocking is Linux-only for now. macOS (`WKContentRuleListStore`, same JSON)
  and Windows (WebView2 `WebResourceRequested`) are marked TODO in `src/lib.rs`.
- Generic cosmetic filters (`##.ad-class` with no domain) are not applied. Only
  rules that match music.youtube.com are.
- Ad blocking on YouTube is an arms race. Server-side ad insertion would defeat
  the scriptlets and leave only the fallback skipper.
- Google sign-in may refuse embedded webviews. If it does, try setting `user_agent`.
- Windows SMTC needs the window HWND passed to souvlaki (TODO in `src/media.rs`).

## License

GPL-3.0-or-later. The generated `resources/ubo-scriptlets.json` is derived from
uBlock Origin (GPL-3.0).
