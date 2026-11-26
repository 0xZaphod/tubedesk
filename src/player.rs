//! Audio playback in Rust, the same on every platform: the stream comes from
//! the local proxy over HTTP (with byte ranges, for seeking), symphonia reads
//! the WebM container, libopus decodes, cpal plays the result.
//!
//! Threads: the *player* thread takes commands and reports ticks; each loaded
//! track has a *decoder* thread filling that track's sample buffer; the
//! audio output (cpal's callback, or a timer for the silent test output)
//! drains the current track's buffer. A track that is replaced is cancelled
//! and simply never read again, so a decoder stuck on the network can't
//! delay the next track.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use symphonia::core::codecs::CODEC_TYPE_OPUS;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::units::Time;

/// Loudest the app ever plays: 50% of full scale (−6 dB).
const MAX_GAIN: f64 = 0.5;

/// Opus always decodes at 48 kHz; we decode to stereo.
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Decoded audio kept ahead of the output (in frames): 2 s.
const BUFFER_FRAMES: usize = 2 * RATE as usize;
/// Decoding starts this far before a seek target: Opus needs some audio
/// before the target to sound right at it.
const SEEK_PREROLL: f64 = 0.08;

/// Maps the 0–1 volume slider to an amplitude gain. A cubic curve matches how
/// loudness is perceived (like PulseAudio/PipeWire sliders), so the low end of
/// the slider is actually quiet: 20% → −48 dB, 50% → −24 dB, 100% → −6 dB.
fn gain(slider: f64) -> f64 {
    MAX_GAIN * slider.clamp(0.0, 1.0).powi(3)
}

pub enum PlayerCmd {
    Load(String),
    /// Load and start at a position, without the start being heard.
    LoadAt(String, f64),
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(f64),
    /// The volume slider, 0–1.
    Volume(f64),
    /// Per-track correction factor (volume normalization), 1.0 = none.
    TrackGain(f64),
    /// Internal: something happened (track ended), look now, not at the next tick.
    Wake,
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Eos,
    Error(String),
    /// `playing` is the intent (what Play/Pause last asked for); `buffering`
    /// is true while a track starts or a seek is carried out.
    Tick { position: f64, duration: f64, playing: bool, buffering: bool },
}

#[derive(Clone)]
pub struct Player {
    /// Shared so the output can hold a `Weak` to it: when every Player is
    /// dropped the channel disconnects and the player thread ends.
    tx: Arc<Sender<PlayerCmd>>,
}

impl Player {
    pub fn send(&self, cmd: PlayerCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn spawn(volume: f64, on_event: impl Fn(PlayerEvent) + Send + 'static) -> Result<Self, String> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let tx = Arc::new(tx);
        std::thread::Builder::new()
            .name("player".into())
            .spawn({
                let wake = Arc::downgrade(&tx);
                move || run(rx, wake, volume, on_event, ready_tx)
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { tx })
    }
}

// ---- shared state ------------------------------------------------------------

/// One loaded track: written by its decoder thread, read by the output.
struct Track {
    /// Interleaved stereo samples at 48 kHz.
    buf: Mutex<VecDeque<f32>>,
    /// Signalled when the output took samples (room for the decoder) or the
    /// track was cancelled / asked to seek.
    changed: Condvar,
    /// A seek the decoder hasn't carried out yet. Locked before `buf`.
    seek: Mutex<Option<f64>>,
    /// Position (s) of the first sample in `buf` after the last (re)start.
    base: Mutex<f64>,
    /// Frames played since `base`.
    played: AtomicU64,
    /// Samples since the last (re)start or seek are available.
    ready: AtomicBool,
    /// The decoder reached the end of the stream.
    finished: AtomicBool,
    /// Finished and everything played.
    drained: AtomicBool,
    cancelled: AtomicBool,
    error: Mutex<Option<String>>,
    /// Length in seconds (0 until known).
    duration: Mutex<f64>,
}

impl Track {
    fn new(start: f64) -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(BUFFER_FRAMES * CHANNELS)),
            changed: Condvar::new(),
            seek: Mutex::new(None),
            base: Mutex::new(start),
            played: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            error: Mutex::new(None),
            duration: Mutex::new(0.0),
        }
    }

    fn position(&self) -> f64 {
        *self.base.lock().unwrap() + self.played.load(Ordering::Relaxed) as f64 / f64::from(RATE)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }

    fn request_seek(&self, to: f64) {
        let mut seek = self.seek.lock().unwrap();
        *seek = Some(to);
        // Silent from now on: what is buffered belongs to the old position.
        self.ready.store(false, Ordering::SeqCst);
        // Not at the end any more (also stops a second end-of-stream report
        // before the decoder gets to the seek).
        self.finished.store(false, Ordering::SeqCst);
        self.drained.store(false, Ordering::SeqCst);
        self.changed.notify_all();
    }

    fn fail(&self, e: String) {
        *self.error.lock().unwrap() = Some(e);
    }
}

/// What the audio output plays from.
struct Mixer {
    current: Mutex<Option<Arc<Track>>>,
    playing: AtomicBool,
    /// Output gain (f32 bits).
    gain: AtomicU32,
    wake: Weak<Sender<PlayerCmd>>,
}

impl Mixer {
    fn set_gain(&self, g: f64) {
        self.gain.store((g as f32).to_bits(), Ordering::Relaxed);
    }

    /// Fills `out` (interleaved, `channels` per frame, at `rate`) from the
    /// current track. Silence while paused, loading or seeking.
    fn render(&self, out: &mut [f32], channels: usize, resampler: &mut Resampler) {
        out.fill(0.0);
        let track = self.current.lock().unwrap().clone();
        let Some(track) = track else { return };
        if !self.playing.load(Ordering::Relaxed) || !track.ready.load(Ordering::SeqCst) {
            return;
        }
        let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
        let consumed = {
            let mut buf = track.buf.lock().unwrap();
            resampler.fill(&mut buf, out, channels, gain)
        };
        if consumed > 0 {
            track.played.fetch_add(consumed as u64, Ordering::Relaxed);
            track.changed.notify_all();
        }
        let empty = track.buf.lock().unwrap().is_empty();
        if empty && track.finished.load(Ordering::SeqCst) && !track.drained.swap(true, Ordering::SeqCst) {
            if let Some(tx) = self.wake.upgrade() {
                let _ = tx.send(PlayerCmd::Wake);
            }
        }
    }
}

/// Converts the 48 kHz stereo buffer to the output's rate and channel count.
/// Linear interpolation: only used when the device can't run at 48 kHz.
struct Resampler {
    step: f64,
    pos: f64,
    prev: [f32; 2],
    next: [f32; 2],
}

impl Resampler {
    fn new(out_rate: u32) -> Self {
        Self { step: f64::from(RATE) / f64::from(out_rate), pos: 1.0, prev: [0.0; 2], next: [0.0; 2] }
    }

    /// Returns the number of source frames taken from `buf`.
    fn fill(&mut self, buf: &mut VecDeque<f32>, out: &mut [f32], channels: usize, gain: f32) -> usize {
        let mut consumed = 0;
        for frame in out.chunks_mut(channels) {
            let (l, r) = if self.step == 1.0 {
                match (buf.pop_front(), buf.pop_front()) {
                    (Some(l), Some(r)) => {
                        consumed += 1;
                        (l, r)
                    }
                    _ => break, // ran dry: the rest stays silent
                }
            } else {
                while self.pos >= 1.0 {
                    match (buf.pop_front(), buf.pop_front()) {
                        (Some(l), Some(r)) => {
                            self.prev = self.next;
                            self.next = [l, r];
                            self.pos -= 1.0;
                            consumed += 1;
                        }
                        _ => return consumed,
                    }
                }
                let t = self.pos as f32;
                self.pos += self.step;
                (self.prev[0] + (self.next[0] - self.prev[0]) * t, self.prev[1] + (self.next[1] - self.prev[1]) * t)
            };
            let (l, r) = (l * gain, r * gain);
            match channels {
                1 => frame[0] = (l + r) * 0.5,
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                }
            }
        }
        consumed
    }
}

// ---- output ------------------------------------------------------------------

/// Keeps the audio output alive (it stops when dropped).
enum Output {
    #[allow(dead_code)] // only held: the stream plays while it exists
    Device(cpal::Stream),
    /// `TUBEDESK_AUDIO_SINK=null` (tests): consumes audio in real time, silently.
    Null(Arc<AtomicBool>),
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Null(stop) = self {
            stop.store(true, Ordering::SeqCst);
        }
    }
}

fn null_output_wanted() -> bool {
    // "fakesink…" is what the GStreamer player understood; still accepted.
    std::env::var("TUBEDESK_AUDIO_SINK").is_ok_and(|v| v == "null" || v.starts_with("fakesink"))
}

fn open_output(mixer: &Arc<Mixer>, failed: &Arc<AtomicBool>) -> Result<Output, String> {
    if null_output_wanted() {
        let stop = Arc::new(AtomicBool::new(false));
        let (mixer, stop2) = (mixer.clone(), stop.clone());
        std::thread::Builder::new()
            .name("null-output".into())
            .spawn(move || {
                let mut resampler = Resampler::new(RATE);
                let mut out = vec![0.0; (RATE as usize / 100) * CHANNELS];
                let start = Instant::now();
                let mut periods = 0u32;
                while !stop2.load(Ordering::SeqCst) {
                    mixer.render(&mut out, CHANNELS, &mut resampler);
                    periods += 1;
                    let due = start + Duration::from_millis(10) * periods;
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            })
            .map_err(|e| e.to_string())?;
        return Ok(Output::Null(stop));
    }

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no audio output device")?;
    let default = device.default_output_config().map_err(|e| format!("audio output: {e}"))?;
    // Prefer 48 kHz (no resampling), keeping the device's default channel
    // count (devices also list mono configs) and sample format.
    let config = device
        .supported_output_configs()
        .ok()
        .and_then(|mut cs| {
            cs.find(|c| {
                c.sample_format() == default.sample_format()
                    && c.channels() == default.channels()
                    && c.min_sample_rate().0 <= RATE
                    && c.max_sample_rate().0 >= RATE
            })
        })
        .map(|c| c.with_sample_rate(cpal::SampleRate(RATE)))
        .unwrap_or(default);
    let format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    let channels = usize::from(config.channels);
    log::debug!("audio output: {} Hz, {channels} channels, {format:?}", config.sample_rate.0);

    let on_error = {
        let failed = failed.clone();
        move |e: cpal::StreamError| {
            log::warn!("audio output error: {e}");
            failed.store(true, Ordering::SeqCst);
        }
    };
    let rate = config.sample_rate.0;
    macro_rules! stream {
        ($t:ty) => {{
            let (mixer, mut resampler, mut scratch) = (mixer.clone(), Resampler::new(rate), Vec::<f32>::new());
            device.build_output_stream(
                &config,
                move |data: &mut [$t], _: &cpal::OutputCallbackInfo| {
                    scratch.resize(data.len(), 0.0);
                    mixer.render(&mut scratch, channels, &mut resampler);
                    for (d, s) in data.iter_mut().zip(&scratch) {
                        *d = <$t as cpal::Sample>::from_sample(*s);
                    }
                },
                on_error,
                None,
            )
        }};
    }
    let stream = match format {
        cpal::SampleFormat::F32 => stream!(f32),
        cpal::SampleFormat::I16 => stream!(i16),
        cpal::SampleFormat::U16 => stream!(u16),
        cpal::SampleFormat::I32 => stream!(i32),
        other => return Err(format!("unsupported audio output format {other:?}")),
    }
    .map_err(|e| format!("audio output: {e}"))?;
    stream.play().map_err(|e| format!("audio output: {e}"))?;
    Ok(Output::Device(stream))
}

// ---- player thread -------------------------------------------------------------

fn run(
    rx: Receiver<PlayerCmd>,
    wake: Weak<Sender<PlayerCmd>>,
    volume: f64,
    on_event: impl Fn(PlayerEvent),
    ready: Sender<Result<(), String>>,
) {
    let mixer = Arc::new(Mixer {
        current: Mutex::new(None),
        playing: AtomicBool::new(false),
        gain: AtomicU32::new(0),
        wake,
    });
    let mut levels = Levels { slider: volume, track: 1.0 };
    mixer.set_gain(levels.value());
    let output_failed = Arc::new(AtomicBool::new(false));
    // No device yet (e.g. started at login before the sound server): keep
    // retrying below instead of failing the app.
    let mut retry_output_at: Option<Instant> = None;
    let mut _output = match open_output(&mixer, &output_failed) {
        Ok(o) => Some(o),
        Err(e) => {
            log::warn!("{e}; retrying");
            output_failed.store(true, Ordering::SeqCst);
            retry_output_at = Some(Instant::now() + Duration::from_secs(2));
            None
        }
    };
    let _ = ready.send(Ok(()));

    let mut track: Option<Arc<Track>> = None;
    let mut want_playing = false;
    // Reported once per track; then no ticks until the next load (the core
    // moves on, and a stale tick would describe the old track as the new one).
    let mut ended = false;
    // While a seek is carried out, report its target (not 0 or the old spot).
    let mut seek_target: Option<f64> = None;
    let mut last_tick = (f64::NAN, f64::NAN, false, false);

    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(cmd) => {
                for cmd in std::iter::once(cmd).chain(rx.try_iter()) {
                    match cmd {
                        PlayerCmd::Load(uri) => {
                            load(&mixer, &mut track, uri, 0.0);
                            (ended, seek_target, want_playing) = (false, None, true);
                        }
                        PlayerCmd::LoadAt(uri, pos) => {
                            load(&mixer, &mut track, uri, pos);
                            (ended, seek_target, want_playing) = (false, Some(pos), true);
                        }
                        PlayerCmd::Play => want_playing = true,
                        PlayerCmd::Pause => want_playing = false,
                        PlayerCmd::Toggle => want_playing = !want_playing,
                        PlayerCmd::Stop => {
                            stop(&mixer, &mut track);
                            (want_playing, seek_target) = (false, None);
                        }
                        PlayerCmd::Seek(secs) => {
                            if let Some(t) = &track {
                                let secs = secs.max(0.0);
                                t.request_seek(secs);
                                seek_target = Some(secs);
                                // Seeking back from the end: the track goes on.
                                ended = false;
                            }
                        }
                        PlayerCmd::Volume(v) => {
                            levels.slider = v;
                            mixer.set_gain(levels.value());
                        }
                        PlayerCmd::TrackGain(g) => {
                            if (g - levels.track).abs() > 1e-6 {
                                log::debug!("track gain {g:.3} ({:+.1} dB)", 20.0 * g.log10());
                                levels.track = g;
                                mixer.set_gain(levels.value());
                            }
                        }
                        PlayerCmd::Wake => {}
                    }
                }
                mixer.playing.store(want_playing, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // The output device went away (unplugged, sound server restart): reopen it.
        if output_failed.load(Ordering::SeqCst) && retry_output_at.is_none_or(|t| Instant::now() >= t) {
            _output = None;
            output_failed.store(false, Ordering::SeqCst);
            match open_output(&mixer, &output_failed) {
                Ok(o) => {
                    if retry_output_at.is_some() {
                        log::info!("audio output available again");
                    }
                    _output = Some(o);
                    retry_output_at = None;
                }
                Err(e) => {
                    // Warned when it first failed; retries every 2 s.
                    log::debug!("{e}; retrying");
                    output_failed.store(true, Ordering::SeqCst);
                    retry_output_at = Some(Instant::now() + Duration::from_secs(2));
                }
            }
        }

        let Some(t) = track.clone() else { continue };
        if ended {
            continue;
        }
        if let Some(e) = t.error.lock().unwrap().take() {
            log::warn!("playback error: {e}");
            stop(&mixer, &mut track);
            (ended, want_playing) = (true, false);
            mixer.playing.store(false, Ordering::Relaxed);
            on_event(PlayerEvent::Error(e));
            continue;
        }
        if t.drained.load(Ordering::SeqCst) {
            (ended, want_playing) = (true, false);
            mixer.playing.store(false, Ordering::Relaxed);
            on_event(PlayerEvent::Eos);
            continue;
        }

        let ready = t.ready.load(Ordering::SeqCst);
        let mut position = t.position();
        if ready {
            seek_target = None;
        } else if let Some(target) = seek_target {
            position = target;
        }
        let duration = *t.duration.lock().unwrap();
        // Report the intent, so the Play/Pause button reflects the last click.
        let playing = want_playing;
        let finished = t.finished.load(Ordering::SeqCst);
        // Waiting for data: starting, seeking, or the buffer ran dry mid-track
        // (network stall); the position doesn't move meanwhile.
        let buffering = !finished && (!ready || (want_playing && t.buf.lock().unwrap().is_empty()));
        let tick = (position, duration, playing, buffering);
        if tick != last_tick {
            last_tick = tick;
            log::trace!("tick {position:.2}/{duration:.0} playing={playing} buffering={buffering}");
            on_event(PlayerEvent::Tick { position, duration, playing, buffering });
        }
    }
    stop(&mixer, &mut track);
}

/// The volume is the slider's gain times the track's correction.
struct Levels {
    slider: f64,
    track: f64,
}

impl Levels {
    fn value(&self) -> f64 {
        gain(self.slider) * self.track.clamp(0.0, 1.0)
    }
}

fn load(mixer: &Mixer, current: &mut Option<Arc<Track>>, uri: String, start: f64) {
    stop(mixer, current);
    let t = Arc::new(Track::new(start));
    *mixer.current.lock().unwrap() = Some(t.clone());
    *current = Some(t.clone());
    let decoder = t.clone();
    let spawned = std::thread::Builder::new().name("decoder".into()).spawn(move || {
        if let Err(e) = decode(&decoder, &uri, start) {
            if !decoder.cancelled.load(Ordering::SeqCst) {
                decoder.fail(e);
            }
        }
    });
    if let Err(e) = spawned {
        t.fail(format!("could not start the decoder: {e}"));
    }
}

fn stop(mixer: &Mixer, current: &mut Option<Arc<Track>>) {
    if let Some(t) = current.take() {
        t.cancel();
    }
    *mixer.current.lock().unwrap() = None;
}

// ---- decoder -------------------------------------------------------------------

fn decode(t: &Track, uri: &str, start: f64) -> Result<(), String> {
    let source = HttpSource::open(uri)?;
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut reader = symphonia::default::formats::MkvReader::try_new(stream, &FormatOptions::default())
        .map_err(|e| format!("not a playable stream: {e}"))?;
    let (track_id, time_base, pre_skip) = {
        let tr = reader
            .tracks()
            .iter()
            .find(|tr| tr.codec_params.codec == CODEC_TYPE_OPUS)
            .ok_or("no Opus audio in the stream")?;
        let tb = tr.codec_params.time_base.ok_or("stream without time base")?;
        if let Some(n) = tr.codec_params.n_frames {
            let d = tb.calc_time(n);
            *t.duration.lock().unwrap() = d.seconds as f64 + d.frac;
        }
        // OpusHead: samples to drop at the very start (encoder delay).
        let pre_skip = tr.codec_params.extra_data.as_deref().filter(|h| h.len() >= 12).map_or(0, |h| u16::from_le_bytes([h[10], h[11]]));
        (tr.id, tb, usize::from(pre_skip))
    };
    let mut opus = opus::Decoder::new(RATE, opus::Channels::Stereo).map_err(|e| format!("opus: {e}"))?;
    let mut pcm = vec![0f32; 5760 * CHANNELS]; // 120 ms, the longest Opus frame
    let secs = |ts: u64| {
        let time = time_base.calc_time(ts);
        time.seconds as f64 + time.frac
    };

    // Audio before `skip_until` (s) is decoded but not played; at the
    // start the encoder delay is skipped instead.
    let mut skip_until = 0.0;
    let mut skip_frames = pre_skip;
    if start > 0.0 {
        *t.seek.lock().unwrap() = Some(start);
    }

    loop {
        if t.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        let pending = t.seek.lock().unwrap().take();
        if let Some(to) = pending {
            // Silent until audio from the new position is buffered (the
            // seek may take a network round trip).
            t.ready.store(false, Ordering::SeqCst);
            let at_end = seek(&mut reader, track_id, to)?;
            opus.reset_state().map_err(|e| format!("opus: {e}"))?;
            t.buf.lock().unwrap().clear();
            *t.base.lock().unwrap() = to;
            t.played.store(0, Ordering::Relaxed);
            t.drained.store(false, Ordering::SeqCst);
            t.finished.store(false, Ordering::SeqCst);
            (skip_until, skip_frames) = (to, 0);
            if at_end {
                mark_finished(t);
                continue;
            }
        }
        if t.finished.load(Ordering::SeqCst) {
            // At the end: wait for a seek back, or for being replaced.
            let buf = t.buf.lock().unwrap();
            let _ = t.changed.wait_timeout(buf, Duration::from_millis(200));
            continue;
        }

        let packet = match reader.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                mark_finished(t);
                continue;
            }
            Err(e) => return Err(format!("reading the stream failed: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let frames = match opus.decode_float(&packet.data, &mut pcm, false) {
            Ok(n) => n,
            Err(e) => {
                log::debug!("skipping an undecodable Opus packet: {e}");
                continue;
            }
        };
        let packet_start = secs(packet.ts);
        let mut from = skip_frames.min(frames);
        skip_frames -= from;
        if skip_until > packet_start {
            from = from.max((((skip_until - packet_start) * f64::from(RATE)) as usize).min(frames));
        }
        if from >= frames {
            continue;
        }
        let samples = &pcm[from * CHANNELS..frames * CHANNELS];

        // Wait for room; a seek or cancel makes these samples stale.
        let seek = t.seek.lock().unwrap();
        if seek.is_some() {
            continue;
        }
        let mut buf = t.buf.lock().unwrap();
        drop(seek);
        // (Taking `seek` while holding `buf` is fine: nothing holds `seek`
        // while waiting for `buf`.)
        while buf.len() + samples.len() > BUFFER_FRAMES * CHANNELS && !t.cancelled.load(Ordering::SeqCst) {
            buf = t.changed.wait_timeout(buf, Duration::from_millis(100)).unwrap().0;
            if t.seek.lock().unwrap().is_some() {
                break;
            }
        }
        // Push and mark ready only while no seek is pending, under its lock,
        // so a seek requested meanwhile never hears these samples.
        let seek = t.seek.lock().unwrap();
        if seek.is_some() || t.cancelled.load(Ordering::SeqCst) {
            continue;
        }
        buf.extend(samples);
        t.ready.store(true, Ordering::SeqCst);
        drop(seek);
        drop(buf);
    }
}

/// The stream is at its end: once the buffer is played, the track ends.
/// `ready` lets the output drain it; not while a seek is pending.
fn mark_finished(t: &Track) {
    let seek = t.seek.lock().unwrap();
    if seek.is_none() {
        t.finished.store(true, Ordering::SeqCst);
        t.ready.store(true, Ordering::SeqCst);
    }
}

/// Seeks so decoding starts a little before `to` (see `SEEK_PREROLL`).
/// Returns true if `to` is past the end (nothing left to play).
fn seek(reader: &mut dyn FormatReader, track_id: u32, to: f64) -> Result<bool, String> {
    use symphonia::core::errors::Error;
    let from = (to - SEEK_PREROLL).max(0.0);
    match reader.seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(from), track_id: Some(track_id) }) {
        Ok(_) => Ok(false),
        // The demuxer reports a position past the last frame as the end of
        // the stream (and one before the first cue as out of range).
        Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(true),
        // Before the first index entry (rare: indexes start at 0): reading
        // continues where it was.
        Err(Error::SeekError(symphonia::core::errors::SeekErrorKind::OutOfRange)) => Ok(false),
        Err(e) => Err(format!("seeking failed: {e}")),
    }
}

// ---- HTTP source -----------------------------------------------------------------

fn http() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            // The proxy may wait out YouTube's refusal window before answering.
            .timeout(None)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("HTTP client")
    })
}

/// A seekable byte stream over HTTP range requests (to the local proxy).
/// Reads go through one open response; a seek far away opens a new one.
/// The start of the file is kept: reading the header jumps back and forth
/// in it, and every new request makes the proxy fetch a chunk upstream.
struct HttpSource {
    url: String,
    len: u64,
    pos: u64,
    head: Vec<u8>,
    body: Option<(u64, reqwest::blocking::Response)>,
}

/// Bytes kept from the start of the file: header and seek index (Cues).
const HEAD_LEN: u64 = 128 * 1024;

/// Forward seeks up to this far read through the open response instead of
/// starting a new request.
const SKIP_LIMIT: u64 = 256 * 1024;

impl HttpSource {
    fn open(url: &str) -> Result<Self, String> {
        let mut s = Self { url: url.to_string(), len: 0, pos: 0, head: Vec::new(), body: None };
        let mut resp = s.request(0).map_err(|e| e.to_string())?;
        let len = content_range_total(&resp)
            .or_else(|| resp.content_length())
            .ok_or("stream without a length")?;
        let mut head = Vec::with_capacity(HEAD_LEN.min(len) as usize);
        (&mut resp).take(HEAD_LEN.min(len)).read_to_end(&mut head).map_err(|e| format!("reading the stream failed: {e}"))?;
        s.len = len;
        s.body = Some((head.len() as u64, resp));
        s.head = head;
        Ok(s)
    }

    fn request(&self, from: u64) -> std::io::Result<reqwest::blocking::Response> {
        let resp = http()
            .get(&self.url)
            .header(reqwest::header::RANGE, format!("bytes={from}-"))
            .send()
            .map_err(std::io::Error::other)?;
        if !resp.status().is_success() {
            return Err(std::io::Error::other(format!("stream request failed: HTTP {}", resp.status())));
        }
        Ok(resp)
    }
}

fn content_range_total(resp: &reqwest::blocking::Response) -> Option<u64> {
    let v = resp.headers().get(reqwest::header::CONTENT_RANGE)?.to_str().ok()?;
    v.rsplit('/').next()?.parse().ok()
}

impl Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len || out.is_empty() {
            return Ok(0);
        }
        if let Some(rest) = self.head.get(self.pos as usize..).filter(|r| !r.is_empty()) {
            let n = rest.len().min(out.len());
            out[..n].copy_from_slice(&rest[..n]);
            self.pos += n as u64;
            return Ok(n);
        }
        // Reuse the open response if it is at (or a little before) `pos`.
        if let Some((at, body)) = &mut self.body {
            if *at < self.pos && self.pos - *at <= SKIP_LIMIT {
                let skip = self.pos - *at;
                std::io::copy(&mut body.take(skip), &mut std::io::sink())?;
                *at += skip;
            }
        }
        if !matches!(&self.body, Some((at, _)) if *at == self.pos) {
            self.body = Some((self.pos, self.request(self.pos)?));
        }
        let (at, body) = self.body.as_mut().unwrap();
        let n = body.read(out)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "stream ended early"));
        }
        *at += n as u64;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek out of range"))?;
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}
