//! Resolves audio stream URLs with yt-dlp.
//!
//! rustypipe without its separate Botguard binary falls back to clients whose
//! streams YouTube cuts off after ~1 MB (no PO token). yt-dlp is maintained
//! for exactly this, solves the JS challenges with deno, and tells us which
//! HTTP headers its URLs need.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Background lookups run on limited lanes: each yt-dlp run also starts deno
/// to solve YouTube's JS challenges, and a burst of them slows everything
/// down, including YouTube's warm-up and the UI. Speculative hints (hover)
/// have their own lane, so they never delay the queue's next tracks.
static AHEAD: Semaphore = Semaphore::const_new(2);
static SPECULATIVE: Semaphore = Semaphore::const_new(1);
/// A speculative hint that waited this long for its slot is no longer worth it.
const STALE_HINT: Duration = Duration::from_secs(20);

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
    /// When YouTube stops accepting the URL (its `expire=` parameter).
    pub expires: Option<SystemTime>,
    fetched: Instant,
}

impl Stream {
    pub fn is_fresh(&self) -> bool {
        match self.expires {
            Some(t) => t.duration_since(SystemTime::now()).is_ok_and(|left| left > MIN_LEFT),
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
/// Returns (url, size, mime) of the same itag `resolve` asks yt-dlp for.
pub async fn quick(rp: &rustypipe::client::RustyPipe, id: &str, high_quality: bool) -> Result<(String, u64, String), String> {
    let itag = if high_quality { 251 } else { 249 };
    let player = rp.query().unauthenticated().player(id).await.map_err(|e| e.to_string())?;
    let s = player
        .audio_streams
        .iter()
        .find(|s| s.itag == itag && s.drm_track_type.is_none())
        .ok_or_else(|| format!("no itag {itag} stream"))?;
    Ok((s.url.clone(), s.size, s.mime.clone()))
}

/// Looks up `ids` in one yt-dlp run (saving a Python start and a JS
/// challenge solve per extra track). Returns a result for every id.
pub async fn resolve(ytdlp: &str, ids: &[String], high_quality: bool, lane: Lane) -> Vec<(String, Result<Stream, String>)> {
    let all_failed = |e: String| ids.iter().map(|id| (id.clone(), Err(e.clone()))).collect();
    let queued = Instant::now();
    let _permit = match lane {
        Lane::Now => None,
        Lane::Ahead => match AHEAD.acquire().await {
            Ok(p) => Some(p),
            Err(e) => return all_failed(e.to_string()),
        },
        Lane::Speculative => {
            let permit = match SPECULATIVE.acquire().await {
                Ok(p) => p,
                Err(e) => return all_failed(e.to_string()),
            };
            if queued.elapsed() > STALE_HINT {
                return all_failed("hint dropped (waited too long)".into());
            }
            Some(permit)
        }
    };
    // Prefer the exact itag `quick` uses, so its bytes and ours are the same file.
    let format = if high_quality {
        "251/bestaudio[acodec=opus]/bestaudio"
    } else {
        "249/worstaudio[acodec=opus]/worstaudio"
    };
    let urls: Vec<String> = ids.iter().map(|id| format!("https://music.youtube.com/watch?v={id}")).collect();
    let out = match Command::new(ytdlp)
        .args(["--quiet", "--no-warnings", "--no-playlist", "-f", format, "-j"])
        .args(&urls)
        .kill_on_drop(true)
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => return all_failed(format!("could not run {ytdlp} (is yt-dlp installed?): {e}")),
    };
    // One JSON object per line for each video that worked (yt-dlp goes on
    // after a failed one and reports it on stderr).
    let mut found: HashMap<String, YtDlpFormat> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<YtDlpFormat>(l).map_err(|e| log::warn!("unexpected yt-dlp output: {e}")).ok())
        .map(|f| (f.id.clone(), f))
        .collect();
    let stderr = String::from_utf8_lossy(&out.stderr);
    ids.iter()
        .map(|id| {
            let result = match found.remove(id) {
                Some(f) => Ok(stream(id, f)),
                None => {
                    let lines = || stderr.lines().map(str::trim).filter(|l| !l.is_empty());
                    let line = lines().rfind(|l| l.contains(id.as_str())).or_else(|| lines().next_back());
                    Err(line.unwrap_or("yt-dlp returned no stream").to_string())
                }
            };
            (id.clone(), result)
        })
        .collect()
}

fn url_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    url.split(['?', '&']).find_map(|p| p.strip_prefix(key)?.strip_prefix('='))
}

fn stream(id: &str, f: YtDlpFormat) -> Stream {
    // Fall back to the `clen` URL parameter when yt-dlp has no exact size.
    let clen = url_param(&f.url, "clen").and_then(|v| v.parse().ok());
    let size = f.filesize.or(clen).or(f.filesize_approx).unwrap_or(0);
    let expires = url_param(&f.url, "expire")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
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
