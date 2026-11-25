//! Playback core: the queue, stream resolving and radio autoplay.
//! Runs as a single async task; everything talks to it through [`CoreHandle`].

use crate::media::MediaState;
use crate::player::{Player, PlayerCmd, PlayerEvent};
use crate::proxy::StreamProxy;
use crate::resolver::{self, Stream};
use rustypipe::client::RustyPipe;
use rustypipe::model::TrackItem;
use std::collections::HashMap;
use serde::Serialize;
use std::sync::mpsc::Sender as StdSender;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
}

pub enum CoreMsg {
    PlayTracks { tracks: Vec<TrackItem>, start: usize },
    Enqueue(Vec<TrackItem>),
    PlayNext(Vec<TrackItem>),
    JumpTo(usize),
    Remove(usize),
    Move { from: usize, to: usize },
    /// More tracks of the list that was started with `anchor` as first track
    /// (ignored if something else has been started since).
    Extend { anchor: String, tracks: Vec<TrackItem> },
    /// Prepare a track the user is likely to play next (e.g. hovered).
    Prefetch(String),
    Clear,
    Control(Control),
    SeekBy(f64),
    SeekTo(f64),
    Volume(f64),
    Player(PlayerEvent),
    Resolved { id: String, result: Result<Stream, String> },
    RadioLoaded { for_id: String, tracks: Vec<TrackItem> },
    Sync,
}

#[derive(Clone)]
pub struct CoreHandle(UnboundedSender<CoreMsg>);

impl CoreHandle {
    pub fn send(&self, msg: CoreMsg) {
        let _ = self.0.send(msg);
    }
}

/// Sent to the UI as the `state` event.
#[derive(Debug, Clone, Serialize, Default)]
pub struct NowPlaying {
    pub track: Option<TrackItem>,
    pub index: Option<usize>,
    pub playing: bool,
    pub loading: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub error: Option<String>,
}

/// Sent to the UI as the `queue` event.
#[derive(Debug, Clone, Serialize)]
pub struct QueueView<'a> {
    pub tracks: &'a [TrackItem],
    pub index: Option<usize>,
}

pub struct CoreConfig {
    pub volume: f64,
    pub autoplay_radio: bool,
    pub high_quality: bool,
    pub ytdlp: String,
    /// Where the queue, current track and position are remembered.
    pub session_path: std::path::PathBuf,
}

/// The last session, restored (paused) at start-up.
#[derive(serde::Serialize, serde::Deserialize)]
struct Session {
    tracks: Vec<TrackItem>,
    index: Option<usize>,
    position: f64,
}

struct Core {
    app: AppHandle,
    rp: RustyPipe,
    proxy: StreamProxy,
    player: Player,
    media: Option<StdSender<MediaState>>,
    me: CoreHandle,
    cfg: CoreConfig,
    queue: Vec<TrackItem>,
    index: Option<usize>,
    now: NowPlaying,
    /// Track id the current track is waiting on to be resolved (not loaded yet).
    awaiting: Option<String>,
    /// Ids with a yt-dlp lookup in flight, so each id is resolved at most once at a time.
    inflight: std::collections::HashSet<String>,
    /// Restored from the last session but not loaded yet: Play resumes it.
    restored: bool,
    /// Seek here once the (resumed) track starts playing.
    pending_seek: Option<f64>,
    /// Position bucket of the last session save (saves every few seconds).
    saved_at: Option<u64>,
    consecutive_errors: u32,
    radio_requested_for: Option<String>,
    /// Resolved stream URLs by video id (current + prefetched next track).
    streams: HashMap<String, Stream>,
}

pub fn spawn(
    app: AppHandle,
    rp: RustyPipe,
    proxy: StreamProxy,
    media: Option<StdSender<MediaState>>,
    cfg: CoreConfig,
) -> Result<CoreHandle, String> {
    let (tx, rx) = unbounded_channel();
    let me = CoreHandle(tx);
    let me2 = me.clone();
    let player = Player::spawn(cfg.volume, move |ev| me2.send(CoreMsg::Player(ev)))?;
    let core = Core {
        app,
        rp,
        proxy,
        player,
        media,
        me: me.clone(),
        now: NowPlaying { volume: cfg.volume, ..Default::default() },
        cfg,
        queue: Vec::new(),
        index: None,
        awaiting: None,
        inflight: Default::default(),
        restored: false,
        pending_seek: None,
        saved_at: None,
        consecutive_errors: 0,
        radio_requested_for: None,
        streams: HashMap::new(),
    };
    let mut core = core;
    core.restore();
    tauri::async_runtime::spawn(core.run(rx));
    Ok(me)
}

impl Core {
    async fn run(mut self, mut rx: UnboundedReceiver<CoreMsg>) {
        while let Some(msg) = rx.recv().await {
            self.handle(msg);
        }
    }

    fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::PlayTracks { tracks, start } => {
                self.queue = tracks;
                self.radio_requested_for = None;
                self.start_at(start.min(self.queue.len().saturating_sub(1)));
                self.emit_queue();
            }
            CoreMsg::Enqueue(tracks) => {
                let was_empty = self.queue.is_empty();
                self.queue.extend(tracks);
                if was_empty {
                    self.start_at(0);
                }
                self.emit_queue();
            }
            CoreMsg::PlayNext(tracks) => {
                let at = self.index.map_or(0, |i| i + 1).min(self.queue.len());
                let was_empty = self.queue.is_empty();
                self.queue.splice(at..at, tracks);
                if was_empty {
                    self.start_at(0);
                }
                self.emit_queue();
            }
            CoreMsg::JumpTo(i) if i < self.queue.len() => self.start_at(i),
            CoreMsg::JumpTo(_) => {}
            CoreMsg::Remove(i) if i < self.queue.len() => {
                self.queue.remove(i);
                match self.index {
                    Some(cur) if i < cur => self.index = Some(cur - 1),
                    Some(cur) if i == cur => {
                        if cur < self.queue.len() {
                            self.start_at(cur);
                        } else {
                            self.stop();
                        }
                    }
                    _ => {}
                }
                self.emit_queue();
            }
            CoreMsg::Remove(_) => {}
            CoreMsg::Move { from, to } if from < self.queue.len() && to < self.queue.len() && from != to => {
                let t = self.queue.remove(from);
                self.queue.insert(to, t);
                // Keep pointing at the same (playing) track.
                if let Some(cur) = self.index {
                    self.index = Some(if cur == from {
                        to
                    } else if from < cur && to >= cur {
                        cur - 1
                    } else if from > cur && to <= cur {
                        cur + 1
                    } else {
                        cur
                    });
                    self.now.index = self.index;
                }
                self.emit_queue();
                if let Some(i) = self.index {
                    self.prefetch_ahead(i);
                }
            }
            CoreMsg::Move { .. } => {}
            CoreMsg::Extend { anchor, tracks } => {
                if self.queue.first().is_some_and(|t| t.id == anchor) {
                    log::info!("appended {} more tracks of the list", tracks.len());
                    self.queue.extend(tracks);
                    self.emit_queue();
                    if let Some(i) = self.index {
                        self.prefetch_ahead(i);
                    }
                }
            }
            CoreMsg::Prefetch(id) => {
                if !self.streams.get(&id).is_some_and(|s| s.is_fresh()) {
                    self.request_background(&id);
                }
            }
            CoreMsg::Clear => {
                self.queue.clear();
                self.stop();
                self.emit_queue();
            }
            CoreMsg::Control(c) => self.control(c),
            CoreMsg::SeekBy(d) => self.player.send(PlayerCmd::Seek(self.now.position + d)),
            CoreMsg::SeekTo(t) => self.player.send(PlayerCmd::Seek(t)),
            CoreMsg::Volume(v) => {
                let v = v.clamp(0.0, 1.0);
                self.now.volume = v;
                self.player.send(PlayerCmd::Volume(v));
                self.emit_state();
            }
            CoreMsg::Player(ev) => self.player_event(ev),
            CoreMsg::Sync => {
                self.emit_state();
                self.emit_queue();
            }
            CoreMsg::Resolved { id, result } => {
                self.inflight.remove(&id);
                let wanted = self.awaiting.as_deref() == Some(id.as_str());
                match result {
                    Ok(stream) => {
                        self.cache_stream(id.clone(), stream.clone());
                        if wanted {
                            self.load(&id, &stream);
                        } else {
                            // Upcoming track: register it and pre-fetch its start.
                            self.proxy.register(&id, &stream);
                            let proxy = self.proxy.clone();
                            tauri::async_runtime::spawn(async move { proxy.warm(&id).await });
                        }
                    }
                    Err(e) if wanted => {
                        self.awaiting = None;
                        self.failed(e);
                    }
                    Err(e) => log::debug!("background resolve of {id} failed: {e}"),
                }
            }
            CoreMsg::RadioLoaded { for_id, tracks } => {
                let known: std::collections::HashSet<_> = self.queue.iter().map(|t| t.id.clone()).collect();
                let new: Vec<_> = tracks.into_iter().filter(|t| t.id != for_id && !known.contains(&t.id)).collect();
                if !new.is_empty() {
                    log::info!("radio: appended {} tracks", new.len());
                    self.queue.extend(new);
                    self.emit_queue();
                    if let Some(i) = self.index {
                        self.prefetch_ahead(i);
                    }
                }
            }
        }
    }

    fn control(&mut self, c: Control) {
        match c {
            Control::Play | Control::Toggle if self.restored => self.resume(),
            Control::Play if self.index.is_none() && !self.queue.is_empty() => self.start_at(0),
            Control::Play => self.player.send(PlayerCmd::Play),
            Control::Pause => self.player.send(PlayerCmd::Pause),
            Control::Toggle if self.index.is_none() && !self.queue.is_empty() => self.start_at(0),
            Control::Toggle => self.player.send(PlayerCmd::Toggle),
            Control::Stop => self.player.send(PlayerCmd::Stop),
            Control::Next => self.advance(),
            Control::Previous => {
                if self.now.position > 3.0 || self.index == Some(0) {
                    self.player.send(PlayerCmd::Seek(0.0));
                } else if let Some(i) = self.index {
                    self.start_at(i - 1);
                }
            }
        }
    }

    fn advance(&mut self) {
        match self.index {
            Some(i) if i + 1 < self.queue.len() => self.start_at(i + 1),
            _ => self.stop(),
        }
    }

    fn stop(&mut self) {
        self.awaiting = None;
        self.player.send(PlayerCmd::Stop);
        self.index = None;
        self.now = NowPlaying { volume: self.now.volume, ..Default::default() };
        self.emit_state();
    }

    fn start_at(&mut self, i: usize) {
        self.restored = false;
        self.pending_seek = None;
        let Some(track) = self.queue.get(i).cloned() else { return };
        // Repeated play requests for a track that is still starting up (e.g. a
        // double-click) must not restart it.
        if self.index == Some(i) && self.now.loading && self.now.track.as_ref().map(|t| &t.id) == Some(&track.id) {
            return;
        }
        self.awaiting = None;
        self.index = Some(i);
        self.player.send(PlayerCmd::Stop);
        self.now = NowPlaying {
            duration: track.duration.unwrap_or(0) as f64,
            track: Some(track.clone()),
            index: Some(i),
            loading: true,
            volume: self.now.volume,
            ..Default::default()
        };
        self.emit_state();
        self.emit_queue();

        // Play right away if the stream was prefetched, else wait for (possibly
        // already running) resolve.
        match self.streams.get(&track.id).filter(|s| s.is_fresh()).cloned() {
            Some(stream) => self.load(&track.id, &stream),
            None => {
                self.awaiting = Some(track.id.clone());
                self.request(&track.id);
            }
        }
        self.prefetch_ahead(i);

        // Near the end of the queue: extend it with YouTube Music's radio for this track.
        if self.cfg.autoplay_radio && i + 2 >= self.queue.len() && self.radio_requested_for.as_deref() != Some(&track.id) {
            self.radio_requested_for = Some(track.id.clone());
            let (rp, me) = (self.rp.clone(), self.me.clone());
            tauri::async_runtime::spawn(async move {
                match crate::signed_query(&rp).music_radio_track(&track.id).await {
                    Ok(p) => me.send(CoreMsg::RadioLoaded { for_id: track.id, tracks: p.items }),
                    Err(e) => log::warn!("radio for {} failed: {e}", track.id),
                }
            });
        }
    }

    fn failed(&mut self, e: String) {
        log::warn!("track failed: {e}");
        self.consecutive_errors += 1;
        self.now.loading = false;
        self.now.playing = false;
        self.now.error = Some(e);
        self.emit_state();
        // Skip unplayable tracks, but don't spin through a whole queue if extraction is broken.
        if self.consecutive_errors < 3 {
            self.advance();
        }
    }

    fn player_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Eos => {
                self.consecutive_errors = 0;
                self.advance();
            }
            PlayerEvent::Error(e) => self.failed(e),
            PlayerEvent::Tick { position, duration, playing } => {
                // While showing a restored (not yet loaded) track, the idle
                // player's ticks would reset the remembered position.
                if self.index.is_none() || self.restored {
                    return;
                }
                if playing && position > 0.0 && self.now.loading {
                    log::info!("playback started");
                    self.now.loading = false;
                    self.consecutive_errors = 0;
                    if let Some(p) = self.pending_seek.take() {
                        self.player.send(PlayerCmd::Seek(p));
                    }
                }
                // Remember the position every 5 s of playback and on pause.
                let bucket = if playing { position as u64 / 5 } else { u64::MAX };
                if self.saved_at != Some(bucket) && !self.now.loading {
                    self.saved_at = Some(bucket);
                    self.now.position = position;
                    self.save_session();
                }
                self.now.position = position;
                if duration > 0.0 {
                    self.now.duration = duration;
                }
                self.now.playing = playing;
                self.emit_state();
            }
        }
    }

    fn load(&mut self, id: &str, stream: &Stream) {
        self.awaiting = None;
        let uri = self.proxy.register(id, stream);
        self.player.send(PlayerCmd::Load(uri));
    }

    fn cache_stream(&mut self, id: String, stream: Stream) {
        self.streams.retain(|_, s| s.is_fresh());
        if self.streams.len() > 16 {
            self.streams.clear();
        }
        self.streams.insert(id, stream);
    }

    /// Prepares the two tracks after `i`, so playing on or skipping once is instant.
    fn prefetch_ahead(&mut self, i: usize) {
        self.prefetch(i + 1);
        self.prefetch(i + 2);
    }

    /// Resolves the track at `i` in the background so skipping to it is instant.
    fn prefetch(&mut self, i: usize) {
        let Some(id) = self.queue.get(i).map(|t| t.id.clone()) else { return };
        if !self.streams.get(&id).is_some_and(|s| s.is_fresh()) {
            self.request(&id);
        }
    }

    /// Like `request`, but dropped when several lookups are already running,
    /// so speculative prefetches (hover) never pile up yt-dlp processes.
    fn request_background(&mut self, id: &str) {
        if self.inflight.len() < 4 {
            self.request(id);
        }
    }

    /// Starts a yt-dlp lookup for `id` unless one is already running.
    fn request(&mut self, id: &str) {
        if !self.inflight.insert(id.to_string()) {
            return;
        }
        let (me, ytdlp, hq, id) = (self.me.clone(), self.cfg.ytdlp.clone(), self.cfg.high_quality, id.to_string());
        tauri::async_runtime::spawn(async move {
            let result = resolver::resolve(&ytdlp, &id, hq).await;
            me.send(CoreMsg::Resolved { id, result });
        });
    }

    fn emit_state(&self) {
        let _ = self.app.emit("state", &self.now);
        if let Some(tx) = &self.media {
            let t = self.now.track.as_ref();
            let _ = tx.send(MediaState {
                title: t.map(|t| t.name.clone()),
                artist: t.map(|t| t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")),
                album: t.and_then(|t| t.album.as_ref().map(|a| a.name.clone())),
                artwork: t.and_then(|t| crate::best_thumb(&t.cover)),
                playing: self.now.playing,
                position: self.now.position,
                duration: self.now.duration,
            });
        }
        crate::update_tray_title(&self.app, self.now.track.as_ref());
    }

    fn emit_queue(&self) {
        let _ = self.app.emit("queue", QueueView { tracks: &self.queue, index: self.index });
        self.save_session();
    }

    fn save_session(&self) {
        let s = Session { tracks: self.queue.clone(), index: self.index, position: self.now.position };
        match serde_json::to_vec(&s) {
            Ok(b) => {
                if let Err(e) = std::fs::write(&self.cfg.session_path, b) {
                    log::debug!("could not save session: {e}");
                }
            }
            Err(e) => log::debug!("could not serialize session: {e}"),
        }
    }

    /// Shows the last session's track (paused) without loading any audio yet.
    fn restore(&mut self) {
        let Some(s) = std::fs::read(&self.cfg.session_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Session>(&b).ok())
        else {
            return;
        };
        let Some(i) = s.index.filter(|&i| i < s.tracks.len()) else { return };
        let track = s.tracks[i].clone();
        log::info!("restored last session: {} ({:.0}s)", track.name, s.position);
        self.queue = s.tracks;
        self.index = Some(i);
        self.restored = true;
        self.now = NowPlaying {
            duration: track.duration.unwrap_or(0) as f64,
            track: Some(track),
            index: Some(i),
            position: s.position,
            volume: self.now.volume,
            ..Default::default()
        };
        // The UI isn't listening yet; it asks via Sync once loaded.
        // Prepare the restored track and the next ones, so Play is instant.
        let id = self.queue[i].id.clone();
        self.request(&id);
        self.prefetch_ahead(i);
    }

    /// Starts the restored track from where it was left.
    fn resume(&mut self) {
        self.restored = false;
        let (Some(i), pos) = (self.index, self.now.position) else { return };
        self.start_at(i);
        if pos > 1.0 {
            self.pending_seek = Some(pos);
        }
    }
}
