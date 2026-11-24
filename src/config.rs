use serde::{Deserialize, Serialize};
use std::path::Path;

pub const START_URL: &str = "https://music.youtube.com/";

/// Stored as `config.json` in the app config dir
/// (`~/.config/dev.tubedesk.app/config.json` on Linux).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Audio-only mode: never show video. Music videos are switched to their
    /// "Song" (audio) version when one exists, otherwise the video is hidden
    /// behind the album art.
    pub audio_only: bool,
    /// Master switch for all ad blocking (scriptlets, cosmetic and network rules).
    pub adblock: bool,
    /// Native network-level blocking (WebKit content filters on Linux).
    pub network_filtering: bool,
    /// Expose a native MPRIS / media-key player. Turn off if your desktop shows
    /// a duplicate player (newer WebKitGTK builds can publish their own).
    pub media_controls: bool,
    /// Closing the window hides it to the tray instead of quitting.
    pub close_to_tray: bool,
    /// Re-download filter lists when they are older than this many hours.
    pub filter_max_age_hours: u64,
    /// Override the webview user agent (None = webview default).
    pub user_agent: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio_only: true,
            adblock: true,
            network_filtering: true,
            media_controls: true,
            close_to_tray: true,
            filter_max_age_hours: 96,
            user_agent: None,
        }
    }
}

impl Config {
    /// Loads the config, writing defaults if the file does not exist yet.
    pub fn load_or_init(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("invalid {}: {e}; using defaults", path.display());
                Self::default()
            }),
            Err(_) => {
                let cfg = Self::default();
                cfg.save(path);
                cfg
            }
        }
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    log::warn!("could not write {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("could not serialize config: {e}"),
        }
    }
}
