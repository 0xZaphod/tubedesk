# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

TubeDesk: an audio-only YouTube Music desktop client. Tauri 2 shell, Rust core,
plain HTML/CSS/JS UI in `ui/` (no build step, no framework). It does not wrap
the YouTube Music website. The webview-wrapper predecessor is the git tag
`wrapper-final`.

## Commands

```sh
cargo build
cargo test                                   # a few unit tests (resolver); no linters set up
RUST_LOG=tubedesk=debug cargo run            # normal dev run
node --check ui/app.js                       # syntax-check the UI after edits
./scripts/build-appimage.sh                  # release AppImage (Windows/macOS: GitHub Actions, build.yml)

# Headless/silent test run: search, play the top results, no audio output
TUBEDESK_AUDIO_SINK=null RUST_LOG=tubedesk=debug \
  timeout 60 ./target/debug/tubedesk --play "daft punk get lucky"
```

Testing caveats:
- **Single instance:** if the user's TubeDesk is running, a second launch only
  focuses it and exits immediately. Check `pgrep -x tubedesk` first; don't
  kill the user's instance.
- **Real data dirs:** test runs share `~/.local/share/dev.tubedesk.app`
  (session, pins, home cache, account cookie) with the user. `--play`
  overwrites their remembered session. Isolate tests with
  `XDG_DATA_HOME=… XDG_CONFIG_HOME=… XDG_CACHE_HOME=…` pointing at a temp dir.
  The paths must be **absolute**: relative XDG paths are ignored and the run
  silently uses the real dirs (e.g. `mktemp -d -p "$PWD"`, not `-p .`).
- MPRIS can drive a running instance: `dbus-send --session --print-reply
  --dest=org.mpris.MediaPlayer2.tubedesk /org/mpris/MediaPlayer2
  org.mpris.MediaPlayer2.Player.{PlayPause,Next,Seek int64:…}`.
- AppImage: its launcher points `LD_LIBRARY_PATH`, `PYTHONHOME` etc. into the
  image; child processes must go through `appimage::host_env` or the system
  yt-dlp breaks ("<no Python frame>"). All helper programs are started via
  `process::host_command` (also: no console window on Windows, Homebrew's
  PATH on macOS).
- libopus: system copy via pkg-config on Linux; built from source with CMake
  elsewhere (`.cargo/config.toml` sets the CMake policy CMake 4 needs).
- Updates (`update.rs`, Tauri updater): releases are signed in CI with the
  `TAURI_SIGNING_PRIVATE_KEY(_PASSWORD)` repo secrets; the key pair lives in
  `~/.tauri/tubedesk.key*` (public key in `tauri.conf.json`; losing the
  private key strands installed copies). Local building needs both env vars
  (`createUpdaterArtifacts`). Test without a release: build the "old" app
  with `--config '{"plugins":{"updater":{"dangerousInsecureTransportProtocol":true}}}'`,
  serve a `latest.json` over http, run it with
  `TUBEDESK_UPDATE_URL=http://…/latest.json … --update` (installs and restarts).
- Player position is logged at trace level (`RUST_LOG=tubedesk::player=trace`).
- Anything that writes to the user's YouTube account (likes, playlist edits)
  must not be exercised in tests; only the read endpoints.

## Architecture

**Data vs. audio are separate paths.**
- Metadata, library and home feed come from `rustypipe` (YouTube Music's
  internal API). Things rustypipe doesn't wrap use `RustyPipeQuery::raw`:
  the home feed (`home.rs`, `FEmusic_home`) and account writes
  (`account.rs`: `like/*`, `browse/edit_playlist`,
  `playlist/get_add_to_playlist`).
- Audio: yt-dlp resolves stream URLs → local HTTP proxy (`proxy.rs`) →
  `player.rs`: HTTP range reader → symphonia's WebM demuxer → libopus →
  cpal. A player thread takes commands and sends ticks; each track has a
  decoder thread filling its buffer; the output callback drains the current
  track's buffer. A replaced track is just cancelled and never read again.

**Why the proxy exists (non-obvious YouTube behaviour, all measured):**
- googlevideo answers 403 to requests without a byte range, so the proxy
  fetches upstream in 1 MiB `&range=` chunks and honours the player's `Range`.
  Every new request costs an upstream chunk, so the player keeps the file's
  first 128 KiB (header, seek index) instead of re-reading it.
- A freshly issued yt-dlp URL is refused (403) for ~3 s, purely time-based;
  `fetch` retries.
- rustypipe's own stream URLs (iOS client, no PO token) are usable at once,
  but only for the first ~500 KB. They serve the same file as yt-dlp's
  (itag 251), so the proxy plays the first `QUICK_LIMIT` bytes from that
  "quick" URL while yt-dlp runs, then continues from yt-dlp's "full" URL.
  Both sides must request the same itag (`resolver.rs`), and sizes are checked
  before bytes are mixed.
- The next two queue tracks (not hover hints) are "warmed": their first
  1 MiB is fetched and kept. Warming and full-URL requests wait out the
  ~3.5 s refusal window instead of retrying into 403s.

**Core (`core.rs`) is a single actor** owning the queue and playback state.
Everything talks to it via `CoreHandle::send(CoreMsg)`: Tauri commands, the
MPRIS thread (`media.rs`), the tray, and the player thread's events. It emits
`state`, `queue` (contents changed) and `queue_index` (track changed) events
to the UI and persists `session.json` (queue, index, position) for
restore-on-start.
- Lookups run on `resolver::Lane`s: `Now` (the track the user waits for,
  unlimited), `Ahead` (2 slots: `prefetch_ahead`, a list's first track) and
  `Speculative` (1 slot, dropped when busy or stale: hover hints, prepared and
  pinned lists), so guesses never delay the next track. `prefetch_ahead`
  looks up both next tracks in one yt-dlp run (`resolver::resolve` takes ids).
- Stream URLs expire (their `expire=` parameter): the proxy asks the core for
  a new one (`CoreMsg::Refresh`) when a URL expired or keeps being refused.
- A list's later pages arrive as `Extend` tagged with the `PlayList` token;
  anything started in between invalidates it.
- Resuming at a position uses `PlayerCmd::LoadAt`: the decoder seeks before
  producing any audio, so the start isn't heard.
- Shuffle is a mode: `Core::order` keeps the pre-shuffle ids (also in
  `session.json`) so turning it off restores the order (`unshuffled`, tested).
  The Play / Shuffle buttons set the mode; clicking a song keeps it.
- Volume normalization: yt-dlp gives no loudness, rustypipe's player response
  does (`loudness_db`, from the quick lookup or `want_loudness`). Loud tracks
  are turned down only (`PlayerCmd::TrackGain`), like YouTube.
- After end of stream the player sends no ticks until the next load: a stale
  tick would mark the next (already prepared) track as loaded-but-paused.

**Auth:** account cookies live in rustypipe's cache
(`~/.local/share/dev.tubedesk.app/rustypipe_cache.json`, dir mode 0700).
`lib.rs::signed_query` uses the session when present. Playlists you own fail
to parse when fetched signed in, so `playlist_first_page` tries anonymous
first, then signed in, then the radio endpoint (personal `RDTMAK…` mixes only
exist as radios). Library calls in rustypipe authenticate themselves.

**UI (`ui/app.js`):** talks to Rust only via `invoke` and the `state` /
`queue` / `auth` events (`withGlobalTauri`). Remote strings are inserted with
`textContent` only, never `innerHTML` (CSP in `tauri.conf.json`). The window
is frameless; the custom title bar uses `data-tauri-drag-region`, and window
permissions are in `capabilities/default.json`. Sync Tauri commands run on the
main thread, so keep them trivial; anything doing I/O is `async`.

## Conventions

- Keep the README minimal (setup, configuration, design notes); don't add an
  entry for every feature.
- Config lives in `~/.config/dev.tubedesk.app/config.json` (`config.rs`,
  `#[serde(default)]`, so new keys need defaults).
- The icon is `icons/icon.svg` (original; never use YouTube's logo or red
  play-button look). After changing it: render a 1024 px PNG with
  `rsvg-convert`, run `cargo tauri icon` on it, copy the sizes listed in
  `tauri.conf.json` into `icons/` and the 128 px one to `ui/icon.png`.
