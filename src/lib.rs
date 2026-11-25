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
    let q = app.rp.query();
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

#[tauri::command]
async fn playlist(app: State<'_, App>, id: String) -> Res<rustypipe::model::MusicPlaylist> {
    let q = app.rp.query();
    let mut p = q.music_playlist(&id).await.map_err(err)?;
    p.tracks.extend_limit(&q, LIST_LIMIT).await.map_err(err)?;
    Ok(p)
}

#[tauri::command]
async fn album(app: State<'_, App>, id: String) -> Res<rustypipe::model::MusicAlbum> {
    app.rp.query().music_album(&id).await.map_err(err)
}

#[tauri::command]
async fn artist(app: State<'_, App>, id: String) -> Res<rustypipe::model::MusicArtist> {
    app.rp.query().music_artist(&id, false).await.map_err(err)
}

#[tauri::command]
async fn home(app: State<'_, App>) -> Res<Vec<home::HomeSection>> {
    home::load(&app.rp).await
}

// ---- playback --------------------------------------------------------------

/// Plays a YouTube Music mix / radio (generated `RD…` playlist).
#[tauri::command]
async fn play_mix(app: State<'_, App>, id: String) -> Res<()> {
    let tracks = app.rp.query().music_radio(&id).await.map_err(err)?.items;
    if tracks.is_empty() {
        return Err("this mix is empty".into());
    }
    app.core.send(CoreMsg::PlayTracks { tracks, start: 0 });
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
    app.core.send(CoreMsg::Control(action));
}

#[tauri::command]
fn seek(app: State<'_, App>, position: f64) {
    app.core.send(CoreMsg::SeekTo(position));
}

#[tauri::command]
fn set_volume(app: State<'_, App>, volume: f64, persist: bool) {
    app.core.send(CoreMsg::Volume(volume));
    if persist {
        let mut cfg = app.config.lock().unwrap();
        cfg.volume = volume.clamp(0.0, 1.0);
        cfg.save();
    }
}

/// Re-sends the current `state` and `queue` events (UI start-up).
#[tauri::command]
fn sync(app: State<'_, App>) {
    app.core.send(CoreMsg::Sync);
}

// ---- account ---------------------------------------------------------------

#[tauri::command]
async fn auth_status(app: State<'_, App>) -> Res<auth::AuthStatus> {
    Ok(auth::status(&app.rp).await)
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
    app.manage(App { rp, core, config: Mutex::new(config) });
    Ok(())
}

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("tubedesk=info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| media::show_window(app)))
        .invoke_handler(tauri::generate_handler![
            home, search, library, playlist, album, artist,
            play, play_mix, enqueue, queue_jump, queue_remove, queue_move, queue_clear, control, seek, set_volume, sync,
            auth_status, auth_login, auth_import, auth_logout,
        ])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("error while running TubeDesk");
}
