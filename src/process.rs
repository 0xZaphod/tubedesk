//! Starting helper programs (yt-dlp, and the deno it runs) the same way on
//! every platform.

/// Prepares `cmd` for a host program started from the app.
pub fn host_command(cmd: &mut tokio::process::Command) {
    crate::appimage::host_env(cmd);
    #[cfg(windows)]
    {
        // A GUI app's console child would otherwise open a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(target_os = "macos")]
    {
        // Apps started from Finder get a minimal PATH without Homebrew's
        // directories, where yt-dlp and deno usually are.
        let mut dirs: Vec<std::path::PathBuf> =
            std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
            if !dirs.iter().any(|d| d.as_os_str() == extra) {
                dirs.push(extra.into());
            }
        }
        if let Ok(path) = std::env::join_paths(dirs) {
            cmd.env("PATH", path);
        }
    }
}
