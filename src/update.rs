//! In-app updates via Tauri's updater: releases publish a signed
//! `latest.json` (see `.github/workflows/build.yml`); the app checks it,
//! downloads the new build, verifies its signature against the public key in
//! `tauri.conf.json`, installs it and restarts.
//!
//! Installs that can't replace themselves (a Linux .deb/.rpm, or an
//! AppImage in a directory the user can't write to) only get a download link.

use crate::core::{CoreHandle, CoreMsg};
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_updater::{Update, UpdaterExt};

/// Releases page, for installs that can't update themselves.
pub const RELEASES_URL: &str = "https://github.com/0xZaphod/tubedesk/releases/latest";
/// How often the automatic check runs (after one shortly after start).
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
    /// This install can replace itself; otherwise offer `RELEASES_URL`.
    pub installable: bool,
}

/// Sent as `update_progress` while downloading.
#[derive(Clone, Serialize)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

async fn find<R: Runtime>(app: &AppHandle<R>) -> Result<Option<Update>, String> {
    let mut builder = app.updater_builder();
    // For testing against a local server (with a build that allows http).
    if let Ok(url) = std::env::var("TUBEDESK_UPDATE_URL") {
        let url = url.parse().map_err(|e| format!("TUBEDESK_UPDATE_URL: {e}"))?;
        builder = builder.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    let updater = builder.build().map_err(|e| e.to_string())?;
    updater.check().await.map_err(|e| e.to_string())
}

/// The newest release, if it is newer than this build.
pub async fn check<R: Runtime>(app: &AppHandle<R>) -> Result<Option<UpdateInfo>, String> {
    Ok(find(app).await?.map(|u| UpdateInfo {
        version: u.version.clone(),
        current: u.current_version.clone(),
        notes: u.body.clone(),
        installable: installable(),
    }))
}

/// Whether this copy can replace itself.
pub fn installable() -> bool {
    if cfg!(target_os = "linux") {
        // Only AppImages: packages belong to the system's package manager.
        // The updater renames the AppImage, so its directory must be writable.
        let Some(image) = std::env::var_os("APPIMAGE").map(std::path::PathBuf::from) else { return false };
        let Some(dir) = image.parent() else { return false };
        let probe = dir.join(format!(".tubedesk-update-check-{}", std::process::id()));
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    } else {
        true
    }
}

/// Downloads, verifies and installs the update, then restarts. The session
/// is saved first (on Windows the installer ends this process itself).
pub async fn install<R: Runtime>(app: &AppHandle<R>, core: &CoreHandle) -> Result<(), String> {
    if !installable() {
        return Err("This copy can't update itself; download the new version instead".into());
    }
    let update = find(app).await?.ok_or("TubeDesk is up to date")?;
    log::info!("updating {} -> {}", update.current_version, update.version);
    let mut downloaded = 0u64;
    let bytes = update
        .download(
            |chunk, total| {
                downloaded += chunk as u64;
                let _ = app.emit("update_progress", Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    save_session(core);
    update.install(bytes).map_err(|e| format!("installing failed: {e}"))?;
    log::info!("update installed; restarting");
    app.restart();
}

/// Lets the core write the session before the process is replaced.
pub fn save_session(core: &CoreHandle) {
    let (tx, rx) = std::sync::mpsc::channel();
    core.send(CoreMsg::Shutdown(tx));
    if rx.recv_timeout(Duration::from_secs(2)).is_err() {
        log::warn!("session may not have been saved");
    }
}

/// Checks shortly after start and then every few hours while `enabled()`;
/// a found update is sent to the UI as the `update` event.
pub fn spawn_checks<R: Runtime>(app: AppHandle<R>, enabled: impl Fn() -> bool + Send + 'static) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_AFTER).await;
        loop {
            if enabled() {
                match check(&app).await {
                    Ok(Some(info)) => {
                        log::info!("update available: {}", info.version);
                        let _ = app.emit("update", &info);
                    }
                    Ok(None) => log::debug!("no update available"),
                    Err(e) => log::debug!("update check failed: {e}"),
                }
            }
            tokio::time::sleep(CHECK_EVERY).await;
        }
    });
}
