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
./scripts/build-appimage.sh                  # release AppImage (~110 MB), curated GStreamer plugins

# Headless/silent test run: search, play the top results, no audio output
TUBEDESK_AUDIO_SINK="fakesink sync=true" RUST_LOG=tubedesk=debug \
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
- MPRIS can drive a running instance: `dbus-send --session --print-reply
  --dest=org.mpris.MediaPlayer2.tubedesk /org/mpris/MediaPlayer2
  org.mpris.MediaPlayer2.Player.{PlayPause,Next,Seek int64:…}`.
- AppImage: its launcher points `LD_LIBRARY_PATH`, `PYTHONHOME` etc. into the
  image; child processes must go through `appimage::host_env` or the system
  yt-dlp breaks ("<no Python frame>"). Its mount path changes every start, so
  `appimage::fix_environment` gives GStreamer a stable symlink + own registry
  (otherwise a full plugin rescan on every launch).
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
  GStreamer `playbin` (`player.rs`, its own thread).

**Why the proxy exists (non-obvious YouTube behaviour, all measured):**
- googlevideo answers 403 to requests without a byte range, so the proxy
  fetches upstream in 1 MiB `&range=` chunks and honours GStreamer's `Range`.
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
- Resuming at a position uses `PlayerCmd::LoadAt`: preroll paused, seek, then
  play, so the start isn't heard.

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
- The bundled icon is rendered from an icon theme's YouTube Music logo
  (trademark): fine locally, not for publishing.
