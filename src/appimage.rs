//! Running from an AppImage. Its launcher points `LD_LIBRARY_PATH`,
//! `PYTHONHOME`, `PATH` and GTK variables into the image. That is right for
//! TubeDesk itself, but host programs it starts (yt-dlp, a Python script)
//! break on it, so they get the variables without the image's entries.

fn appdir() -> Option<String> {
    std::env::var("APPDIR").ok().filter(|d| !d.is_empty() && std::env::var_os("APPIMAGE").is_some())
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
    for key in ["PYTHONDONTWRITEBYTECODE", "GTK_THEME"] {
        cmd.env_remove(key);
    }
}
