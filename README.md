# TubeDesk

A lightweight, audio-only YouTube Music desktop client built with Tauri 2 / Rust.
It doesn't wrap the YouTube Music site. The UI is its own, playback runs
natively in Rust, and it downloads only audio streams. With no YouTube page
loaded, there are no ads to block.

## Features

- Search songs, albums, artists and playlists; artist, album and playlist pages
- **Your YouTube Music library**: liked songs, saved playlists, albums,
  artists and history (sign-in required)
- Audio-only playback through GStreamer: Opus, best or smallest stream
- Queue with play next, add to queue and remove. When the queue runs out it
  continues with YouTube Music's radio for the last track.
- The next track is resolved in advance, so skipping is instant
- MPRIS media controls (media keys and desktop widgets), tray menu, close to tray
- Music keeps playing with the window hidden

## Requirements (Arch)

```sh
sudo pacman -S webkit2gtk-4.1 libayatana-appindicator \
  gstreamer gst-plugins-base gst-plugins-good gst-libav \
  yt-dlp deno
```

`yt-dlp` resolves the audio streams, and `deno` lets it solve YouTube's
JavaScript challenges. Keep yt-dlp up to date. When YouTube changes things,
that's the part that breaks first.

## Run

```sh
cargo run
RUST_LOG=tubedesk=debug cargo run                 # verbose logging
cargo run -- --play "daft punk get lucky"         # search and play right away
TUBEDESK_AUDIO_SINK="fakesink sync=true" cargo run -- --play "…"   # silent test
cargo build --release
```

Installable bundles: `cargo install tauri-cli --version "^2"` then `cargo tauri build`.

## Signing in

Under **Sign in** in the sidebar:

1. **Sign in with Google** opens a private login window. Once Google
   redirects back, the app copies the YouTube session cookies and closes the
   window, so nothing else keeps using (and rotating) that session.
2. **Import cookies.txt** is for when Google refuses to sign in inside the
   app. Sign in at music.youtube.com in a *private/incognito* window of your
   normal browser, export the youtube.com cookies in Netscape `cookies.txt`
   format, **close that private window**, then import the file.

The session cookie is stored by rustypipe in
`~/.local/share/dev.tubedesk.app/rustypipe_cache.json` (the directory is
`0700`). Treat it like a password. Signing out removes it.

## Configuration

`~/.config/dev.tubedesk.app/config.json`:

| key | default | |
|---|---|---|
| `high_quality` | `true` | best Opus stream (~130–160 kbps) vs the smallest |
| `autoplay_radio` | `true` | continue with radio when the queue ends |
| `volume` | `0.8` | remembered volume |
| `media_controls` | `true` | MPRIS / media keys |
| `close_to_tray` | `true` | close button hides the window |
| `login_user_agent` | `null` | user agent for the Google login window |
| `ytdlp` | `"yt-dlp"` | yt-dlp executable |

## How it works

```
ui/                   HTML/CSS/JS frontend (no build step)
src/lib.rs            Tauri commands, tray, setup
src/core.rs           queue, playback orchestration, radio, prefetch
src/player.rs         GStreamer playbin thread (audio only)
src/resolver.rs       yt-dlp → audio stream URL + required headers
src/proxy.rs          local 127.0.0.1 proxy GStreamer streams from
src/auth.rs           login window / cookies.txt → rustypipe cookie auth
src/media.rs          souvlaki MPRIS thread
```

- **Metadata and library** come from [rustypipe](https://codeberg.org/ThetaDev/rustypipe)
  (the YouTube Music internal API).
- **Streams** come from yt-dlp. rustypipe's own stream URLs get cut off by
  YouTube after about 1 MB without a PO token (that would need the separate
  `rustypipe-botguard` binary).
- **The proxy exists** because googlevideo.com refuses (403) requests without
  a byte range, throttles big ones, and refuses brand-new URLs for about 2 s.
  It fetches in 1 MiB ranged chunks with yt-dlp's headers, retries those early
  403s, and honours GStreamer's `Range` requests for seeking.

The UI only inserts remote text with `textContent`, and the CSP allows no
remote scripts.

The previous webview-wrapper version (music.youtube.com with uBO-style ad
blocking) is tagged `wrapper-final`.

## Known limitations / TODO

- Startup of the *first* track takes ~5 s (yt-dlp, then YouTube's 2 s
  warm-up). Later tracks are prefetched.
- No liking or unliking and no playlist editing yet. rustypipe only reads the
  library.
- Google may refuse the embedded login window; use cookies.txt import.
- Windows media controls need the window HWND (TODO in `src/media.rs`).

## License

GPL-3.0-or-later (rustypipe is GPL-3.0).
