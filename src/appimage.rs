//! Running from an AppImage. Its launcher points `LD_LIBRARY_PATH`,
//! `PYTHONHOME`, `PATH`, GStreamer and GTK variables into the image. That is
//! right for TubeDesk itself, but host programs it starts (yt-dlp, a Python
//! script) break on it, so they get the variables without the image's entries.

use std::path::Path;

fn appdir() -> Option<String> {
    std::env::var("APPDIR").ok().filter(|d| !d.is_empty() && std::env::var_os("APPIMAGE").is_some())
}

/// Call before anything starts threads or GStreamer. The bundling hook
/// expects Debian's library layout; on other build hosts the plugin scanner
/// sits elsewhere in the image.
pub fn fix_environment() {
    let Some(dir) = appdir() else { return };
    let plugins = Path::new(&dir).join("usr/lib/gstreamer-1.0");
    let scanner = plugins.join("gst-plugin-scanner");
    let configured = std::env::var_os("GST_PLUGIN_SCANNER_1_0");
    if scanner.exists() && !configured.as_deref().is_some_and(|p| Path::new(p).exists()) {
        std::env::set_var("GST_PLUGIN_SCANNER_1_0", &scanner);
    }
    if plugins.is_dir() {
        stable_plugin_path(&plugins);
    }
}

/// The image is mounted at a new random path on every start, so GStreamer
/// would treat every bundled plugin as new and rebuild its registry (several
/// seconds). Point it at a fixed symlink to the current mount instead, with a
/// registry of its own (the default one is shared with the host's plugins).
fn stable_plugin_path(plugins: &Path) {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
        .map(|c| c.join("dev.tubedesk.app"));
    let Some(cache) = cache else { return };
    if std::fs::create_dir_all(&cache).is_err() {
        return;
    }
    let link = cache.join("appimage-gst-plugins");
    // Plugins are loaded lazily, so a running instance still needs its mount
    // behind the link: only repoint a link whose mount is gone. (A second
    // launch just hands over to the running instance and exits.)
    if !link.exists() {
        let tmp = cache.join(format!("appimage-gst-plugins.{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        if std::os::unix::fs::symlink(plugins, &tmp).is_err() || std::fs::rename(&tmp, &link).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
    }
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", &link);
    std::env::remove_var("GST_PLUGIN_PATH_1_0");
    std::env::set_var("GST_REGISTRY_1_0", cache.join("appimage-gst-registry.bin"));
}

/// Gives a child process the host's environment instead of the image's.
pub fn host_env(cmd: &mut tokio::process::Command) {
    let Some(dir) = appdir() else { return };
    for (key, value) in std::env::vars_os() {
        let Some(value) = value.to_str() else { continue };
        if !value.contains(dir.as_str()) {
            continue;
        }
        let kept: Vec<&str> = value.split(':').filter(|p| !p.is_empty() && !p.starts_with(dir.as_str())).collect();
        if kept.is_empty() {
            cmd.env_remove(&key);
        } else {
            cmd.env(&key, kept.join(":"));
        }
    }
    // Set by the launcher without pointing into the image.
    for key in ["PYTHONDONTWRITEBYTECODE", "GST_REGISTRY_REUSE_PLUGIN_SCANNER", "GTK_THEME"] {
        cmd.env_remove(key);
    }
}
