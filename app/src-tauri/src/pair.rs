//! The pairing window (03 樱粉): ① name and what the AI may do here (four levels, a vertical list,
//! "只限这些文件夹" by default with its folders), ② 正在连上 (three steps), ③ 连上了, or 没连上
//! (network / expired code, one sentence and one button). The same window in `level` mode changes the
//! access level later (from the tray): choosing here is the local confirmation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentrouter_device::config::Access;
use agentrouter_device::keystore;
use agentrouter_device::net::{self, Conn};
use agentrouter_device::protocol::DeviceKey;
use agentrouter_device::share::display_code;
use agentrouter_device::util::log;
use serde::Serialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::ui::{APP, ui};

pub const PAIR: &str = "pair";

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairState {
    /// `link` or `level`.
    pub mode: String,
    /// `choose`, `linking`, `done` or `error`.
    pub stage: String,
    pub name: String,
    pub access: String,
    pub folders: Vec<String>,
    /// The pairing code, `123 456`-style, while linking.
    pub code: String,
    /// Steps done while linking: 1 账号确认好了, 2 和云端连上, 3 托盘准备好.
    pub step: u8,
    /// `network`, `expired` or `other`.
    pub error: String,
    pub message: String,
    /// The cloud page asked for this pairing and waits for the code.
    pub from_web: bool,
}

/// The cloud page waiting for the pairing code (bridge `device_link`).
static WEB_WAITER: Mutex<Option<Sender<Result<Value, String>>>> = Mutex::new(None);
/// Set when the person cancels or closes the window while linking.
static CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

fn reply_web(answer: Result<Value, String>) {
    if let Some(tx) = WEB_WAITER.lock().unwrap().take() {
        let _ = tx.send(answer);
    }
}

/// Open (or bring forward) the pairing window. Call from a worker thread, never the event loop.
pub fn open(app: &AppHandle, mode: &str, from_web: bool) {
    let cfg = ui().rt.config.lock().unwrap().clone();
    {
        let mut p = ui().pair.lock().unwrap();
        let busy = p.stage == "linking" && app.get_webview_window(PAIR).is_some();
        if !busy {
            let access =
                if mode == "link" && cfg.access == Access::Readonly && cfg.folders.is_empty() {
                    // A new link starts from "只限这些文件夹" (the owner's draft); the person picks.
                    Access::Folders
                } else {
                    cfg.access
                };
            *p = PairState {
                mode: mode.to_string(),
                stage: "choose".into(),
                name: cfg.name.clone(),
                access: access.as_str().into(),
                folders: cfg.folders.clone(),
                from_web,
                ..Default::default()
            };
        }
    }
    if let Some(w) = app.get_webview_window(PAIR) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, PAIR, WebviewUrl::App("pair.html".into()))
        .title(if mode == "level" {
            "AgentRouter · 访问级别"
        } else {
            "AgentRouter · 链接这台电脑"
        })
        .inner_size(540.0, 640.0)
        .resizable(false)
        .maximizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .center()
        .focused(true)
        .build();
    if let Err(e) = built {
        log(&format!("the pairing window could not open: {e}"));
    }
}

pub fn close(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(PAIR) {
        let _ = w.close();
    }
}

/// Cancel linking (if running), tell a waiting cloud page, close the window.
pub fn cancel(app: &AppHandle) {
    if let Some(flag) = CANCEL.lock().unwrap().take() {
        flag.store(true, Ordering::SeqCst);
    }
    reply_web(Err("你在这台电脑上取消了链接。".into()));
    close(app);
}

#[tauri::command]
pub fn pair_get() -> Value {
    let p = ui().pair.lock().unwrap().clone();
    let mut v = serde_json::to_value(&p).unwrap_or_default();
    v["levels"] = json!(
        Access::ALL
            .iter()
            .map(|a| json!({"id": a.as_str(), "label": a.label()}))
            .collect::<Vec<_>>()
    );
    v
}

/// Choose a folder (the system folder picker).
#[tauri::command]
pub async fn pair_pick_folder(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || crate::tray::pick_folder(&app))
        .await
        .ok()
        .flatten()
}

/// The person's choice: apply the level (level mode) or start linking (link mode).
#[tauri::command]
pub fn pair_submit(name: String, access: String, folders: Vec<String>) -> Result<(), String> {
    let access = Access::parse(&access).ok_or("unknown level")?;
    if access.needs_folders() && folders.is_empty() {
        return Err("先加一个文件夹。".into());
    }
    let folders: Vec<String> = folders
        .into_iter()
        .filter_map(|f| std::fs::canonicalize(&f).ok())
        .map(|p| agentrouter_device::gate::display(&p))
        .collect();
    let ui = ui();
    let mode = ui.pair.lock().unwrap().mode.clone();
    let name = name.trim().chars().take(40).collect::<String>();
    {
        let mut cfg = ui.rt.config.lock().unwrap();
        if !name.is_empty() {
            cfg.name = name.clone();
        }
        cfg.access = access;
        cfg.folders = folders.clone();
        if mode == "link" {
            cfg.disconnected = false;
            cfg.paused = false;
        }
        cfg.save().map_err(|e| format!("设置没有保存：{e}"))?;
        ui.rt.device.set_access(cfg.access, &cfg.folders);
        ui.rt.device.record(
            json!({"event": "access", "access": cfg.access.as_str(), "folders": cfg.folders}),
        );
    }
    ui.rt.reconnect.store(true, Ordering::SeqCst);
    let app = APP.get().ok_or("not ready")?;
    if mode == "level" {
        close(app);
        return Ok(());
    }
    {
        let mut p = ui.pair.lock().unwrap();
        p.name = name;
        p.access = access.as_str().into();
        p.folders = folders;
    }
    start(app);
    Ok(())
}

/// Start (or restart: "再试一次", "换一个码") linking.
pub fn start(app: &AppHandle) {
    if keystore::is_linked() {
        let mut p = ui().pair.lock().unwrap();
        p.stage = "done".into();
        p.step = 3;
        return;
    }
    {
        let mut p = ui().pair.lock().unwrap();
        p.stage = "linking".into();
        p.step = 0;
        p.code.clear();
        p.error.clear();
        p.message.clear();
    }
    let flag = Arc::new(AtomicBool::new(false));
    if let Some(old) = CANCEL.lock().unwrap().replace(flag.clone()) {
        old.store(true, Ordering::SeqCst);
    }
    let app = app.clone();
    std::thread::spawn(move || run(&app, &flag));
}

fn fail(kind: &str, message: &str) {
    let mut p = ui().pair.lock().unwrap();
    p.stage = "error".into();
    p.error = kind.into();
    p.message = message.into();
}

fn run(app: &AppHandle, cancel: &AtomicBool) {
    let ui = ui();
    let cfg = ui.rt.config.lock().unwrap().clone();
    let key = DeviceKey::generate();
    let pairing = match net::start_pairing(&cfg.gateway, &key, &cfg.name) {
        Ok(p) => p,
        Err(e) => {
            log(&format!("pairing could not start: {e}"));
            if e.status == 0 || e.status >= 500 {
                fail("network", "这台电脑连不上云端。看看网络，再试一次。");
            } else {
                fail("other", "云端现在不能链接新电脑，稍后再试一次。");
            }
            reply_web(Err("链接没有成功。".into()));
            return;
        }
    };
    ui.pair.lock().unwrap().code = display_code(&pairing.code);
    let from_web = ui.pair.lock().unwrap().from_web;
    if from_web {
        // The cloud page confirms the code with the person's own session.
        reply_web(Ok(
            json!({"code": pairing.code, "expiresIn": pairing.expires_in}),
        ));
    } else {
        crate::open_in_web(app, &pairing.verify_url);
    }
    match net::wait_for_approval(&cfg.gateway, &key, &pairing, cancel) {
        Ok(device) => {
            ui.rt.device.record(json!({"event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders}));
            ui.pair.lock().unwrap().step = 1;
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && !cancel.load(Ordering::SeqCst) {
                if ui.rt.conn() == Conn::Online {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            if ui.rt.conn() != Conn::Online {
                fail("network", "这台电脑连不上云端。看看网络，再试一次。");
                return;
            }
            ui.pair.lock().unwrap().step = 2;
            std::thread::sleep(Duration::from_millis(400));
            let mut p = ui.pair.lock().unwrap();
            p.step = 3;
            p.stage = "done".into();
        }
        Err(_) if cancel.load(Ordering::SeqCst) => {}
        Err(message) if message.contains("过期") || message.contains("失效") => fail(
            "expired",
            "配对码 10 分钟内有效。换一个新的，再填到网页上。",
        ),
        Err(message) => fail("other", &message),
    }
}

/// The cloud page's "link this computer": open the window, wait for the person, give the page the code.
pub fn from_web(app: &AppHandle) -> Result<Value, String> {
    let (tx, rx) = channel();
    if let Some(old) = WEB_WAITER.lock().unwrap().replace(tx) {
        let _ = old.send(Err("又开始了一次链接。".into()));
    }
    // A worker thread (the caller runs in spawn_blocking): WebView2 windows are not built on the
    // event-loop thread.
    open(app, "link", true);
    rx.recv_timeout(Duration::from_secs(15 * 60))
        .unwrap_or_else(|_| Err("等太久了，链接取消了。".into()))
}
