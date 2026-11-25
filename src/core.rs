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
    Clear,
    Control(Control),
    SeekBy(f64),
    SeekTo(f64),
    Volume(f64),
    Player(PlayerEvent),
    Resolved { gen: u64, id: String, result: Result<Stream, String> },
    Prefetched { id: String, stream: Stream },
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
    gen: u64,
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
        gen: 0,
        consecutive_errors: 0,
        radio_requested_for: None,
        streams: HashMap::new(),
    };
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
            CoreMsg::Resolved { gen, id, result } => match result {
                Ok(stream) => {
                    self.cache_stream(id.clone(), stream.clone());
                    if gen == self.gen {
                        self.load(&id, &stream);
                    }
                }
                Err(e) if gen == self.gen => self.failed(e),
                Err(_) => {} // stale: user already skipped
            },
            CoreMsg::Prefetched { id, stream } => self.cache_stream(id, stream),
            CoreMsg::RadioLoaded { for_id, tracks } => {
                let known: std::collections::HashSet<_> = self.queue.iter().map(|t| t.id.clone()).collect();
                let new: Vec<_> = tracks.into_iter().filter(|t| t.id != for_id && !known.contains(&t.id)).collect();
                if !new.is_empty() {
                    log::info!("radio: appended {} tracks", new.len());
                    self.queue.extend(new);
                    self.emit_queue();
                    if let Some(i) = self.index {
                        self.prefetch(i + 1);
                    }
                }
            }
        }
    }

    fn control(&mut self, c: Control) {
        match c {
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
        self.gen += 1;
        self.player.send(PlayerCmd::Stop);
        self.index = None;
        self.now = NowPlaying { volume: self.now.volume, ..Default::default() };
        self.emit_state();
    }

    fn start_at(&mut self, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else { return };
        self.gen += 1;
        let gen = self.gen;
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

        // Play right away if the stream was prefetched, else resolve it off the actor.
        match self.streams.get(&track.id).filter(|s| s.is_fresh()).cloned() {
            Some(stream) => self.load(&track.id, &stream),
            None => {
                let (me, ytdlp, hq, id) = (self.me.clone(), self.cfg.ytdlp.clone(), self.cfg.high_quality, track.id.clone());
                tauri::async_runtime::spawn(async move {
                    let result = resolver::resolve(&ytdlp, &id, hq).await;
                    me.send(CoreMsg::Resolved { gen, id, result });
                });
            }
        }
        self.prefetch(i + 1);

        // Near the end of the queue: extend it with YouTube Music's radio for this track.
        if self.cfg.autoplay_radio && i + 2 >= self.queue.len() && self.radio_requested_for.as_deref() != Some(&track.id) {
            self.radio_requested_for = Some(track.id.clone());
            let (rp, me) = (self.rp.clone(), self.me.clone());
            tauri::async_runtime::spawn(async move {
                match rp.query().music_radio_track(&track.id).await {
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
                if self.index.is_none() {
                    return;
                }
                if playing && position > 0.0 && self.now.loading {
                    log::info!("playback started");
                    self.now.loading = false;
                    self.consecutive_errors = 0;
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

    /// Resolves the track at `i` in the background so skipping to it is instant.
    fn prefetch(&mut self, i: usize) {
        let Some(track) = self.queue.get(i) else { return };
        if self.streams.get(&track.id).is_some_and(|s| s.is_fresh()) {
            return;
        }
        let (me, ytdlp, hq, id) = (self.me.clone(), self.cfg.ytdlp.clone(), self.cfg.high_quality, track.id.clone());
        tauri::async_runtime::spawn(async move {
            match resolver::resolve(&ytdlp, &id, hq).await {
                Ok(stream) => me.send(CoreMsg::Prefetched { id, stream }),
                Err(e) => log::debug!("prefetch of {id} failed: {e}"),
            }
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
    }
}
