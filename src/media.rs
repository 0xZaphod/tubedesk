//! Native media controls (MPRIS on Linux, SMTC on Windows, Now Playing on
//! macOS) via souvlaki. Runs on its own thread; events are forwarded to the
//! page through `window.__tubedesk.control()`.

use serde::{Deserialize, Serialize};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub artwork: Option<String>,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub ad: bool,
}

pub fn control_js(cmd: &str, arg: Option<f64>) -> String {
    let arg = arg.map_or("undefined".to_string(), |a| a.to_string());
    format!("window.__tubedesk && window.__tubedesk.control({cmd:?}, {arg});")
}

pub fn send_control(app: &AppHandle, cmd: &str, arg: Option<f64>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(control_js(cmd, arg));
    }
}

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Starts the media-controls thread and returns a sender for state updates.
pub fn spawn(app: AppHandle) -> Option<Sender<PlayerState>> {
    let (tx, rx) = channel::<PlayerState>();
    std::thread::Builder::new()
        .name("media-controls".into())
        .spawn(move || {
            let config = PlatformConfig {
                dbus_name: "tubedesk",
                display_name: "TubeDesk",
                hwnd: None, // TODO(windows): pass the main window HWND for SMTC
            };
            let mut controls = match MediaControls::new(config) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("media controls unavailable: {e:?}");
                    return;
                }
            };
            let handle = app.clone();
            let attached = controls.attach(move |event| {
                let a = &handle;
                match event {
                    MediaControlEvent::Play => send_control(a, "play", None),
                    MediaControlEvent::Pause => send_control(a, "pause", None),
                    MediaControlEvent::Toggle => send_control(a, "toggle", None),
                    MediaControlEvent::Next => send_control(a, "next", None),
                    MediaControlEvent::Previous => send_control(a, "previous", None),
                    MediaControlEvent::Stop => send_control(a, "stop", None),
                    MediaControlEvent::Seek(dir) => {
                        let s = if dir == SeekDirection::Forward { 10.0 } else { -10.0 };
                        send_control(a, "seek", Some(s));
                    }
                    MediaControlEvent::SeekBy(dir, d) => {
                        let s = d.as_secs_f64();
                        send_control(a, "seek", Some(if dir == SeekDirection::Forward { s } else { -s }));
                    }
                    MediaControlEvent::SetPosition(MediaPosition(p)) => {
                        send_control(a, "position", Some(p.as_secs_f64()))
                    }
                    MediaControlEvent::Raise => show_window(a),
                    MediaControlEvent::Quit => a.exit(0),
                    _ => {}
                }
            });
            if let Err(e) = attached {
                log::warn!("could not attach media controls: {e:?}");
                return;
            }

            let mut last_meta: Option<(Option<String>, Option<String>, Option<String>, Option<String>, u64)> = None;
            for state in rx {
                let meta_key = (
                    state.title.clone(),
                    state.artist.clone(),
                    state.album.clone(),
                    state.artwork.clone(),
                    state.duration as u64,
                );
                if last_meta.as_ref() != Some(&meta_key) {
                    let _ = controls.set_metadata(MediaMetadata {
                        title: state.title.as_deref(),
                        artist: state.artist.as_deref(),
                        album: state.album.as_deref(),
                        cover_url: state.artwork.as_deref(),
                        duration: (state.duration > 0.0).then(|| Duration::from_secs_f64(state.duration)),
                    });
                    last_meta = Some(meta_key);
                }
                let progress = Some(MediaPosition(Duration::from_secs_f64(state.position.max(0.0))));
                let playback = if state.title.is_none() {
                    MediaPlayback::Stopped
                } else if state.playing {
                    MediaPlayback::Playing { progress }
                } else {
                    MediaPlayback::Paused { progress }
                };
                let _ = controls.set_playback(playback);
            }
        })
        .ok()?;
    Some(tx)
}
