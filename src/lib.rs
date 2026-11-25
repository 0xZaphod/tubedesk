mod account;
mod auth;
mod config;
mod core;
mod home;
mod media;
mod player;
mod proxy;
mod resolver;

use crate::core::{Control, CoreConfig, CoreHandle, CoreMsg};
use config::Config;
use rustypipe::client::RustyPipe;
use rustypipe::model::{Thumbnail, TrackItem};
use serde::Serialize;
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State, WindowEvent};

const TRAY_ID: &str = "main-tray";
/// Upper bound for tracks loaded from one playlist / library list.
const LIST_LIMIT: usize = 1000;

struct App {
    rp: RustyPipe,
    core: CoreHandle,
    config: Mutex<Config>,
    /// Home sections loaded since the last first-page load; saved to disk so
    /// the next start can show them instantly.
    home: Mutex<Vec<home::HomeSection>>,
    home_cache_path: std::path::PathBuf,
    /// Lists prepared on hover, keyed by "kind:id".
    prepared: Mutex<std::collections::HashMap<String, ListStart>>,
    pins_path: std::path::PathBuf,
    /// Serialises read-modify-write of the pins file.
    pins_lock: Mutex<()>,
}

impl App {
    /// A query that carries the account session when signed in, so personal
    /// content (mixes, private playlists, personalised results) works.
    fn q(&self) -> rustypipe::client::RustyPipeQuery {
        signed_query(&self.rp)
    }
}

pub fn signed_query(rp: &RustyPipe) -> rustypipe::client::RustyPipeQuery {
    let q = rp.query();
    if auth::has_session(rp) { q.authenticated() } else { q }
}

type Query = rustypipe::client::RustyPipeQuery;

/// Loads a playlist's first page, trying in order:
/// 1. anonymous: works for public/unlisted playlists, including your own
///    (rustypipe can't parse the signed-in page of playlists you *own*),
/// 2. signed in: private playlists,
/// 3. as a radio: personal mixes (`RDTMAK…`) only exist as radios.
///
/// Returns the playlist (if any) and the query that worked, for paging on.
async fn playlist_first_page(q: &Query, id: &str) -> Res<(Option<rustypipe::model::MusicPlaylist>, Vec<TrackItem>, Query)> {
    if !id.starts_with("RDTMAK") {
        let anon = q.clone().unauthenticated();
        let mut last = String::new();
        for (label, query) in [("anonymous", anon), ("signed-in", q.clone())] {
            match query.music_playlist(id).await {
                Ok(p) => {
                    let tracks = p.tracks.items.clone();
                    return Ok((Some(p), tracks, query));
                }
                Err(e) => {
                    log::debug!("{label} playlist {id} failed: {e}");
                    last = e.to_string();
                }
            }
        }
        if !id.starts_with("RD") {
            return Err(last);
        }
    }
    Ok((None, q.music_radio(id).await.map_err(err)?.items, q.clone()))
}

/// All tracks of a playlist (up to LIST_LIMIT), for the playlist page.
async fn playlist_tracks(q: &Query, id: &str) -> Res<(Option<rustypipe::model::MusicPlaylist>, Vec<TrackItem>)> {
    let (mut meta, mut tracks, query) = playlist_first_page(q, id).await?;
    if let Some(p) = meta.as_mut() {
        p.tracks.extend_limit(&query, LIST_LIMIT).await.map_err(err)?;
        tracks = std::mem::take(&mut p.tracks.items);
    }
    Ok((meta, tracks))
}

/// The start of a playable list: enough to begin playback immediately, plus
/// what is needed to load the remaining pages afterwards.
#[derive(Clone)]
struct ListStart {
    tracks: Vec<TrackItem>,
    /// The same first page in shuffled order, decided in advance so its first
    /// track can be prefetched before Shuffle is clicked.
    shuffled: Vec<TrackItem>,
    rest: Option<(rustypipe::model::paginator::Paginator<TrackItem>, Query)>,
    loaded: std::time::Instant,
}

async fn list_start(q: &Query, kind: &str, id: &str) -> Res<ListStart> {
    let (tracks, rest) = match kind {
        "playlist" => {
            let (meta, tracks, query) = playlist_first_page(q, id).await?;
            let rest = meta.filter(|p| !p.tracks.is_exhausted()).map(|p| (p.tracks, query));
            (tracks, rest)
        }
        "album" => {
            let a = album_data(q, id).await?;
            let cover = a.cover.clone();
            let tracks = a
                .tracks
                .into_iter()
                .map(|mut t| {
                    if t.cover.is_empty() {
                        t.cover = cover.clone();
                    }
                    t
                })
                .collect();
            (tracks, None)
        }
        "mix" => (q.music_radio(id).await.map_err(err)?.items, None),
        other => return Err(format!("can't play a {other}")),
    };
    if tracks.is_empty() {
        return Err("nothing to play".into());
    }
    let mut shuffled = tracks.clone();
    shuffle(&mut shuffled);
    Ok(ListStart { tracks, shuffled, rest, loaded: std::time::Instant::now() })
}

/// Fisher–Yates shuffle with a time-seeded xorshift (no need for a crypto RNG).
fn shuffle<T>(items: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9e3779b97f4a7c15, |d| d.as_nanos() as u64)
        | 1;
    for i in (1..items.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        items.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

type Res<T> = Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Largest thumbnail up to ~600px wide (YouTube lists them small → large).
pub fn best_thumb(thumbs: &[Thumbnail]) -> Option<String> {
    thumbs
        .iter()
        .filter(|t| t.width <= 600)
        .max_by_key(|t| t.width)
        .or(thumbs.last())
        .map(|t| t.url.clone())
}

pub fn update_tray_title(app: &AppHandle, track: Option<&TrackItem>) {
    let tip = match track {
        Some(t) => {
            let artists = t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
            if artists.is_empty() { t.name.clone() } else { format!("{} — {artists}", t.name) }
        }
        None => "TubeDesk".into(),
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(&tip));
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_title(&tip);
    }
}

// ---- browsing --------------------------------------------------------------

#[derive(Serialize)]
struct SearchResults {
    tracks: Vec<TrackItem>,
    albums: Vec<rustypipe::model::AlbumItem>,
    artists: Vec<rustypipe::model::ArtistItem>,
    playlists: Vec<rustypipe::model::MusicPlaylistItem>,
}

#[tauri::command]
async fn search(app: State<'_, App>, query: String) -> Res<SearchResults> {
    let q = app.q();
    let (tracks, albums, artists, playlists) = tokio::join!(
        q.music_search_tracks(&query),
        q.music_search_albums(&query),
        q.music_search_artists(&query),
        q.music_search_playlists(&query, false),
    );
    Ok(SearchResults {
        tracks: tracks.map_err(err)?.items.items,
        albums: albums.map(|r| r.items.items).unwrap_or_default(),
        artists: artists.map(|r| r.items.items).unwrap_or_default(),
        playlists: playlists.map(|r| r.items.items).unwrap_or_default(),
    })
}

/// The signed-in user's YouTube Music library.
#[tauri::command]
async fn library(app: State<'_, App>, kind: String) -> Res<serde_json::Value> {
    let q = app.rp.query();
    let v = match kind.as_str() {
        "liked" => {
            let mut p = q.music_liked_tracks().await.map_err(err)?;
            p.tracks.extend_limit(&q, LIST_LIMIT).await.map_err(err)?;
            serde_json::to_value(p.tracks.items)
        }
        "playlists" => {
            let mut p = q.music_saved_playlists().await.map_err(err)?;
            p.extend_limit(&q, LIST_LIMIT).await.map_err(err)?;
            serde_json::to_value(p.items)
        }
        "albums" => {
            let mut p = q.music_saved_albums().await.map_err(err)?;
            p.extend_limit(&q, LIST_LIMIT).await.map_err(err)?;
            serde_json::to_value(p.items)
        }
        "artists" => {
            let mut p = q.music_saved_artists().await.map_err(err)?;
            p.extend_limit(&q, LIST_LIMIT).await.map_err(err)?;
            serde_json::to_value(p.items)
        }
        "history" => {
            let p = q.music_history().await.map_err(err)?;
            serde_json::to_value(p.items.into_iter().map(|h| h.item).collect::<Vec<_>>())
        }
        other => return Err(format!("unknown library section {other}")),
    };
    v.map_err(err)
}

/// Playlist page data. For mixes read as radios there is no playlist header,
/// so name/thumbnail are null and the UI uses what the card showed.
#[tauri::command]
async fn playlist(app: State<'_, App>, id: String) -> Res<serde_json::Value> {
    let (meta, tracks) = playlist_tracks(&app.q(), &id).await?;
    Ok(serde_json::json!({
        "id": id,
        "name": meta.as_ref().map(|p| p.name.clone()),
        "thumbnail": meta.as_ref().map(|p| p.thumbnail.clone()).unwrap_or_default(),
        "channel": meta.as_ref().and_then(|p| p.channel.clone()),
        "track_count": meta.as_ref().and_then(|p| p.track_count).unwrap_or(tracks.len() as u64),
        "tracks": tracks,
    }))
}

#[tauri::command]
async fn album(app: State<'_, App>, id: String) -> Res<rustypipe::model::MusicAlbum> {
    album_data(&app.q(), &id).await
}

/// Signed-in first, anonymous as fallback (see `playlist_tracks`).
async fn album_data(q: &rustypipe::client::RustyPipeQuery, id: &str) -> Res<rustypipe::model::MusicAlbum> {
    match q.music_album(id).await {
        Ok(a) => Ok(a),
        Err(e) => {
            log::debug!("signed-in album {id} failed: {e}; retrying anonymously");
            q.clone().unauthenticated().music_album(id).await.map_err(err)
        }
    }
}

#[tauri::command]
async fn artist(app: State<'_, App>, id: String) -> Res<rustypipe::model::MusicArtist> {
    let q = app.q();
    match q.music_artist(&id, false).await {
        Ok(a) => Ok(a),
        Err(e) => {
            log::debug!("signed-in artist {id} failed: {e}; retrying anonymously");
            q.unauthenticated().music_artist(&id, false).await.map_err(err)
        }
    }
}

/// The home page as it was last loaded (instant, possibly stale).
#[tauri::command]
async fn home_cached(app: State<'_, App>) -> Res<Vec<home::HomeSection>> {
    Ok(std::fs::read(&app.home_cache_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default())
}

/// One page of the live home feed; `token` = None starts over.
#[tauri::command]
async fn home_page(app: State<'_, App>, token: Option<String>) -> Res<home::HomePage> {
    let first = token.is_none();
    let page = home::load_page(&app.rp, token).await?;
    let mut all = app.home.lock().unwrap();
    if first {
        all.clear();
    }
    all.extend(page.sections.iter().cloned());
    if let Ok(json) = serde_json::to_vec(&*all) {
        let _ = config::write_atomic(&app.home_cache_path, &json);
    }
    Ok(page)
}

// ---- playback --------------------------------------------------------------

/// Prepared list starts are reused for this long.
const PREPARED_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// Hint that a list may be played soon (the UI calls this on card hover):
/// loads its first page and prefetches the first track of both the normal
/// and the shuffled order, so Play and Shuffle start immediately.
#[tauri::command]
async fn prepare_list(app: State<'_, App>, kind: String, id: String) -> Res<()> {
    let key = format!("{kind}:{id}");
    if app.prepared.lock().unwrap().get(&key).is_some_and(|p| p.loaded.elapsed() < PREPARED_TTL) {
        return Ok(());
    }
    let start = list_start(&app.q(), &kind, &id).await?;
    // Speculative (a hover or a pinned list): dropped while lookups pile up.
    app.core.send(CoreMsg::Prefetch { id: start.tracks[0].id.clone(), priority: false });
    app.core.send(CoreMsg::Prefetch { id: start.shuffled[0].id.clone(), priority: false });
    let mut prepared = app.prepared.lock().unwrap();
    prepared.retain(|_, p| p.loaded.elapsed() < PREPARED_TTL);
    prepared.insert(key, start);
    Ok(())
}

/// Plays (optionally shuffled) a playlist, album or mix by id. Playback starts
/// with the first page; further pages are appended in the background.
#[tauri::command]
async fn play_list(app: State<'_, App>, kind: String, id: String, shuffle: bool) -> Res<()> {
    let key = format!("{kind}:{id}");
    let cached = app.prepared.lock().unwrap().get(&key).filter(|p| p.loaded.elapsed() < PREPARED_TTL).cloned();
    let start = match cached {
        Some(s) => s,
        None => list_start(&app.q(), &kind, &id).await?,
    };
    // Keep the list prepared for the next Play/Shuffle. A used shuffle order
    // is replaced by a fresh one right away, and its first song prepared, so
    // the *next* Shuffle is instant too.
    {
        let mut next = start.clone();
        if shuffle {
            next.shuffled = next.tracks.clone();
            crate::shuffle(&mut next.shuffled);
            app.core.send(CoreMsg::Prefetch { id: next.shuffled[0].id.clone(), priority: false });
        }
        app.prepared.lock().unwrap().insert(key, next);
    }
    let first = if shuffle { start.shuffled } else { start.tracks };
    let (token, first_len) = (crate::core::list_token(), first.len());
    app.core.send(CoreMsg::PlayList { tracks: first, shuffled: shuffle, token });

    if let Some((mut pages, query)) = start.rest {
        let core = app.core.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = pages.extend_limit(&query, LIST_LIMIT).await {
                log::warn!("loading the rest of {id} failed: {e}");
            }
            // A shuffled list's rest is shuffled in by the core.
            let rest: Vec<TrackItem> = pages.items.into_iter().skip(first_len).collect();
            if !rest.is_empty() {
                core.send(CoreMsg::Extend { token, tracks: rest });
            }
        });
    }
    Ok(())
}

#[tauri::command]
fn play(app: State<'_, App>, tracks: Vec<TrackItem>, start: usize) {
    app.core.send(CoreMsg::PlayTracks { tracks, start });
}

#[tauri::command]
fn enqueue(app: State<'_, App>, tracks: Vec<TrackItem>, next: bool) {
    app.core.send(if next { CoreMsg::PlayNext(tracks) } else { CoreMsg::Enqueue(tracks) });
}

#[tauri::command]
fn queue_jump(app: State<'_, App>, index: usize) {
    app.core.send(CoreMsg::JumpTo(index));
}

#[tauri::command]
fn queue_remove(app: State<'_, App>, index: usize) {
    app.core.send(CoreMsg::Remove(index));
}

/// Hint that a track may be played soon: on hover (`priority` false) or as
/// the first track of an opened list (`priority` true).
#[tauri::command]
fn prefetch(app: State<'_, App>, id: String, priority: Option<bool>) {
    app.core.send(CoreMsg::Prefetch { id, priority: priority.unwrap_or(false) });
}

// ---- account actions -----------------------------------------------------------

#[tauri::command]
async fn track_rating(app: State<'_, App>, id: String) -> Res<String> {
    account::rating(&app.rp, &id).await
}

#[tauri::command]
async fn rate_track(app: State<'_, App>, id: String, rating: String) -> Res<()> {
    account::rate(&app.rp, &id, &rating).await
}

#[tauri::command]
async fn playlist_options(app: State<'_, App>, id: String) -> Res<Vec<account::PlaylistOption>> {
    account::playlist_options(&app.rp, &id).await
}

#[tauri::command]
async fn remove_from_playlist(app: State<'_, App>, playlist_id: String, video_id: String) -> Res<()> {
    account::remove_from_playlist(&app.rp, &playlist_id, &video_id).await
}

#[tauri::command]
async fn add_to_playlist(app: State<'_, App>, playlist_id: String, video_id: String) -> Res<bool> {
    account::add_to_playlist(&app.rp, &playlist_id, &video_id).await
}

// ---- pins --------------------------------------------------------------------

fn load_pins(app: &App) -> Vec<home::HomeItem> {
    std::fs::read(&app.pins_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_pins(app: &App, pins: &[home::HomeItem]) -> Res<()> {
    let json = serde_json::to_vec_pretty(pins).map_err(err)?;
    config::write_atomic(&app.pins_path, &json).map_err(err)
}

/// Your own pinned songs, playlists, albums and mixes (stored locally).
// File I/O: async commands, so it stays off the main thread.
#[tauri::command]
async fn pins(app: State<'_, App>) -> Res<Vec<home::HomeItem>> {
    Ok(load_pins(&app))
}

/// Pins an item at the front (re-pinning moves it there). Returns all pins.
#[tauri::command]
async fn pin(app: State<'_, App>, item: home::HomeItem) -> Res<Vec<home::HomeItem>> {
    let _guard = app.pins_lock.lock().unwrap();
    let mut pins = load_pins(&app);
    pins.retain(|p| !(p.kind == item.kind && p.id == item.id));
    pins.insert(0, item);
    save_pins(&app, &pins)?;
    Ok(pins)
}

#[tauri::command]
async fn unpin(app: State<'_, App>, kind: String, id: String) -> Res<Vec<home::HomeItem>> {
    let _guard = app.pins_lock.lock().unwrap();
    let mut pins = load_pins(&app);
    pins.retain(|p| !(p.kind == kind && p.id == id));
    save_pins(&app, &pins)?;
    Ok(pins)
}

#[tauri::command]
fn queue_move(app: State<'_, App>, from: usize, to: usize) {
    app.core.send(CoreMsg::Move { from, to });
}

#[tauri::command]
fn queue_clear(app: State<'_, App>) {
    app.core.send(CoreMsg::Clear);
}

#[tauri::command]
fn control(app: State<'_, App>, action: Control) {
    log::debug!("ui control {action:?}");
    app.core.send(CoreMsg::Control(action));
}

#[tauri::command]
fn seek(app: State<'_, App>, position: f64) {
    app.core.send(CoreMsg::SeekTo(position));
}

#[tauri::command]
async fn set_volume(app: State<'_, App>, volume: f64, persist: bool) -> Res<()> {
    app.core.send(CoreMsg::Volume(volume));
    if persist {
        let mut cfg = app.config.lock().unwrap();
        cfg.volume = volume.clamp(0.0, 1.0);
        cfg.save();
    }
    Ok(())
}

/// Re-sends the current `state` and `queue` events (UI start-up).
#[tauri::command]
fn sync(app: State<'_, App>) {
    app.core.send(CoreMsg::Sync);
}

// ---- account ---------------------------------------------------------------

/// Local check only (is a session stored?). Verifying it online took a whole
/// request at every start; an expired session shows up as library errors.
#[tauri::command]
fn auth_status(app: State<'_, App>) -> auth::AuthStatus {
    auth::AuthStatus { logged_in: auth::has_session(&app.rp), error: None }
}

#[tauri::command]
fn auth_login(handle: AppHandle, app: State<'_, App>) -> Res<()> {
    let ua = app.config.lock().unwrap().login_user_agent.clone();
    auth::open_login_window(&handle, app.rp.clone(), ua).map_err(err)
}

#[tauri::command]
async fn auth_import(handle: AppHandle, app: State<'_, App>, cookies: String) -> Res<()> {
    auth::import_cookies_txt(&app.rp, &cookies).await?;
    auth::emit(&handle, true, None);
    Ok(())
}

#[tauri::command]
async fn auth_logout(handle: AppHandle, app: State<'_, App>) -> Res<()> {
    auth::logout(&app.rp).await;
    auth::emit(&handle, false, None);
    Ok(())
}

// ---- app -------------------------------------------------------------------

fn build_tray(app: &AppHandle, core: CoreHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Play / Pause", true, None::<&str>)?;
    let next = MenuItem::with_id(app, "next", "Next", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "previous", "Previous", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show / Hide window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(app, &[&toggle, &next, &prev, &sep()?, &show, &sep()?, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().expect("bundled icon"))
        .tooltip("TubeDesk")
        .menu(&menu)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "toggle" => core.send(CoreMsg::Control(Control::Toggle)),
            "next" => core.send(CoreMsg::Control(Control::Next)),
            "previous" => core.send(CoreMsg::Control(Control::Previous)),
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    if w.is_visible().unwrap_or(false) {
                        let _ = w.hide();
                    } else {
                        media::show_window(app);
                    }
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let config = Config::load_or_init(&app.path().app_config_dir()?.join("config.json"));

    // rustypipe keeps its cache *and the account cookie* here: owner-only.
    let data_dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&data_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let rp = RustyPipe::builder().storage_dir(&data_dir).build()?;

    let proxy = tauri::async_runtime::block_on(proxy::StreamProxy::start())?;

    let (core_tx, core_rx) = std::sync::mpsc::channel();
    let media_tx = if config.media_controls { media::spawn(handle.clone(), core_rx) } else { None };
    let core = core::spawn(
        handle.clone(),
        rp.clone(),
        proxy,
        media_tx,
        CoreConfig {
            volume: config.volume,
            autoplay_radio: config.autoplay_radio,
            high_quality: config.high_quality,
            ytdlp: config.ytdlp.clone(),
            session_path: data_dir.join("session.json"),
        },
    )?;
    let _ = core_tx.send(core.clone());

    let window = app.get_webview_window("main").expect("main window from tauri.conf.json");
    if config.hide_on_close {
        let w = window.clone();
        window.on_window_event(move |e| {
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w.hide();
            }
        });
    }

    // `tubedesk --play "query"`: search and play the top results right away.
    let args: Vec<String> = std::env::args().collect();
    if let Some(query) = args.iter().position(|a| a == "--play").and_then(|i| args.get(i + 1)).cloned() {
        let (rp, core) = (rp.clone(), core.clone());
        tauri::async_runtime::spawn(async move {
            match rp.query().music_search_tracks(&query).await {
                Ok(r) => core.send(CoreMsg::PlayTracks { tracks: r.items.items, start: 0 }),
                Err(e) => log::warn!("--play search failed: {e}"),
            }
        });
    }

    build_tray(&handle, core.clone())?;
    app.manage(App {
        rp,
        core,
        config: Mutex::new(config),
        home: Mutex::default(),
        home_cache_path: data_dir.join("home_cache.json"),
        prepared: Mutex::default(),
        pins_path: data_dir.join("pins.json"),
        pins_lock: Mutex::default(),
    });
    Ok(())
}

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("tubedesk=info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| media::show_window(app)))
        .invoke_handler(tauri::generate_handler![
            home_cached, home_page, search, library, playlist, album, artist,
            play, play_list, prepare_list, enqueue, queue_jump, queue_remove, queue_move, queue_clear, prefetch, pins, pin, unpin, track_rating, rate_track, playlist_options, add_to_playlist, remove_from_playlist, control, seek, set_volume, sync,
            auth_status, auth_login, auth_import, auth_logout,
        ])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("error while running TubeDesk");
}
