//! Playback core: the queue, stream resolving and radio autoplay.
//! Runs as a single async task; everything talks to it through [`CoreHandle`].

use crate::media::MediaState;
use crate::player::{Player, PlayerCmd, PlayerEvent};
use crate::proxy::StreamProxy;
use crate::resolver::{self, Lane, Promotion, Stream};
use rustypipe::client::RustyPipe;
use rustypipe::model::TrackItem;
use std::collections::{HashMap, HashSet};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// What happens at the end of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    /// Start over at the queue's first track after its last.
    All,
    /// Play the current track again.
    One,
}

/// Settings the core applies while running (see `config.rs`).
pub struct CoreSettings {
    pub high_quality: bool,
    pub autoplay_radio: bool,
    pub normalize: bool,
    pub ytdlp: String,
}

pub enum CoreMsg {
    /// Plays `tracks` from `start`. `shuffle` switches the shuffle mode on or
    /// off first (the Play / Shuffle buttons); `None` keeps the current mode.
    /// With shuffle on, track `start` comes first and the rest is shuffled;
    /// started from the Shuffle button (`Some(true)`), a track whose stream
    /// is already prepared may come first instead, to start instantly.
    PlayTracks { tracks: Vec<TrackItem>, start: usize, shuffle: Option<bool> },
    /// The first page of a list (like `PlayTracks`); its remaining pages
    /// follow as `Extend` with the same `token` (see [`list_token`]).
    PlayList { tracks: Vec<TrackItem>, start: usize, shuffle: bool, token: u64 },
    SetShuffle(bool),
    SetRepeat(Repeat),
    Settings(CoreSettings),
    /// `id`'s loudness from YouTube (`None`: not available).
    Loudness { id: String, db: Option<f32> },
    Enqueue(Vec<TrackItem>),
    PlayNext(Vec<TrackItem>),
    JumpTo(usize),
    Remove(usize),
    Move { from: usize, to: usize },
    /// More tracks of the list started by `PlayList` with this `token`
    /// (ignored if anything else has been started or the queue cleared since).
    Extend { token: u64, tracks: Vec<TrackItem> },
    /// Prepare a track the user is likely to play next. `priority`: a likely
    /// pick (a list's first track), never dropped; otherwise a speculative
    /// hint (hover), dropped while several lookups are running.
    Prefetch { id: String, priority: bool },
    Clear,
    Control(Control),
    SeekBy(f64),
    SeekTo(f64),
    Volume(f64),
    Player(PlayerEvent),
    /// `high_quality`: the quality the lookup was started with.
    Resolved { id: String, result: Result<Stream, String>, high_quality: bool },
    /// A quick URL for the start of `id` (see `resolver::quick`).
    QuickResolved { id: String, url: String, size: u64, mime: String },
    /// The proxy found `id`'s stream URL expired (or refused): look it up again.
    Refresh(String),
    RadioLoaded { for_id: String, tracks: Vec<TrackItem> },
    Sync,
    /// The app is quitting: save the session now and acknowledge once it is
    /// written (the writer thread would otherwise be cut off mid-write).
    Shutdown(StdSender<()>),
}

#[derive(Clone)]
pub struct CoreHandle(UnboundedSender<CoreMsg>);

static NEXT_LIST_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A new token tying a list's `Extend` messages to its `PlayList`.
pub fn list_token() -> u64 {
    NEXT_LIST_TOKEN.fetch_add(1, Ordering::Relaxed)
}

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
    /// A seek (or resume) is in progress: show a busy state, keep controls live.
    pub buffering: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f64,
    pub error: Option<String>,
    /// Copied from the core on every emit (`now` is reset per track).
    pub shuffle: bool,
    pub repeat: Repeat,
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
    /// Turn loud tracks down to YouTube's reference loudness.
    pub normalize: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub ytdlp: String,
    /// Where the queue, current track and position are remembered.
    pub session_path: std::path::PathBuf,
}

/// The last session, restored (paused) at start-up.
#[derive(serde::Deserialize)]
struct Session {
    tracks: Vec<TrackItem>,
    index: Option<usize>,
    position: f64,
    /// Length as reported by the player: many tracks (e.g. from the home
    /// feed) carry no duration of their own.
    #[serde(default)]
    duration: f64,
    /// The queue's order before shuffling (see `Core::order`).
    #[serde(default)]
    order: Option<Vec<String>>,
}

/// The list being played, while its later pages may still arrive.
struct ListInfo {
    token: u64,
    shuffled: bool,
    /// Ids of the first page, to tell them from tracks queued by hand.
    ids: HashSet<String>,
}

/// Lookups of one track id in flight.
#[derive(Default)]
struct Lookup {
    /// One of them is a foreground lookup (the track the user waits for).
    foreground: bool,
    /// Only speculative (hover) lookups are running: such a lookup can wait
    /// behind other hints and is dropped when stale, so it must not stand in
    /// for preparing an upcoming track.
    speculative_only: bool,
    running: u32,
    /// Lets the background lookup skip its queue (see `request_now`).
    promotion: Option<Arc<Promotion>>,
}

/// [`Session`] as written (borrowing the queue instead of copying it).
#[derive(Serialize)]
struct SessionRef<'a> {
    tracks: &'a [TrackItem],
    index: Option<usize>,
    position: f64,
    duration: f64,
    order: Option<&'a [String]>,
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
    /// yt-dlp lookups in flight by id: at most one background and one
    /// foreground lookup per id.
    inflight: HashMap<String, Lookup>,
    list: Option<ListInfo>,
    /// Track id the tray tooltip / window title were last set for.
    titled: Option<Option<String>>,
    /// The queue or index changed: save the session once the message is handled.
    dirty: bool,
    /// Latest session file contents, for the writer thread.
    session_tx: StdSender<WriterMsg>,
    /// Restored from the last session but not loaded yet: Play resumes it.
    restored: bool,
    /// The current track is playing from its quick URL and has no full URL
    /// registered yet: if its lookup fails it would stall after the start.
    quick_only: Option<String>,
    /// Seek here once the (resumed) track starts playing.
    pending_seek: Option<f64>,
    /// The last seek target and when it was asked for (see `SeekBy`).
    last_seek: Option<(f64, std::time::Instant)>,
    /// Position bucket of the last session save (saves every few seconds).
    saved_at: Option<u64>,
    consecutive_errors: u32,
    radio_requested_for: Option<String>,
    /// Resolved stream URLs by video id (current + prefetched next track).
    streams: HashMap<String, Stream>,
    shuffle: bool,
    repeat: Repeat,
    /// While shuffled: the track ids in the order they had before, so
    /// turning shuffle off restores it. Tracks added since are kept after
    /// the track they follow (see `unshuffle`).
    order: Option<Vec<String>>,
    /// YouTube's loudness per track id (dB above its reference level).
    loudness: HashMap<String, Option<f32>>,
    loudness_pending: HashSet<String>,
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
    let me3 = me.clone();
    proxy.on_refresh(move |id| me3.send(CoreMsg::Refresh(id.to_string())));
    let session_tx = spawn_session_writer(cfg.session_path.clone());
    let (cfg_shuffle, cfg_repeat) = (cfg.shuffle, cfg.repeat);
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
        list: None,
        session_tx,
        titled: None,
        dirty: false,
        restored: false,
        quick_only: None,
        pending_seek: None,
        last_seek: None,
        saved_at: None,
        consecutive_errors: 0,
        radio_requested_for: None,
        streams: HashMap::new(),
        shuffle: cfg_shuffle,
        repeat: cfg_repeat,
        order: None,
        loudness: HashMap::new(),
        loudness_pending: HashSet::new(),
    };
    let mut core = core;
    core.restore();
    tauri::async_runtime::spawn(core.run(rx));
    Ok(me)
}

enum WriterMsg {
    Save(Vec<u8>),
    /// Acknowledged once everything sent before it is on disk.
    Flush(StdSender<()>),
}

/// Writes the session file on its own thread (it can be large: the whole
/// queue), skipping versions that were superseded before they got written.
fn spawn_session_writer(path: std::path::PathBuf) -> StdSender<WriterMsg> {
    let (tx, rx) = std::sync::mpsc::channel::<WriterMsg>();
    let _ = std::thread::Builder::new().name("session-writer".into()).spawn(move || {
        while let Ok(first) = rx.recv() {
            let (mut latest, mut acks) = (None, Vec::new());
            for msg in std::iter::once(first).chain(rx.try_iter()) {
                match msg {
                    WriterMsg::Save(bytes) => latest = Some(bytes),
                    WriterMsg::Flush(ack) => acks.push(ack),
                }
            }
            if let Some(bytes) = latest {
                if let Err(e) = crate::config::write_atomic(&path, &bytes) {
                    log::debug!("could not save session: {e}");
                }
            }
            for ack in acks {
                let _ = ack.send(());
            }
        }
    });
    tx
}

impl Core {
    async fn run(mut self, mut rx: UnboundedReceiver<CoreMsg>) {
        while let Some(msg) = rx.recv().await {
            self.handle(msg);
            if std::mem::take(&mut self.dirty) {
                self.save_session();
            }
        }
    }

    fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::PlayTracks { tracks, .. } | CoreMsg::PlayList { tracks, .. } if tracks.is_empty() => {
                log::debug!("nothing to play; keeping the queue");
            }
            CoreMsg::PlayTracks { tracks, start, shuffle } => self.play_tracks(tracks, start, shuffle),
            CoreMsg::PlayList { tracks, start, shuffle, token } => {
                // Kept in both modes: shuffle may be switched on before the
                // later pages arrive, and only these may be shuffled with them.
                let ids = tracks.iter().map(|t| t.id.clone()).collect();
                self.play_tracks(tracks, start, Some(shuffle));
                self.list = Some(ListInfo { token, shuffled: self.shuffle, ids });
            }
            CoreMsg::SetShuffle(on) => self.set_shuffle(on),
            CoreMsg::SetRepeat(mode) => {
                self.repeat = mode;
                if let Some(i) = self.index {
                    self.prefetch_ahead(i); // the track after the last may now be the first
                }
                self.emit_state();
            }
            CoreMsg::Settings(s) => {
                let quality_changed = s.high_quality != self.cfg.high_quality;
                let normalize_on = s.normalize && !self.cfg.normalize;
                self.cfg.high_quality = s.high_quality;
                self.cfg.autoplay_radio = s.autoplay_radio;
                self.cfg.normalize = s.normalize;
                self.cfg.ytdlp = s.ytdlp;
                if quality_changed {
                    // Prepared streams are in the old quality; keep only the
                    // one playing (its quick start must match its full URL).
                    let current = self.now.track.as_ref().map(|t| t.id.clone());
                    self.streams.retain(|id, _| Some(id) == current.as_ref());
                    if let Some(i) = self.index {
                        self.prefetch_ahead(i);
                    }
                }
                if normalize_on {
                    // Nothing was measured while it was off.
                    if let Some(i) = self.index {
                        let ids: Vec<String> =
                            std::iter::once(i).chain(self.ahead(i)).filter_map(|k| self.queue.get(k)).map(|t| t.id.clone()).collect();
                        for id in ids {
                            self.want_loudness(&id);
                        }
                    }
                }
                self.apply_gain();
            }
            CoreMsg::Loudness { id, db } => {
                self.loudness_pending.remove(&id);
                if self.loudness.len() > 1000 {
                    self.loudness.clear();
                }
                log::debug!("{id}: loudness {db:?} dB");
                self.loudness.insert(id.clone(), db);
                if self.now.track.as_ref().is_some_and(|t| t.id == id) {
                    self.apply_gain();
                }
            }
            CoreMsg::Enqueue(tracks) => {
                let was_empty = self.queue.is_empty();
                if let Some(order) = &mut self.order {
                    order.extend(tracks.iter().map(|t| t.id.clone()));
                }
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
                            // The next track takes its place, paused if it was.
                            let play = self.now.playing;
                            self.start_at_from(cur, None, play);
                        } else {
                            self.stop();
                        }
                    }
                    _ => {}
                }
                self.emit_queue();
                if let Some(i) = self.index {
                    self.prefetch_ahead(i); // the next two may have changed
                }
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
            CoreMsg::Extend { token, tracks } => {
                // A list's rest arrives once, and only counts for that list.
                if let Some(list) = self.list.take_if(|l| l.token == token) {
                    log::info!("adding {} more tracks of the list", tracks.len());
                    if let Some(order) = &mut self.order {
                        order.extend(tracks.iter().map(|t| t.id.clone()));
                    }
                    if list.shuffled {
                        self.mix_in(&list.ids, tracks);
                    } else {
                        self.queue.extend(tracks);
                    }
                    self.emit_queue();
                    if let Some(i) = self.index {
                        self.prefetch_ahead(i);
                    }
                }
            }
            CoreMsg::Prefetch { id, priority } => {
                if !self.streams.get(&id).is_some_and(|s| s.is_fresh()) {
                    if priority {
                        self.request(&id);
                    } else {
                        self.request_background(&id);
                    }
                }
            }
            CoreMsg::Clear => {
                self.queue.clear();
                self.order = self.shuffle.then(Vec::new);
                self.list = None;
                self.stop();
                self.emit_queue();
            }
            CoreMsg::Control(c) => self.control(c),
            CoreMsg::SeekBy(d) => {
                // Presses in quick succession add up: until the player reports
                // the new position, count from the last target.
                let from = match self.last_seek {
                    Some((target, at)) if at.elapsed() < std::time::Duration::from_millis(1500) => target,
                    _ => self.now.position,
                };
                self.seek_to(from + d);
            }
            CoreMsg::SeekTo(t) => self.seek_to(t),
            CoreMsg::Refresh(id) => {
                log::info!("{id}: stream URL expired or refused; looking it up again");
                self.streams.remove(&id);
                self.request_now(&id);
            }
            CoreMsg::Volume(v) => {
                let v = v.clamp(0.0, 1.0);
                self.now.volume = v;
                self.player.send(PlayerCmd::Volume(v));
                self.emit_state();
            }
            CoreMsg::Player(ev) => self.player_event(ev),
            CoreMsg::Shutdown(ack) => {
                self.save_session();
                let _ = self.session_tx.send(WriterMsg::Flush(ack));
            }
            CoreMsg::Sync => {
                self.emit_state();
                self.emit_queue();
            }
            CoreMsg::QuickResolved { id, url, size, mime } => {
                if self.awaiting.as_deref() == Some(id.as_str()) {
                    log::debug!("{id}: starting from the quick URL");
                    let uri = self.proxy.register_quick(&id, url, size, mime);
                    self.load_uri(uri);
                    self.quick_only = Some(id);
                }
            }
            CoreMsg::Resolved { id, result, high_quality } => {
                let others_running = match self.inflight.get_mut(&id) {
                    Some(l) if l.running > 1 => {
                        l.running -= 1;
                        true
                    }
                    _ => {
                        self.inflight.remove(&id);
                        false
                    }
                };
                let wanted = self.awaiting.as_deref() == Some(id.as_str());
                // Started before the quality setting changed: its stream is
                // the other format, so look the track up again if needed.
                if result.is_ok() && high_quality != self.cfg.high_quality {
                    log::debug!("{id}: lookup was for the old quality; dropped");
                    if !others_running && (wanted || self.quick_only.as_deref() == Some(id.as_str())) {
                        self.request_now(&id);
                    }
                    return;
                }
                // Fill in a missing length for the shown track (e.g. a restored
                // session or a home-feed track without duration).
                if let Ok(stream) = &result {
                    let shown = self.now.track.as_ref().is_some_and(|t| t.id == id);
                    if shown && self.now.duration <= 0.0 {
                        if let Some(d) = stream.duration.filter(|d| *d > 0.0) {
                            log::debug!("{id}: length {d:.0}s from yt-dlp");
                            self.now.duration = d;
                            self.emit_state();
                        }
                    }
                }
                match result {
                    // The second of two lookups of the same track: keep the
                    // first URL, it may be in use already (and a new one is
                    // refused for a few seconds).
                    Ok(_) if !wanted && self.streams.get(&id).is_some_and(|s| s.is_fresh()) => {
                        log::debug!("{id}: already resolved; keeping the first URL");
                        if self.quick_only.as_deref() == Some(id.as_str()) {
                            let s = self.streams[&id].clone();
                            self.proxy.register(&id, &s);
                            self.quick_only = None;
                        }
                    }
                    Ok(stream) => {
                        if self.quick_only.as_deref() == Some(id.as_str()) {
                            self.quick_only = None; // its full URL is registered below
                        }
                        self.cache_stream(id.clone(), stream.clone());
                        if wanted {
                            self.load(&id, &stream);
                        } else {
                            // Upcoming track: register it; pre-fetch its start
                            // only if it is next in the queue (not for hover hints).
                            self.proxy.register(&id, &stream);
                            if self.should_warm(&id) {
                                self.warm(&id);
                            }
                        }
                    }
                    Err(e) if others_running => log::debug!("lookup of {id} failed ({e}); another one is still running"),
                    Err(e) if wanted => {
                        self.awaiting = None;
                        self.failed(e);
                    }
                    // Playing from the quick URL, which only covers the start:
                    // without a full URL the song would stall, so skip it now.
                    Err(e) if self.playing_quick_only(&id) => {
                        log::warn!("{id}: no full stream URL; stopping after the quick start");
                        self.player.send(PlayerCmd::Stop);
                        self.proxy.forget(&id);
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
                    if let Some(order) = &mut self.order {
                        order.extend(new.iter().map(|t| t.id.clone()));
                    }
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
        log::debug!("control {c:?} (playing={}, loading={})", self.now.playing, self.now.loading);
        // The track isn't loaded yet (its URL is still being looked up): the
        // player is idle, so remember the choice and apply it on load.
        if self.awaiting.is_some() && matches!(c, Control::Play | Control::Pause | Control::Toggle) {
            self.now.playing = match c {
                Control::Play => true,
                Control::Pause => false,
                _ => !self.now.playing,
            };
            self.emit_state();
            return;
        }
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

    fn seek_to(&mut self, t: f64) {
        // Never past the end: that would end (or, resumed, skip) the track.
        let end = if self.now.duration > 0.0 { (self.now.duration - 1.0).max(0.0) } else { f64::MAX };
        let t = t.clamp(0.0, end);
        self.last_seek = Some((t, std::time::Instant::now()));
        // Nothing loaded yet (restored, or the URL is still being looked up):
        // the idle player would ignore the seek, so start there instead.
        if self.restored || self.awaiting.is_some() {
            self.now.position = t;
            if self.awaiting.is_some() {
                self.pending_seek = Some(t);
            } else {
                self.dirty = true; // resume from here, also after a restart
            }
            self.emit_state();
            return;
        }
        self.player.send(PlayerCmd::Seek(t));
    }

    /// Shuffles a shuffled list's later pages in with its not yet played
    /// first-page tracks, so a long list isn't played first page first.
    /// Tracks queued by hand, the current track and the two prepared after
    /// it stay where they are.
    fn mix_in(&mut self, ids: &HashSet<String>, rest: Vec<TrackItem>) {
        let keep = self.index.map_or(0, |i| i + 3).min(self.queue.len());
        let slots: Vec<usize> = (keep..self.queue.len()).filter(|&k| ids.contains(&self.queue[k].id)).collect();
        let mut pool: Vec<TrackItem> = slots.iter().map(|&k| self.queue[k].clone()).collect();
        pool.extend(rest);
        crate::shuffle(&mut pool);
        let mut pool = pool.into_iter();
        for &k in &slots {
            if let Some(t) = pool.next() {
                self.queue[k] = t;
            }
        }
        self.queue.extend(pool);
    }

    fn advance(&mut self) {
        match self.index.and_then(|i| self.next_index(i)) {
            Some(k) => self.start_at(k),
            None => self.stop(),
        }
    }

    /// The track after `i`: with repeat on, the first follows the last.
    fn next_index(&self, i: usize) -> Option<usize> {
        if i + 1 < self.queue.len() {
            Some(i + 1)
        } else if self.repeat != Repeat::Off && !self.queue.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    /// Up to two tracks that play after `i` (other than `i` itself).
    fn ahead(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut k = i;
        while out.len() < 2 {
            match self.next_index(k) {
                Some(n) if n != i && !out.contains(&n) => {
                    out.push(n);
                    k = n;
                }
                _ => break,
            }
        }
        out
    }

    fn play_tracks(&mut self, mut tracks: Vec<TrackItem>, start: usize, shuffle: Option<bool>) {
        if let Some(on) = shuffle {
            self.shuffle = on;
        }
        let mut start = start.min(tracks.len() - 1);
        if self.shuffle {
            self.order = Some(tracks.iter().map(|t| t.id.clone()).collect());
            // The Shuffle button: `start` is a random pick made in advance
            // (and prepared, so it usually starts instantly). Only the track
            // playing now is avoided. Preferring *any* prepared track instead
            // kept picking the few tracks the last shuffles had prepared.
            if shuffle == Some(true) {
                let ids: Vec<&str> = tracks.iter().map(|t| t.id.as_str()).collect();
                let playing = self.now.track.as_ref().map(|t| t.id.as_str());
                let k = shuffle_start(&ids, start, playing);
                if k != start {
                    log::debug!("shuffle: starting with {} instead of {}", ids[k], ids[start]);
                }
                start = k;
            }
            let first = tracks.remove(start);
            crate::shuffle(&mut tracks);
            tracks.insert(0, first);
            start = 0;
        } else {
            self.order = None;
        }
        self.queue = tracks;
        self.radio_requested_for = None;
        self.list = None;
        self.start_at(start);
        self.emit_queue();
    }

    /// Shuffle on: the current track moves to the top, everything else is
    /// shuffled after it. Off: the order from before comes back.
    fn set_shuffle(&mut self, on: bool) {
        if on != self.shuffle {
            self.shuffle = on;
            if on {
                self.order = Some(self.queue.iter().map(|t| t.id.clone()).collect());
                let first = self.index.map(|i| self.queue.remove(i));
                crate::shuffle(&mut self.queue);
                if let Some(t) = first {
                    self.queue.insert(0, t);
                    self.index = Some(0);
                }
                if let Some(list) = &mut self.list {
                    list.shuffled = true; // its later pages get mixed in
                }
            } else {
                if let Some(order) = self.order.take() {
                    self.unshuffle(&order);
                }
                if let Some(list) = &mut self.list {
                    list.shuffled = false;
                }
            }
            self.now.index = self.index;
            self.emit_queue();
            if let Some(i) = self.index {
                self.prefetch_ahead(i);
            }
        }
        self.emit_state();
    }

    /// Restores the order the queue had before shuffling. Tracks that weren't
    /// in it (added with "Play next") stay right after the track they follow.
    fn unshuffle(&mut self, order: &[String]) {
        let ids: Vec<&str> = self.queue.iter().map(|t| t.id.as_str()).collect();
        let perm = unshuffled(&ids, order);
        let mut old: Vec<Option<TrackItem>> = std::mem::take(&mut self.queue).into_iter().map(Some).collect();
        self.queue = perm.iter().filter_map(|&k| old[k].take()).collect();
        self.index = self.index.and_then(|c| perm.iter().position(|&k| k == c));
    }

    fn stop(&mut self) {
        self.quick_only = None;
        self.awaiting = None;
        self.player.send(PlayerCmd::Stop);
        self.index = None;
        self.proxy.set_upcoming(Vec::new());
        self.now = NowPlaying { volume: self.now.volume, ..Default::default() };
        self.emit_state();
        self.emit_index();
    }

    fn start_at(&mut self, i: usize) {
        self.start_at_from(i, None, true);
    }

    /// Starts track `i`, optionally from position `from` (resume): the player
    /// then seeks before it starts playing, so the beginning is never heard.
    /// `play` false loads it paused.
    fn start_at_from(&mut self, i: usize, from: Option<f64>, play: bool) {
        self.restored = false;
        self.quick_only = None;
        self.pending_seek = from;
        self.last_seek = None;
        let Some(track) = self.queue.get(i).cloned() else { return };
        // Repeated play requests for a track that is still starting up (e.g. a
        // double-click) must not restart it.
        if self.index == Some(i) && self.now.loading && self.now.track.as_ref().map(|t| &t.id) == Some(&track.id) {
            return;
        }
        log::debug!("starting track {i}: {}", track.id);
        self.awaiting = None;
        self.index = Some(i);
        self.player.send(PlayerCmd::Stop);
        self.now = NowPlaying {
            duration: if from.is_some() && self.now.duration > 0.0 { self.now.duration } else { track.duration.unwrap_or(0) as f64 },
            track: Some(track.clone()),
            index: Some(i),
            loading: true,
            playing: play,
            // Resuming: show the saved position right away, not 0:00.
            position: from.unwrap_or(0.0),
            volume: self.now.volume,
            ..Default::default()
        };
        self.emit_state();
        self.emit_index();

        // Play right away if the stream was prefetched, else wait for (possibly
        // already running) resolve.
        match self.streams.get(&track.id).filter(|s| s.is_fresh()).cloned() {
            Some(stream) => {
                self.want_loudness(&track.id);
                self.load(&track.id, &stream);
            }
            None => {
                self.awaiting = Some(track.id.clone());
                self.request_now(&track.id);
                // Start from a quick URL meanwhile; yt-dlp's full URL takes
                // over. The same answer carries the track's loudness.
                let loudness = !self.loudness.contains_key(&track.id) && self.loudness_pending.insert(track.id.clone());
                let (rp, me, id, hq) = (self.rp.clone(), self.me.clone(), track.id.clone(), self.cfg.high_quality);
                tauri::async_runtime::spawn(async move {
                    match resolver::quick(&rp, &id, hq).await {
                        Ok(q) => {
                            if loudness {
                                me.send(CoreMsg::Loudness { id: id.clone(), db: q.loudness_db });
                            }
                            me.send(CoreMsg::QuickResolved { id, url: q.url, size: q.size, mime: q.mime });
                        }
                        Err(e) => {
                            log::debug!("{id}: no quick URL ({e})");
                            if loudness {
                                me.send(CoreMsg::Loudness { id, db: None });
                            }
                        }
                    }
                });
            }
        }
        self.prefetch_ahead(i);

        // Near the end of the queue: extend it with YouTube Music's radio for this track.
        if self.cfg.autoplay_radio && self.repeat == Repeat::Off && i + 2 >= self.queue.len() && self.radio_requested_for.as_deref() != Some(&track.id) {
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
                log::debug!("end of track (repeat {:?})", self.repeat);
                self.consecutive_errors = 0;
                match self.index {
                    Some(i) if self.repeat == Repeat::One => self.start_at(i),
                    _ => self.advance(),
                }
            }
            PlayerEvent::Error(e) => self.failed(e),
            PlayerEvent::Tick { position, duration, playing, buffering } => {
                // While showing a restored track, or while the current track
                // isn't loaded yet, the idle player's ticks (position 0) would
                // make the seek bar jump to the start.
                if self.index.is_none() || self.restored || self.awaiting.is_some() {
                    return;
                }
                self.now.buffering = buffering;
                if !playing && !buffering && self.now.loading {
                    self.now.loading = false; // loaded but paused: nothing to wait for
                }
                if playing && !buffering && position > 0.0 && self.now.loading {
                    log::info!("playback started");
                    self.now.loading = false;
                    self.consecutive_errors = 0;
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
        self.load_uri(uri);
    }

    fn load_uri(&mut self, uri: String) {
        self.awaiting = None;
        self.apply_gain();
        // Paused (while loading, or loaded paused): load via LoadAt, which
        // produces no sound before the Play, not even the first moment.
        self.player.send(match (self.pending_seek.take(), self.now.playing) {
            (Some(pos), _) => PlayerCmd::LoadAt(uri, pos),
            (None, false) => PlayerCmd::LoadAt(uri, 0.0),
            (None, true) => PlayerCmd::Load(uri),
        });
        if !self.now.playing {
            self.player.send(PlayerCmd::Pause);
        }
    }

    /// The current track's volume correction: loud tracks are turned down to
    /// YouTube's reference level, quiet ones are left alone (as YouTube does;
    /// raising them could clip).
    fn track_gain(&self) -> f64 {
        let db = self.now.track.as_ref().and_then(|t| self.loudness.get(&t.id).copied().flatten());
        match db {
            Some(db) if self.cfg.normalize && db > 0.0 => 10f64.powf(-f64::from(db) / 20.0),
            _ => 1.0,
        }
    }

    fn apply_gain(&self) {
        self.player.send(PlayerCmd::TrackGain(self.track_gain()));
    }

    /// Fetches `id`'s loudness (one quick API request), unless known.
    fn want_loudness(&mut self, id: &str) {
        if !self.cfg.normalize || self.loudness.contains_key(id) || !self.loudness_pending.insert(id.to_string()) {
            return;
        }
        let (rp, me, id) = (self.rp.clone(), self.me.clone(), id.to_string());
        tauri::async_runtime::spawn(async move {
            let db = match rp.query().unauthenticated().player(&id).await {
                Ok(p) => p.audio_streams.iter().find_map(|s| s.loudness_db),
                Err(e) => {
                    log::debug!("{id}: no loudness ({e})");
                    None
                }
            };
            me.send(CoreMsg::Loudness { id, db });
        });
    }

    fn cache_stream(&mut self, id: String, stream: Stream) {
        const MAX: usize = 40;
        self.streams.retain(|_, s| s.is_fresh());
        // Evict the oldest, but never the current track or the two after it
        // (hover prefetches would otherwise push them out).
        let keep: Vec<String> = self
            .index
            .map(|i| std::iter::once(i).chain(self.ahead(i)).filter_map(|k| self.queue.get(k)).map(|t| t.id.clone()).collect())
            .unwrap_or_default();
        while self.streams.len() >= MAX {
            let oldest = self
                .streams
                .iter()
                .filter(|(k, _)| !keep.contains(k))
                .max_by_key(|(_, s)| s.age())
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.streams.remove(&k),
                None => break,
            };
        }
        self.streams.insert(id, stream);
    }

    /// Prepares the two tracks after `i`, so playing on or skipping once is instant.
    /// Both missing ones are looked up in one yt-dlp run.
    fn prefetch_ahead(&mut self, i: usize) {
        self.proxy.set_upcoming(self.upcoming());
        let mut missing = Vec::new();
        for k in self.ahead(i) {
            let Some(id) = self.queue.get(k).map(|t| t.id.clone()) else { continue };
            self.want_loudness(&id);
            match self.streams.get(&id).filter(|s| s.is_fresh()) {
                // Already prepared: re-register so the proxy keeps its warmed
                // start (its eviction is oldest-registered-first), and warm it
                // if it was only resolved as a hover hint.
                Some(s) => {
                    self.proxy.register(&id, s);
                    self.warm(&id);
                }
                // A hover hint for it doesn't count: it may wait behind other
                // hints and be dropped, so prepare it properly anyway.
                None if !self.inflight.get(&id).is_some_and(|l| !l.speculative_only) && !missing.contains(&id) => {
                    missing.push(id)
                }
                None => {}
            }
        }
        if !missing.is_empty() {
            self.spawn_resolve(missing, Lane::Ahead);
        }
    }

    /// A speculative lookup (hover hint): dropped when several lookups are
    /// already running, and on its own lane so it never delays the queue's
    /// next tracks.
    fn request_background(&mut self, id: &str) {
        if self.inflight.len() < 4 && !self.inflight.contains_key(id) {
            self.spawn_resolve(vec![id.to_string()], Lane::Speculative);
        }
    }

    /// Starts a background lookup for `id` (an upcoming or likely track)
    /// unless one is already running.
    fn request(&mut self, id: &str) {
        if self.inflight.get(id).is_some_and(|l| !l.speculative_only) {
            return;
        }
        self.spawn_resolve(vec![id.to_string()], Lane::Ahead);
    }

    /// Starts a lookup for the track the user is waiting for, bypassing the
    /// background queue. A background lookup of the same id already in
    /// flight is promoted (it skips its queue) instead of starting a second
    /// yt-dlp run; the quick URL covers the start meanwhile.
    fn request_now(&mut self, id: &str) {
        match self.inflight.get_mut(id) {
            Some(l) if l.foreground => {}
            Some(l) => {
                log::debug!("{id}: already being looked up; promoting that lookup");
                l.foreground = true;
                if let Some(p) = &l.promotion {
                    p.promote();
                }
            }
            None => self.spawn_resolve(vec![id.to_string()], Lane::Now),
        }
    }

    /// Looks up `ids` in one yt-dlp run; each sends its own `Resolved`.
    fn spawn_resolve(&mut self, ids: Vec<String>, lane: Lane) {
        let promotion = Arc::new(Promotion::default());
        for id in &ids {
            let fresh = !self.inflight.contains_key(id);
            let lookup = self.inflight.entry(id.clone()).or_default();
            lookup.speculative_only = lane == Lane::Speculative && (fresh || lookup.speculative_only);
            lookup.running += 1;
            lookup.foreground |= lane == Lane::Now;
            if lane != Lane::Now {
                lookup.promotion = Some(promotion.clone());
            }
        }
        let (me, ytdlp, hq) = (self.me.clone(), self.cfg.ytdlp.clone(), self.cfg.high_quality);
        tauri::async_runtime::spawn(async move {
            // Results arrive one by one as yt-dlp finishes each video, so the
            // first of a batch isn't held back by the second.
            resolver::resolve(&ytdlp, &ids, hq, lane, promotion, |id, result| {
                me.send(CoreMsg::Resolved { id, result, high_quality: hq })
            })
            .await;
        });
    }

    /// Whether `id`'s start is worth downloading ahead: one of the next two
    /// tracks, or the restored track that Play will resume.
    fn should_warm(&self, id: &str) -> bool {
        self.upcoming().iter().any(|u| u == id)
    }

    fn upcoming(&self) -> Vec<String> {
        let Some(i) = self.index else { return Vec::new() };
        let current = self.restored.then_some(i);
        current.into_iter().chain(self.ahead(i)).filter_map(|k| self.queue.get(k)).map(|t| t.id.clone()).collect()
    }

    fn warm(&self, id: &str) {
        let (proxy, id) = (self.proxy.clone(), id.to_string());
        tauri::async_runtime::spawn(async move { proxy.warm(&id).await });
    }

    /// The current track is playing from its quick URL and has no full URL.
    /// Recorded explicitly: inferring it from "no fresh cached URL" also hit
    /// tracks playing fine from a full URL that merely got old (long pause).
    fn playing_quick_only(&self, id: &str) -> bool {
        self.quick_only.as_deref() == Some(id) && self.now.track.as_ref().is_some_and(|t| t.id == id)
    }

    fn emit_state(&mut self) {
        self.now.shuffle = self.shuffle;
        self.now.repeat = self.repeat;
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
        // Only on track changes: this runs on every position tick.
        let id = self.now.track.as_ref().map(|t| t.id.clone());
        if self.titled.as_ref() != Some(&id) {
            crate::update_tray_title(&self.app, self.now.track.as_ref());
            self.titled = Some(id);
        }
    }

    /// The whole queue (after its contents changed).
    fn emit_queue(&mut self) {
        let _ = self.app.emit("queue", QueueView { tracks: &self.queue, index: self.index });
        self.dirty = true;
    }

    /// Only the current index (switching tracks), so the UI and the session
    /// file don't handle the whole queue on every track change.
    fn emit_index(&mut self) {
        let _ = self.app.emit("queue_index", serde_json::json!({ "index": self.index }));
        self.dirty = true;
    }

    fn save_session(&self) {
        let s = SessionRef {
            tracks: &self.queue,
            index: self.index,
            position: self.now.position,
            duration: self.now.duration,
            order: self.order.as_deref(),
        };
        match serde_json::to_vec(&s) {
            Ok(b) => {
                let _ = self.session_tx.send(WriterMsg::Save(b));
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
        if self.shuffle {
            self.order = Some(s.order.unwrap_or_else(|| self.queue.iter().map(|t| t.id.clone()).collect()));
        }
        let duration = if s.duration > 0.0 { s.duration } else { track.duration.unwrap_or(0) as f64 };
        self.now = NowPlaying {
            duration,
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
        self.want_loudness(&id);
        self.prefetch_ahead(i);
    }

    /// Starts the restored track from where it was left.
    fn resume(&mut self) {
        self.restored = false;
        let (Some(i), pos) = (self.index, self.now.position) else { return };
        self.start_at_from(i, (pos > 1.0).then_some(pos), true);
    }
}

/// Where a shuffled list starts: the `suggested` (randomly pre-picked) track,
/// unless it is the one `playing` now; then another random track, so
/// shuffling the same list again doesn't restart the current song.
fn shuffle_start(ids: &[&str], suggested: usize, playing: Option<&str>) -> usize {
    if ids.len() < 2 || Some(ids[suggested]) != playing {
        return suggested;
    }
    let mut others: Vec<usize> = (0..ids.len()).filter(|&k| Some(ids[k]) != playing).collect();
    crate::shuffle(&mut others);
    others.first().copied().unwrap_or(suggested)
}

/// The queue positions of `ids` in their pre-shuffle `order`: entry `n` of
/// the result is the current position of the track that goes `n`th. Tracks
/// not in `order` stay right after the track they currently follow; repeated
/// ids take their ranks in turn.
fn unshuffled(ids: &[&str], order: &[String]) -> Vec<usize> {
    let mut ranks: HashMap<&str, std::collections::VecDeque<usize>> = HashMap::new();
    for (k, id) in order.iter().enumerate() {
        ranks.entry(id.as_str()).or_default().push_back(k);
    }
    // Ranked tracks sort by (rank + 1, 0); an unranked one by its
    // predecessor's key with the second part counting up.
    let mut anchor = (0, 0);
    let mut keys = Vec::with_capacity(ids.len());
    for (k, id) in ids.iter().enumerate() {
        anchor = match ranks.get_mut(id).and_then(|r| r.pop_front()) {
            Some(rank) => (rank + 1, 0),
            None => (anchor.0, anchor.1 + 1),
        };
        keys.push((anchor, k));
    }
    keys.sort_by_key(|&(key, _)| key);
    keys.into_iter().map(|(_, k)| k).collect()
}

#[cfg(test)]
mod tests {
    use super::{shuffle_start, unshuffled};

    #[test]
    fn shuffle_starts_with_the_random_pick() {
        let ids = ["a", "b", "c", "d"];
        assert_eq!(shuffle_start(&ids, 2, None), 2);
        assert_eq!(shuffle_start(&ids, 2, Some("a")), 2);
    }

    #[test]
    fn shuffle_avoids_the_playing_track() {
        let ids = ["a", "b", "c"];
        for _ in 0..20 {
            assert_ne!(shuffle_start(&ids, 0, Some("a")), 0);
        }
        // A one-track list may restart.
        assert_eq!(shuffle_start(&["a"], 0, Some("a")), 0);
    }

    fn order(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn apply<'a>(ids: &[&'a str], perm: &[usize]) -> Vec<&'a str> {
        perm.iter().map(|&k| ids[k]).collect()
    }

    #[test]
    fn restores_the_original_order() {
        let ids = ["c", "a", "d", "b"];
        assert_eq!(apply(&ids, &unshuffled(&ids, &order(&["a", "b", "c", "d"]))), ["a", "b", "c", "d"]);
    }

    #[test]
    fn added_tracks_stay_after_the_track_they_follow() {
        // "x" was added with Play next while "c" played.
        let ids = ["c", "x", "a", "d", "b"];
        assert_eq!(apply(&ids, &unshuffled(&ids, &order(&["a", "b", "c", "d"]))), ["a", "b", "c", "x", "d"]);
        // At the very top, they stay first.
        let ids = ["x", "b", "a"];
        assert_eq!(apply(&ids, &unshuffled(&ids, &order(&["a", "b"]))), ["x", "a", "b"]);
    }

    #[test]
    fn removed_and_repeated_tracks() {
        // "b" was removed since; "a" is in the list twice.
        let ids = ["a", "c", "a"];
        assert_eq!(unshuffled(&ids, &order(&["a", "b", "a", "c"])), [0, 2, 1]);
    }
}
