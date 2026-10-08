fn main() {
    // The only commands the cloud page can call (see src/bridge.rs); permissions are granted at run
    // time to the configured web origin only.
    let manifest = tauri_build::AppManifest::new().commands(&["device_status", "device_link"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri build step");
}
