fn main() {
    // Declaring app commands generates `allow-<command>` permissions, which the
    // remote-origin capability in capabilities/ grants to music.youtube.com.
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(&["player_state"])),
    )
    .expect("failed to run tauri-build");
}
