//! Resolves audio stream URLs with yt-dlp.
//!
//! rustypipe without its separate Botguard binary falls back to clients whose
//! streams YouTube cuts off after ~1 MB (no PO token). yt-dlp is maintained
//! for exactly this, solves the JS challenges with deno, and tells us which
//! HTTP headers its URLs need.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::{Notify, Semaphore};

/// Background lookups run on limited lanes: each yt-dlp run also starts deno
/// to solve YouTube's JS challenges, and a burst of them slows everything
/// down, including YouTube's warm-up and the UI. Speculative hints (hover)
/// have their own lane, so they never delay the queue's next tracks.
static AHEAD: Semaphore = Semaphore::const_new(2);
static SPECULATIVE: Semaphore = Semaphore::const_new(1);
/// A speculative hint that waited this long for its slot is no longer worth it.
const STALE_HINT: Duration = Duration::from_secs(20);

/// Lets a queued background lookup skip its queue when the user starts
/// waiting for it, instead of running a second lookup of the same track.
#[derive(Default)]
pub struct Promotion {
    set: AtomicBool,
    notify: Notify,
}

impl Promotion {
    pub fn promote(&self) {
        self.set.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The track the user is waiting for: never queued.
    Now,
    /// Upcoming queue tracks and likely picks (a list's first track).
    Ahead,
    /// Hover hints.
    Speculative,
}

/// Stream URLs expire after ~6 h; don't reuse cached ones near that.
/// Used when the URL carries no `expire=` time.
const MAX_AGE: Duration = Duration::from_secs(4 * 3600);
/// A URL needs at least this long left to be used for a new playback.
const MIN_LEFT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct Stream {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub size: u64,
    pub mime: String,
    /// Track length in seconds, as reported by yt-dlp.
    pub duration: Option<f64>,
    /// When YouTube stops accepting the URL, on the monotonic clock. Derived
    /// from the URL's `expire=` time once, when fetched (see `expiry`), so a
    /// wrong system clock can't make every URL look expired.
    pub expires: Option<Instant>,
    fetched: Instant,
}

impl Stream {
    pub fn is_fresh(&self) -> bool {
        match self.expires {
            Some(t) => t.saturating_duration_since(Instant::now()) > MIN_LEFT,
            None => self.fetched.elapsed() < MAX_AGE,
        }
    }

    pub fn age(&self) -> Duration {
        self.fetched.elapsed()
    }
}

#[derive(Deserialize)]
struct YtDlpFormat {
    id: String,
    url: String,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    acodec: Option<String>,
    ext: Option<String>,
    format_id: Option<String>,
    abr: Option<f64>,
    duration: Option<f64>,
}

/// A *quick* URL for the start of a stream, from rustypipe (~0.1–0.6 s, vs
/// ~2.5 s for yt-dlp plus ~3 s until its URL is accepted). Without a PO
/// token YouTube only serves the first ~500 KB of it, but that is ~30 s of
/// audio: plenty of time for the full URL to take over (see `proxy`).
/// Uses the same itag `resolve` asks yt-dlp for.
pub async fn quick(rp: &rustypipe::client::RustyPipe, id: &str, high_quality: bool) -> Result<Quick, String> {
    let itag = if high_quality { 251 } else { 249 };
    let player = rp.query().unauthenticated().player(id).await.map_err(|e| e.to_string())?;
    let s = player
        .audio_streams
        .iter()
        .find(|s| s.itag == itag && s.drm_track_type.is_none())
        .ok_or_else(|| format!("no itag {itag} stream"))?;
    Ok(Quick { url: s.url.clone(), size: s.size, mime: s.mime.clone(), loudness_db: s.loudness_db })
}

pub struct Quick {
    pub url: String,
    pub size: u64,
    pub mime: String,
    /// See `rustypipe::model::AudioStream::loudness_db`.
    pub loudness_db: Option<f32>,
}

/// Looks up `ids` in one yt-dlp run (saving a Python start and a JS
/// challenge solve per extra track). `on_result` is called once per id, as
/// soon as that video is done (yt-dlp prints one JSON line per video), so the
/// first track of a batch isn't held back by the others.
/// A queued lookup stops waiting for its slot once `promotion` is set.
pub async fn resolve(
    ytdlp: &str,
    ids: &[String],
    high_quality: bool,
    lane: Lane,
    promotion: Arc<Promotion>,
    mut on_result: impl FnMut(String, Result<Stream, String>),
) {
    let all_failed = |e: String, on_result: &mut dyn FnMut(String, Result<Stream, String>)| {
        for id in ids {
            on_result(id.clone(), Err(e.clone()));
        }
    };
    let queued = Instant::now();
    let slots = match lane {
        Lane::Now => None,
        Lane::Ahead => Some(&AHEAD),
        Lane::Speculative => Some(&SPECULATIVE),
    };
    let _permit = match slots {
        None => None,
        Some(_) if promotion.is_set() => None,
        Some(slots) => tokio::select! {
            permit = slots.acquire() => match permit {
                Ok(p) => Some(p),
                Err(e) => return all_failed(e.to_string(), &mut on_result),
            },
            _ = promotion.notify.notified() => None,
        },
    };
    if _permit.is_none() && lane != Lane::Now {
        log::debug!("lookup of {} promoted: the track is wanted now", ids.join(", "));
    }
    if lane == Lane::Speculative && queued.elapsed() > STALE_HINT && !promotion.is_set() {
        return all_failed("hint dropped (waited too long)".into(), &mut on_result);
    }
    // Prefer the exact itag `quick` uses, so its bytes and ours are the same file.
    let format = if high_quality {
        "251/bestaudio[acodec=opus]/bestaudio"
    } else {
        "249/worstaudio[acodec=opus]/worstaudio"
    };
    let urls: Vec<String> = ids.iter().map(|id| format!("https://music.youtube.com/watch?v={id}")).collect();
    let mut cmd = Command::new(ytdlp);
    crate::appimage::host_env(&mut cmd);
    let mut child = match cmd
        .args(["--quiet", "--no-warnings", "--no-playlist", "-f", format, "-j"])
        .args(&urls)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return all_failed(format!("could not run {ytdlp} (is yt-dlp installed?): {e}"), &mut on_result),
    };
    // Collect stderr concurrently (a full pipe would block yt-dlp).
    let stderr_task = child.stderr.take().map(|mut err| {
        tokio::spawn(async move {
            let mut buf = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut err, &mut buf).await;
            buf
        })
    });
    // One JSON object per line for each video that worked (yt-dlp goes on
    // after a failed one and reports it on stderr).
    let mut pending: Vec<String> = ids.to_vec();
    if let Some(out) = child.stdout.take() {
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(out));
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<YtDlpFormat>(&line) {
                Ok(f) => {
                    if let Some(k) = pending.iter().position(|id| *id == f.id) {
                        let id = pending.remove(k);
                        let s = stream(&id, f);
                        on_result(id, Ok(s));
                    }
                }
                Err(e) => log::warn!("unexpected yt-dlp output: {e}"),
            }
        }
    }
    let _ = child.wait().await;
    let stderr = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    for id in pending {
        let lines = || stderr.lines().map(str::trim).filter(|l| !l.is_empty());
        let line = lines().rfind(|l| l.contains(id.as_str())).or_else(|| lines().next_back());
        let msg = line.unwrap_or("yt-dlp returned no stream").to_string();
        on_result(id, Err(msg));
    }
}

/// URLs normally live ~6 h. A remaining lifetime outside this range means the
/// system clock is off (e.g. RTC in local time on a dual-boot machine); then
/// the expire time is ignored and the age rule (MAX_AGE) applies instead.
const PLAUSIBLE_LIFETIME: std::ops::RangeInclusive<Duration> = Duration::from_secs(10 * 60)..=Duration::from_secs(8 * 3600);

/// Turns the absolute `expire=` time into a monotonic deadline, using the
/// system clock only once, right after the URL was issued.
fn expiry(expire_unix_secs: u64) -> Option<Instant> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let left = Duration::from_secs(expire_unix_secs).checked_sub(now);
    match left {
        Some(left) if PLAUSIBLE_LIFETIME.contains(&left) => Some(Instant::now() + left),
        _ => {
            log::debug!("ignoring implausible stream expiry (system clock off?)");
            None
        }
    }
}

fn url_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split(['?', '&']).find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
}

fn stream(id: &str, f: YtDlpFormat) -> Stream {
    // Fall back to the `clen` URL parameter when yt-dlp has no exact size.
    let clen = url_param(&f.url, "clen").and_then(|v| v.parse().ok());
    let size = f.filesize.or(clen).or(f.filesize_approx).unwrap_or(0);
    let expires = url_param(&f.url, "expire").and_then(|v| v.parse::<u64>().ok()).and_then(expiry);
    let mime = match (f.ext.as_deref(), f.acodec.as_deref()) {
        (Some("webm"), _) | (_, Some("opus")) => "audio/webm",
        (Some("m4a"), _) | (Some("mp4"), _) => "audio/mp4",
        _ => "application/octet-stream",
    };
    log::info!(
        "resolved {id}: format {} {} {:.0} kbps, {} bytes",
        f.format_id.as_deref().unwrap_or("?"),
        f.acodec.as_deref().unwrap_or("?"),
        f.abr.unwrap_or(0.0),
        size
    );
    Stream {
        url: f.url,
        headers: f.http_headers.into_iter().collect(),
        size,
        mime: mime.to_string(),
        duration: f.duration,
        expires,
        fetched: Instant::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn expiry_uses_a_monotonic_deadline() {
        // A normal URL: ~6 h left.
        let e = expiry(unix_now() + 6 * 3600).expect("plausible");
        let left = e.saturating_duration_since(Instant::now());
        assert!(left > Duration::from_secs(6 * 3600 - 5) && left <= Duration::from_secs(6 * 3600));
    }

    #[test]
    fn expiry_ignores_skewed_clocks() {
        // Clock 6 h fast: the URL seems to expire right away / already expired.
        assert!(expiry(unix_now()).is_none());
        assert!(expiry(unix_now() - 3600).is_none());
        // Clock 6 h slow: the URL seems to live 12 h.
        assert!(expiry(unix_now() + 12 * 3600).is_none());
    }

    #[test]
    fn streams_without_expiry_fall_back_to_age() {
        let s = Stream {
            url: String::new(),
            headers: vec![],
            size: 1,
            mime: String::new(),
            duration: None,
            expires: None,
            fetched: Instant::now(),
        };
        assert!(s.is_fresh());
    }
}
