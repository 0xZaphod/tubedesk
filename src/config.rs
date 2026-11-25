use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Stored as `config.json` in the app config dir
/// (`~/.config/dev.tubedesk.app/config.json` on Linux).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Best available audio (Opus ~160 kbps) instead of the smallest stream.
    pub high_quality: bool,
    /// When the queue runs out, continue with YouTube Music's radio for the last track.
    pub autoplay_radio: bool,
    /// Volume slider position, 0.0–1.0 (remembered). Mapped to loudness on a
    /// perceptual curve with the maximum at −6 dB, see `player::gain`.
    pub volume: f64,
    /// Expose a native MPRIS / media-key player.
    pub media_controls: bool,
    /// Closing the window hides it to the tray (music keeps playing) instead
    /// of quitting. Off by default: the close button quits.
    pub hide_on_close: bool,
    /// User agent for the Google sign-in window (None = webview default).
    /// Try a desktop Firefox/Safari UA if Google refuses to sign in.
    pub login_user_agent: Option<String>,
    /// yt-dlp executable used to resolve audio streams.
    pub ytdlp: String,
    #[serde(skip)]
    path: PathBuf,
    /// False if the file exists but could not be parsed: it is then never
    /// overwritten, so a typo doesn't cost the user their settings.
    #[serde(skip)]
    writable: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            high_quality: true,
            autoplay_radio: true,
            volume: 0.35,
            media_controls: true,
            hide_on_close: false,
            login_user_agent: None,
            ytdlp: "yt-dlp".into(),
            path: PathBuf::new(),
            writable: true,
        }
    }
}

impl Config {
    /// Loads the config, writing defaults if the file does not exist yet.
    pub fn load_or_init(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("invalid {}: {e}; using defaults (the file is left as it is)", path.display());
                Self { writable: false, ..Self::default() }
            }),
            Err(_) => Self::default(),
        };
        cfg.path = path.to_path_buf();
        if !path.exists() {
            cfg.save();
        }
        cfg
    }

    pub fn save(&self) {
        if !self.writable {
            log::warn!("not saving settings: fix {} and restart", self.path.display());
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = write_atomic(&self.path, text.as_bytes()) {
                    log::warn!("could not write {}: {e}", self.path.display());
                }
            }
            Err(e) => log::warn!("could not serialize config: {e}"),
        }
    }
}

/// Writes `path` via a temporary file and a rename, so a crash mid-write
/// leaves the old contents instead of a truncated file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
