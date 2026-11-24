mod adblock;
mod config;
mod filters;
mod media;
#[cfg(target_os = "linux")]
mod netfilter_linux;

use config::{Config, START_URL};
use media::PlayerState;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

const BRIDGE_JS: &str = include_str!("../js/bridge.js");
const PROCEDURAL_JS: &str = include_str!("../js/procedural.js");
const TRAY_ID: &str = "main-tray";

struct AppState {
    media_tx: Mutex<Option<Sender<PlayerState>>>,
    last: Mutex<PlayerState>,
}

/// Called by js/bridge.js whenever playback state changes.
#[tauri::command]
fn player_state(app: AppHandle, state: tauri::State<'_, AppState>, player: PlayerState) {
    log::debug!("player_state: {player:?}");
    if player.ad {
        // Never surface ads as "now playing".
        return;
    }
    {
        let mut last = state.last.lock().unwrap();
        if last.title != player.title || last.artist != player.artist {
            let tip = match (&player.title, &player.artist) {
                (Some(t), Some(a)) => format!("{t} — {a}"),
                (Some(t), None) => t.clone(),
                _ => "TubeDesk".into(),
            };
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                let _ = tray.set_tooltip(Some(&tip));
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_title(&tip);
            }
        }
        *last = player.clone();
    }
    if let Some(tx) = state.media_tx.lock().unwrap().as_ref() {
        let _ = tx.send(player);
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "Play / Pause", true, None::<&str>)?;
    let next = MenuItem::with_id(app, "next", "Next", true, None::<&str>)?;
    let prev = MenuItem::with_id(app, "previous", "Previous", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show / Hide window", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update-filters", "Update filter lists && restart", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(app, &[&toggle, &next, &prev, &sep()?, &show, &update, &sep()?, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().expect("bundled icon"))
        .tooltip("TubeDesk")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "toggle" | "next" | "previous" => media::send_control(app, event.id.as_ref(), None),
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    if w.is_visible().unwrap_or(false) {
                        let _ = w.hide();
                    } else {
                        media::show_window(app);
                    }
                }
            }
            "update-filters" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    if let Ok(dir) = app.path().app_cache_dir() {
                        filters::refresh(&dir.join("filters"), Duration::ZERO);
                    }
                    app.restart();
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let path = app.path();
    let config_path = path.app_config_dir()?.join("config.json");
    let filter_dir = path.app_cache_dir()?.join("filters");
    #[allow(unused_variables)]
    let content_filter_dir = path.app_data_dir()?.join("content-filters");
    let config = Config::load_or_init(&config_path);
    log::info!("config: {}", config_path.display());

    // ---- ad blocking ----------------------------------------------------
    let mut init_script = String::new();
    let mut lists = Vec::new();
    if config.adblock {
        let max_age = Duration::from_secs(config.filter_max_age_hours * 3600);
        if filters::none_cached(&filter_dir) {
            log::info!("first run: downloading filter lists…");
            filters::refresh(&filter_dir, max_age);
        } else {
            // Refresh in the background; new lists apply on next launch.
            let dir = filter_dir.clone();
            std::thread::spawn(move || filters::refresh(&dir, max_age));
        }
        lists = filters::load_cached(&filter_dir);
        let blocking = adblock::build(&lists, PROCEDURAL_JS);
        init_script.push_str(&blocking.init_script);
    }
    init_script.push_str(&BRIDGE_JS.replace("__TD_AUDIO_ONLY__", if config.audio_only { "true" } else { "false" }));

    // ---- window -----------------------------------------------------------
    let start: tauri::Url = START_URL.parse()?;
    let use_netfilter = cfg!(target_os = "linux") && config.adblock && config.network_filtering && !lists.is_empty();
    // With network filtering, start blank and navigate once the filter is attached,
    // so the first page load is already filtered.
    let initial = if use_netfilter { "about:blank".parse()? } else { start.clone() };

    let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(initial))
        .title("TubeDesk")
        .inner_size(1200.0, 800.0)
        .min_inner_size(420.0, 400.0)
        .initialization_script(&init_script);
    if let Some(ua) = &config.user_agent {
        builder = builder.user_agent(ua);
    }
    let window = builder.build()?;

    #[cfg(target_os = "linux")]
    if use_netfilter {
        let fingerprint = filters::fingerprint(&lists);
        let start = start.to_string();
        window.with_webview(move |pw| {
            let wv = pw.inner();
            let wv2 = wv.clone();
            netfilter_linux::install(
                &wv,
                &content_filter_dir,
                &fingerprint,
                move || adblock::content_rules(&lists),
                move || webkit2gtk::WebViewExt::load_uri(&wv2, &start),
            );
        })?;
    }
    // TODO(macos): WKContentRuleListStore accepts the same JSON as WebKitGTK.
    // TODO(windows): block via WebView2's WebResourceRequested + adblock::Engine.

    if config.close_to_tray {
        let w = window.clone();
        window.on_window_event(move |e| {
            if let WindowEvent::CloseRequested { api, .. } = e {
                api.prevent_close();
                let _ = w.hide();
            }
        });
    }

    // ---- integrations -----------------------------------------------------
    build_tray(&handle)?;
    let media_tx = if config.media_controls { media::spawn(handle.clone()) } else { None };
    app.manage(AppState {
        media_tx: Mutex::new(media_tx),
        last: Mutex::new(PlayerState::default()),
    });
    Ok(())
}

pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("tubedesk=info")).init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| media::show_window(app)))
        .invoke_handler(tauri::generate_handler![player_state])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("error while running TubeDesk");
}
