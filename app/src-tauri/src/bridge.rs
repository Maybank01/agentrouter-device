//! What the cloud page may ask of this app (DEVICE-PROTOCOL.md, "desktop shell"). Deliberately small:
//! the page can read the device's status and start linking. It can never approve anything: linking,
//! access levels and commands are approved only in native dialogs on this computer.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agentrouter_device::consent::{TITLE, question};
use agentrouter_device::keystore;
use agentrouter_device::link;
use agentrouter_device::net::{self, Conn, Runtime};
use serde_json::{Value, json};
use tauri::State;

fn conn_name(conn: &Conn) -> &'static str {
    match conn {
        Conn::NotLinked => "not_linked",
        Conn::Paused => "paused",
        Conn::Disconnected => "disconnected",
        Conn::Connecting => "connecting",
        Conn::Online => "online",
        Conn::Offline(_) => "offline",
    }
}

/// The device as the page shows it (no secrets: no key, no token).
#[tauri::command]
pub fn device_status(rt: State<'_, Arc<Runtime>>) -> Value {
    let cfg = rt.config.lock().unwrap().clone();
    json!({
        "app": "desktop",
        "version": env!("CARGO_PKG_VERSION"),
        "linked": keystore::is_linked(),
        "device": rt.device.device_id(),
        "connection": conn_name(&rt.conn()),
        "inUseBy": rt.device.in_use_by(),
        "name": cfg.name,
        "access": cfg.access.as_str(),
        "folders": cfg.folders,
        "paused": cfg.paused,
    })
}

/// Start linking this computer to the signed-in account. The person approves it in a native dialog
/// first; then the page gets the code and confirms it with its own session (POST
/// /api/control/v1/personal/devices/pair), and this app picks up the approval.
#[tauri::command]
pub async fn device_link(
    app: tauri::AppHandle,
    rt: State<'_, Arc<Runtime>>,
) -> Result<Value, String> {
    if keystore::is_linked() {
        return Err("这台电脑已经链接了。".into());
    }
    let rt = rt.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut cfg = rt.config.lock().unwrap().clone();
        if cfg.access.needs_folders() && cfg.folders.is_empty() {
            match crate::tray::pick_folder(&app) {
                Some(folder) => cfg.folders.push(folder),
                None => return Err("没有选文件夹。".to_string()),
            }
        }
        if !question(TITLE, &link::link_question(&cfg), &AtomicBool::new(false)) {
            return Err("你在这台电脑上取消了链接。".to_string());
        }
        cfg.paused = false;
        cfg.disconnected = false;
        cfg.save().map_err(|e| format!("设置没有保存：{e}"))?;
        rt.device.set_access(cfg.access, &cfg.folders);
        *rt.config.lock().unwrap() = cfg.clone();
        let key = agentrouter_device::protocol::DeviceKey::generate();
        let pairing = net::start_pairing(&cfg.gateway, &key, &cfg.name).map_err(|e| format!("链接没有成功（{e}）"))?;
        let answer = json!({"code": pairing.code, "expiresIn": pairing.expires_in});
        std::thread::spawn(move || match net::wait_for_approval(&cfg.gateway, &key, &pairing, &rt.stop) {
            Ok(device) => rt.device.record(json!({"event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders})),
            Err(e) => agentrouter_device::util::log(&format!("linking did not finish: {e}")),
        });
        Ok(answer)
    })
    .await
    .map_err(|e| e.to_string())?
}
