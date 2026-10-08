fn main() {
    // Every command this app has. Permissions are granted at run time (src/main.rs): the cloud page
    // (remote, configured web origin only) gets `device_status` and `device_link`; the app's own local
    // pages (panel, pairing, consent, bar, tray) get the rest. Nothing is granted by default.
    let manifest = tauri_build::AppManifest::new().commands(&[
        "device_status",
        "device_link",
        "ui_state",
        "ui_action",
        "pair_get",
        "pair_submit",
        "pair_pick_folder",
        "consent_get",
        "consent_answer",
        "bar_drag_start",
        "bar_drag",
        "win_fit",
        "win_close",
        "win_minimize",
        "win_drag",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("tauri build step");
}
