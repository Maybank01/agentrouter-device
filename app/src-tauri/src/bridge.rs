//! What the cloud page may ask of this app (DEVICE-PROTOCOL.md, "desktop shell"). Deliberately small:
//! the page can read the device's status and start linking. It can never approve anything: linking,
//! access levels and commands are approved only in this app's own windows on this computer.

use agentrouter_device::keystore;
use serde_json::{Value, json};

use crate::ui::ui;

/// The device as the page shows it (no secrets: no key, no token).
#[tauri::command]
pub fn device_status() -> Value {
    let state = ui().state();
    json!({
        "app": "desktop",
        "version": env!("CARGO_PKG_VERSION"),
        "linked": keystore::is_linked(),
        "device": ui().rt.device.device_id(),
        "connection": state["conn"],
        "inUseBy": ui().rt.device.in_use_by(),
        "name": state["name"],
        "access": state["access"],
        "folders": state["folders"],
        "paused": state["paused"],
    })
}

/// Start linking this computer to the signed-in account. The person chooses the name and the level in
/// the pairing window first; then the page gets the code and confirms it with its own session (POST
/// /api/control/v1/personal/devices/pair), and this app picks up the approval.
#[tauri::command]
pub async fn device_link(app: tauri::AppHandle) -> Result<Value, String> {
    if keystore::is_linked() {
        return Err("这台电脑已经链接了。".into());
    }
    tauri::async_runtime::spawn_blocking(move || crate::pair::from_web(&app))
        .await
        .map_err(|e| e.to_string())?
}
