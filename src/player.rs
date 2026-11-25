//! Audio playback with GStreamer's `playbin`, on a dedicated thread.

use gstreamer as gst;
use gst::prelude::*;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// Loudest the app ever plays: 50% of full scale (−6 dB).
const MAX_GAIN: f64 = 0.5;

/// Maps the 0–1 volume slider to an amplitude gain. A cubic curve matches how
/// loudness is perceived (like PulseAudio/PipeWire sliders), so the low end of
/// the slider is actually quiet: 20% → −48 dB, 50% → −24 dB, 100% → −6 dB.
fn gain(slider: f64) -> f64 {
    MAX_GAIN * slider.clamp(0.0, 1.0).powi(3)
}

pub enum PlayerCmd {
    Load(String),
    /// Load and start at a position: preroll paused, seek, then play.
    LoadAt(String, f64),
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(f64),
    Volume(f64),
    /// Internal: a bus message arrived, handle it now (not at the next tick).
    Wake,
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Eos,
    Error(String),
    /// `playing` is the intent (what Play/Pause last asked for); `buffering`
    /// is true while a resume seek or a seek is still being carried out.
    Tick { position: f64, duration: f64, playing: bool, buffering: bool },
}

#[derive(Clone)]
pub struct Player {
    /// Shared so the bus handler can hold a `Weak` to it: when every Player
    /// is dropped the channel disconnects and the player thread ends.
    tx: std::sync::Arc<Sender<PlayerCmd>>,
}

impl Player {
    pub fn send(&self, cmd: PlayerCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn spawn(
        volume: f64,
        on_event: impl Fn(PlayerEvent) + Send + 'static,
    ) -> Result<Self, String> {
        gst::init().map_err(|e| format!("GStreamer init failed: {e}"))?;
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let tx = std::sync::Arc::new(tx);
        std::thread::Builder::new()
            .name("player".into())
            .spawn({
                let wake = std::sync::Arc::downgrade(&tx);
                move || run(rx, wake, volume, on_event, ready_tx)
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { tx })
    }
}

fn run(
    rx: Receiver<PlayerCmd>,
    wake: std::sync::Weak<Sender<PlayerCmd>>,
    volume: f64,
    on_event: impl Fn(PlayerEvent),
    ready: Sender<Result<(), String>>,
) {
    let playbin = match gst::ElementFactory::make("playbin").build() {
        Ok(p) => p,
        Err(e) => {
            let _ = ready.send(Err(format!("playbin unavailable (install gst-plugins-base): {e}")));
            return;
        }
    };
    // Audio only: never build a video branch.
    playbin.set_property_from_str("flags", "audio+soft-volume");
    playbin.set_property("volume", gain(volume));
    // e.g. TUBEDESK_AUDIO_SINK="fakesink sync=true" for silent testing.
    if let Ok(desc) = std::env::var("TUBEDESK_AUDIO_SINK") {
        match gst::parse::bin_from_description(&desc, true) {
            Ok(sink) => playbin.set_property("audio-sink", &sink),
            Err(e) => log::warn!("invalid TUBEDESK_AUDIO_SINK: {e}"),
        }
    }
    let bus = playbin.bus().expect("playbin has a bus");
    // Wake the loop as soon as something it acts on is posted (end of track,
    // error, preroll done), instead of up to 250 ms later.
    // Only a weak handle, or it would keep the command channel open forever.
    bus.set_sync_handler(move |_, msg| {
        use gst::MessageView;
        if matches!(msg.view(), MessageView::Eos(..) | MessageView::Error(..) | MessageView::AsyncDone(..)) {
            if let Some(tx) = wake.upgrade() {
                let _ = tx.send(PlayerCmd::Wake);
            }
        }
        gst::BusSyncReply::Pass
    });
    let _ = ready.send(Ok(()));

    let mut want_playing = false;
    // GStreamer often posts several errors for one failure; report only the first per load.
    let mut errored = false;
    let mut last_tick = (f64::NAN, f64::NAN, false, false);
    // While a seek is being carried out GStreamer reports no (or the old)
    // position; report the seek target instead so the UI doesn't jump to 0.
    let mut seek_target: Option<(f64, std::time::Instant)> = None;
    let mut last_pos = 0.0;
    // LoadAt: position to seek to once the pipeline has prerolled (paused).
    let mut pending_start: Option<f64> = None;

    loop {
        // Commands (block briefly so the loop also acts as a ~4 Hz ticker).
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(cmd) => {
                for cmd in std::iter::once(cmd).chain(rx.try_iter()) {
                    match &cmd {
                        PlayerCmd::LoadAt(uri, pos) => {
                            errored = false;
                            pending_start = Some(*pos);
                            seek_target = None;
                            last_pos = *pos;
                            let _ = playbin.set_state(gst::State::Ready);
                            playbin.set_property("uri", uri);
                            want_playing = true;
                            let _ = playbin.set_state(gst::State::Paused);
                            continue;
                        }
                        // Still prerolling for LoadAt: only decide whether to
                        // play once the seek is done, never start early.
                        PlayerCmd::Play | PlayerCmd::Pause | PlayerCmd::Toggle if pending_start.is_some() => {
                            want_playing = match cmd {
                                PlayerCmd::Play => true,
                                PlayerCmd::Pause => false,
                                _ => !want_playing,
                            };
                            continue;
                        }
                        PlayerCmd::Load(_) | PlayerCmd::Stop => {
                            errored = false;
                            pending_start = None;
                            seek_target = None;
                            last_pos = 0.0;
                        }
                        PlayerCmd::Seek(secs) => seek_target = Some((secs.max(0.0), std::time::Instant::now())),
                        _ => {}
                    }
                    handle(&playbin, cmd, &mut want_playing);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Bus messages.
        while let Some(msg) = bus.pop() {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(..) => {
                    want_playing = false;
                    let _ = playbin.set_state(gst::State::Ready);
                    on_event(PlayerEvent::Eos);
                }
                MessageView::AsyncDone(..) if pending_start.is_some() => {
                    // Prerolled: seek while still paused, then play.
                    let pos = pending_start.take().unwrap();
                    let t = gst::ClockTime::from_nseconds((pos * 1e9) as u64);
                    let _ = playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t);
                    seek_target = Some((pos, std::time::Instant::now()));
                    if want_playing {
                        let _ = playbin.set_state(gst::State::Playing);
                    }
                }
                MessageView::Error(_) if errored => {}
                MessageView::Error(err) => {
                    errored = true;
                    want_playing = false;
                    let _ = playbin.set_state(gst::State::Ready);
                    let text = format!("{} ({:?})", err.error(), err.debug());
                    log::warn!("playback error: {text}");
                    on_event(PlayerEvent::Error(text));
                }
                _ => {}
            }
        }

        // Position tick.
        let mut position = playbin
            .query_position::<gst::ClockTime>()
            .map_or(last_pos, |t| t.nseconds() as f64 / 1e9);
        if let Some(pos) = pending_start {
            position = pos; // still prerolling for a LoadAt
        } else if let Some((target, at)) = seek_target {
            if (position - target).abs() < 1.5 || at.elapsed() > Duration::from_secs(3) {
                seek_target = None; // seek done (or gave up waiting)
            } else {
                position = target;
            }
        }
        last_pos = position;
        let duration = playbin
            .query_duration::<gst::ClockTime>()
            .map_or(0.0, |t| t.nseconds() as f64 / 1e9);
        // Report the intent, so the Play/Pause button reflects the last click
        // even while a resume seek is still in progress.
        let playing = want_playing;
        let buffering = pending_start.is_some() || seek_target.is_some();
        let tick = (position, duration, playing, buffering);
        if tick.0 != last_tick.0 || tick.1 != last_tick.1 || tick.2 != last_tick.2 || tick.3 != last_tick.3 {
            last_tick = tick;
            log::trace!("tick {position:.2}/{duration:.0} playing={playing} buffering={buffering}");
            on_event(PlayerEvent::Tick { position, duration, playing, buffering });
        }
    }
    let _ = playbin.set_state(gst::State::Null);
}

fn handle(playbin: &gst::Element, cmd: PlayerCmd, want_playing: &mut bool) {
    match cmd {
        PlayerCmd::LoadAt(..) => {} // handled in the loop (needs the preroll state)
        PlayerCmd::Load(uri) => {
            let _ = playbin.set_state(gst::State::Ready);
            playbin.set_property("uri", &uri);
            *want_playing = true;
            let _ = playbin.set_state(gst::State::Playing);
        }
        PlayerCmd::Play => {
            *want_playing = true;
            let _ = playbin.set_state(gst::State::Playing);
        }
        PlayerCmd::Pause => {
            *want_playing = false;
            let _ = playbin.set_state(gst::State::Paused);
        }
        PlayerCmd::Toggle => {
            let next = if *want_playing { PlayerCmd::Pause } else { PlayerCmd::Play };
            handle(playbin, next, want_playing);
        }
        PlayerCmd::Stop => {
            *want_playing = false;
            let _ = playbin.set_state(gst::State::Ready);
        }
        PlayerCmd::Seek(secs) => {
            let t = gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64);
            let _ = playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t);
        }
        PlayerCmd::Volume(v) => playbin.set_property("volume", gain(v)),
        PlayerCmd::Wake => {}
    }
}
