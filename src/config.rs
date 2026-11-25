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
    /// Playback volume, 0.0–1.0 (remembered).
    pub volume: f64,
    /// Expose a native MPRIS / media-key player.
    pub media_controls: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// User agent for the Google sign-in window (None = webview default).
    /// Try a desktop Firefox/Safari UA if Google refuses to sign in.
    pub login_user_agent: Option<String>,
    /// yt-dlp executable used to resolve audio streams.
    pub ytdlp: String,
    #[serde(skip)]
    path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            high_quality: true,
            autoplay_radio: true,
            volume: 0.8,
            media_controls: true,
            close_to_tray: true,
            login_user_agent: None,
            ytdlp: "yt-dlp".into(),
            path: PathBuf::new(),
        }
    }
}

impl Config {
    /// Loads the config, writing defaults if the file does not exist yet.
    pub fn load_or_init(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("invalid {}: {e}; using defaults", path.display());
                Self::default()
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
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&self.path, text) {
                    log::warn!("could not write {}: {e}", self.path.display());
                }
            }
            Err(e) => log::warn!("could not serialize config: {e}"),
        }
    }
}
