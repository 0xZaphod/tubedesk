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
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(f64),
    Volume(f64),
}

#[derive(Debug, Clone)]
pub enum PlayerEvent {
    Eos,
    Error(String),
    Tick { position: f64, duration: f64, playing: bool },
}

#[derive(Clone)]
pub struct Player {
    tx: Sender<PlayerCmd>,
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
        std::thread::Builder::new()
            .name("player".into())
            .spawn(move || run(rx, volume, on_event, ready_tx))
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(Self { tx })
    }
}

fn run(
    rx: Receiver<PlayerCmd>,
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
    let _ = ready.send(Ok(()));

    let mut want_playing = false;
    // GStreamer often posts several errors for one failure; report only the first per load.
    let mut errored = false;
    let mut last_tick = (f64::NAN, f64::NAN, false);
    // While a seek is being carried out GStreamer reports no (or the old)
    // position; report the seek target instead so the UI doesn't jump to 0.
    let mut seek_target: Option<(f64, std::time::Instant)> = None;
    let mut last_pos = 0.0;

    loop {
        // Commands (block briefly so the loop also acts as a ~4 Hz ticker).
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(cmd) => {
                for cmd in std::iter::once(cmd).chain(rx.try_iter()) {
                    match &cmd {
                        PlayerCmd::Load(_) | PlayerCmd::Stop => {
                            errored = false;
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
        if let Some((target, at)) = seek_target {
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
        let tick = (position, duration, want_playing);
        if tick.0 != last_tick.0 || tick.1 != last_tick.1 || tick.2 != last_tick.2 {
            last_tick = tick;
            log::trace!("tick {position:.2}/{duration:.0} playing={want_playing}");
            on_event(PlayerEvent::Tick { position, duration, playing: want_playing });
        }
    }
    let _ = playbin.set_state(gst::State::Null);
}

fn handle(playbin: &gst::Element, cmd: PlayerCmd, want_playing: &mut bool) {
    match cmd {
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
    }
}
