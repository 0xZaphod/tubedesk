//! Native media controls (MPRIS on Linux, SMTC on Windows, Now Playing on
//! macOS) via souvlaki, on their own thread.

use crate::core::{Control, CoreHandle, CoreMsg};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Default)]
pub struct MediaState {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub artwork: Option<String>,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
}

pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Starts the media-controls thread and returns a sender for state updates.
/// The core handle is delivered later (it needs this sender to be created first).
pub fn spawn(app: AppHandle, core: std::sync::mpsc::Receiver<CoreHandle>) -> Option<Sender<MediaState>> {
    let (tx, rx) = channel::<MediaState>();
    std::thread::Builder::new()
        .name("media-controls".into())
        .spawn(move || {
            let Ok(core) = core.recv() else { return };
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
            let attached = controls.attach(move |event| {
                log::debug!("media control event: {event:?}");
                let c = |ctl| core.send(CoreMsg::Control(ctl));
                match event {
                    MediaControlEvent::Play => c(Control::Play),
                    MediaControlEvent::Pause => c(Control::Pause),
                    MediaControlEvent::Toggle => c(Control::Toggle),
                    MediaControlEvent::Next => c(Control::Next),
                    MediaControlEvent::Previous => c(Control::Previous),
                    MediaControlEvent::Stop => c(Control::Stop),
                    MediaControlEvent::Seek(dir) => {
                        core.send(CoreMsg::SeekBy(if dir == SeekDirection::Forward { 10.0 } else { -10.0 }))
                    }
                    MediaControlEvent::SeekBy(dir, d) => {
                        let s = d.as_secs_f64();
                        core.send(CoreMsg::SeekBy(if dir == SeekDirection::Forward { s } else { -s }));
                    }
                    MediaControlEvent::SetPosition(MediaPosition(p)) => core.send(CoreMsg::SeekTo(p.as_secs_f64())),
                    MediaControlEvent::SetVolume(v) => core.send(CoreMsg::Volume(v)),
                    MediaControlEvent::Raise => show_window(&app),
                    MediaControlEvent::Quit => app.exit(0),
                    _ => {}
                }
            });
            if let Err(e) = attached {
                log::warn!("could not attach media controls: {e:?}");
                return;
            }

            type MetaKey = (Option<String>, Option<String>, Option<String>, Option<String>, u64);
            let mut last_meta: Option<MetaKey> = None;
            let mut last_playback: Option<(bool, bool, u64)> = None;
            for state in rx {
                let key = (state.title.clone(), state.artist.clone(), state.album.clone(), state.artwork.clone(), state.duration as u64);
                if last_meta.as_ref() != Some(&key) {
                    let _ = controls.set_metadata(MediaMetadata {
                        title: state.title.as_deref(),
                        artist: state.artist.as_deref(),
                        album: state.album.as_deref(),
                        cover_url: state.artwork.as_deref(),
                        duration: (state.duration > 0.0).then(|| Duration::from_secs_f64(state.duration)),
                    });
                    last_meta = Some(key);
                }
                // Position only needs to be pushed on state changes / roughly every few seconds.
                let pb = (state.title.is_some(), state.playing, state.position as u64 / 5);
                if last_playback == Some(pb) {
                    continue;
                }
                last_playback = Some(pb);
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
