//! Resolves audio stream URLs with yt-dlp.
//!
//! rustypipe without its separate Botguard binary falls back to clients whose
//! streams YouTube cuts off after ~1 MB (no PO token). yt-dlp is maintained
//! for exactly this, solves the JS challenges with deno, and tells us which
//! HTTP headers its URLs need.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Background lookups (prefetches) run at most this many at once: each
/// yt-dlp run also starts deno to solve YouTube's JS challenges, and a burst
/// of them slows everything down, including YouTube's warm-up and the UI.
static BACKGROUND: Semaphore = Semaphore::const_new(2);

/// Stream URLs expire after ~6 h; don't reuse cached ones near that.
const MAX_AGE: Duration = Duration::from_secs(4 * 3600);

#[derive(Debug, Clone)]
pub struct Stream {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub size: u64,
    pub mime: String,
    fetched: Instant,
}

impl Stream {
    pub fn is_fresh(&self) -> bool {
        self.fetched.elapsed() < MAX_AGE
    }

    pub fn age(&self) -> Duration {
        self.fetched.elapsed()
    }
}

#[derive(Deserialize)]
struct YtDlpFormat {
    url: String,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    acodec: Option<String>,
    ext: Option<String>,
    format_id: Option<String>,
    abr: Option<f64>,
}

/// `background`: wait for one of the limited background slots first.
pub async fn resolve(ytdlp: &str, id: &str, high_quality: bool, background: bool) -> Result<Stream, String> {
    let _permit = if background { Some(BACKGROUND.acquire().await.map_err(|e| e.to_string())?) } else { None };
    let format = if high_quality {
        "bestaudio[acodec=opus]/bestaudio"
    } else {
        "worstaudio[acodec=opus]/worstaudio"
    };
    let url = format!("https://music.youtube.com/watch?v={id}");
    let out = Command::new(ytdlp)
        .args(["--quiet", "--no-warnings", "--no-playlist", "-f", format, "-j", &url])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run {ytdlp} (is yt-dlp installed?): {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("yt-dlp failed");
        return Err(line.trim().to_string());
    }
    let f: YtDlpFormat = serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected yt-dlp output: {e}"))?;

    // Fall back to the `clen` URL parameter when yt-dlp has no exact size.
    let clen = f
        .url
        .split(['?', '&'])
        .find_map(|p| p.strip_prefix("clen="))
        .and_then(|v| v.parse().ok());
    let size = f.filesize.or(clen).or(f.filesize_approx).unwrap_or(0);
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
    Ok(Stream {
        url: f.url,
        headers: f.http_headers.into_iter().collect(),
        size,
        mime: mime.to_string(),
        fetched: Instant::now(),
    })
}
