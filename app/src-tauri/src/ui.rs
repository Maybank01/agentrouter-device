//! The app's own pages (the 电脑 panel, pairing, consent, the "being controlled" bar, the tray popup)
//! and the state they show. Pages are local (`app/ui/*.html`), poll [`ui_state`] and call
//! [`ui_action`]; none of these commands is granted to the cloud page (see `main.rs`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

use agentrouter_device::config::Access;
use agentrouter_device::consent::Decision;
use agentrouter_device::keystore;
use agentrouter_device::mcp::{self, LocalHub};
use agentrouter_device::net::{Conn, Runtime};
use agentrouter_device::share::{self, ShareService, ShareTerms, ShareView};
use agentrouter_device::util::now_ms;
use serde_json::{Value, json};
use tauri::{AppHandle, LogicalSize, Webview};

pub static APP: OnceLock<AppHandle> = OnceLock::new();
static UI: OnceLock<Arc<Ui>> = OnceLock::new();

pub fn ui() -> &'static Arc<Ui> {
    UI.get()
        .expect("ui state is installed before any window opens")
}

pub fn install(ui: Arc<Ui>) {
    let _ = UI.set(ui);
}

/// A consent dialog waiting for the person.
pub struct Pending {
    pub view: Value,
    pub tx: Sender<Decision>,
}

#[derive(Default)]
pub struct ShareUi {
    pub view: Option<ShareView>,
    pub message: String,
    /// The anti-fraud question is open.
    pub fraud: bool,
}

#[derive(Default)]
pub struct ConnectUi {
    pub message: String,
    pub busy: bool,
}

pub struct Ui {
    pub rt: Arc<Runtime>,
    pub hub: Arc<LocalHub>,
    pub shares: Box<dyn ShareService>,
    pub consents: Mutex<HashMap<String, Pending>>,
    pub pair: Mutex<crate::pair::PairState>,
    pub share: Mutex<ShareUi>,
    pub connect: Mutex<ConnectUi>,
    pub panel_open: AtomicBool,
    /// Which part of the panel the tray asked for ("share", "connect"), shown once.
    pub focus: Mutex<String>,
    pub bar: Mutex<crate::bar::BarUi>,
    /// The bar could not be shown the last time it was needed.
    pub indicator_failed: AtomicBool,
    /// Smoke test only: pretend the bar cannot be shown.
    pub break_indicator: AtomicBool,
    /// Smoke test only: values laid over the real state (a linked, online device…).
    pub fake: Mutex<Value>,
}

impl Ui {
    pub fn new(rt: Arc<Runtime>, hub: Arc<LocalHub>, shares: Box<dyn ShareService>) -> Ui {
        Ui {
            rt,
            hub,
            shares,
            consents: Mutex::new(HashMap::new()),
            pair: Mutex::new(Default::default()),
            share: Mutex::new(Default::default()),
            connect: Mutex::new(Default::default()),
            panel_open: AtomicBool::new(true),
            focus: Mutex::new(String::new()),
            bar: Mutex::new(Default::default()),
            indicator_failed: AtomicBool::new(false),
            break_indicator: AtomicBool::new(false),
            fake: Mutex::new(json!({})),
        }
    }

    fn conn_name(&self) -> &'static str {
        match self.rt.conn() {
            Conn::NotLinked => "not_linked",
            Conn::Paused => "paused",
            Conn::Disconnected => "disconnected",
            Conn::Connecting => "connecting",
            Conn::Online => "online",
            Conn::Offline(_) => "offline",
        }
    }

    /// Everything the local pages show, in one object.
    pub fn state(&self) -> Value {
        let cfg = self.rt.config.lock().unwrap().clone();
        let device = &self.rt.device;
        let sessions: Vec<Value> = device
            .presence
            .snapshot(&device.jobs.running_sessions())
            .into_iter()
            .map(|s| {
                let mut v = serde_json::to_value(&s).unwrap_or_default();
                v["who"]["title"] = json!(s.who.title());
                v
            })
            .collect();
        let conn = self.conn_name();
        let exe = mcp::connector_path();
        let share = self.share.lock().unwrap();
        let connect = self.connect.lock().unwrap();
        let mut state = json!({
            "now": now_ms(),
            "linked": keystore::is_linked(),
            "conn": conn,
            "name": cfg.name,
            "access": cfg.access.as_str(),
            "accessLabel": cfg.access.label(),
            "folders": cfg.folders,
            "paused": device.presence.paused(),
            "disconnected": cfg.disconnected,
            "web": cfg.web,
            "sessions": sessions,
            "glowing": device.presence.glowing(device.jobs.running()),
            "local": self.hub.clients(),
            "prompt": mcp::setup_prompt(&exe),
            "snippets": mcp::setup_snippets(&exe),
            "share": {
                "on": share.view.is_some(),
                "code": share.view.as_ref().map(|v| v.code.clone()),
                "password": share.view.as_ref().map(|v| v.password.clone()),
                "message": share.message,
                "fraud": share.fraud,
            },
            "connect": {"message": connect.message, "busy": connect.busy},
            "panelOpen": self.panel_open.load(Ordering::SeqCst),
            "focus": std::mem::take(&mut *self.focus.lock().unwrap()),
            "bar": self.bar.lock().unwrap().view(),
            "indicatorFailed": self.indicator_failed.load(Ordering::SeqCst),
        });
        if let (Some(target), Some(fake)) =
            (state.as_object_mut(), self.fake.lock().unwrap().as_object())
        {
            for (k, v) in fake {
                target.insert(k.clone(), v.clone());
            }
        }
        if state.get("look").is_none() {
            let conn = state["conn"].as_str().unwrap_or("");
            let working = state["sessions"].as_array().is_some_and(|s| !s.is_empty());
            let look = if conn == "not_linked" {
                "unlinked"
            } else if state["disconnected"] == true || conn == "disconnected" {
                "disconnected"
            } else if working {
                "working"
            } else if conn == "online" {
                "idle"
            } else {
                "reconnecting"
            };
            state["look"] = json!(look);
        }
        state
    }
}

#[tauri::command]
pub fn ui_state() -> Value {
    ui().state()
}

fn app() -> &'static AppHandle {
    APP.get().expect("app handle")
}

/// What the local pages can do. Every widening of access goes through the pairing window (the
/// person's own choice on this computer); the rest only narrows or shows things.
#[tauri::command]
pub async fn ui_action(action: String, arg: Option<Value>) -> Result<Value, String> {
    let arg = arg.unwrap_or(Value::Null);
    tauri::async_runtime::spawn_blocking(move || act(&action, &arg))
        .await
        .map_err(|e| e.to_string())?
}

pub fn act(action: &str, arg: &Value) -> Result<Value, String> {
    let ui = ui();
    let rt = &ui.rt;
    let app = app();
    match action {
        "open_main" => crate::show_main(app),
        "open_pair" => {
            if keystore::is_linked() {
                crate::show_main(app);
            } else {
                crate::pair::open(app, "link", false);
            }
        }
        "open_level" => crate::pair::open(app, "level", false),
        "open_share" | "open_connect" => {
            ui.panel_open.store(true, Ordering::SeqCst);
            *ui.focus.lock().unwrap() = action.trim_start_matches("open_").to_string();
            crate::layout_main(app);
            crate::show_main(app);
        }
        "stop_all" => rt.device.stop_all("stopped from the tray"),
        "disconnect" => {
            let mut cfg = rt.config.lock().unwrap();
            cfg.disconnected = true;
            let _ = cfg.save();
            drop(cfg);
            rt.device.stop_all("disconnected from the tray");
        }
        "reconnect" => {
            if keystore::is_linked() {
                let mut cfg = rt.config.lock().unwrap();
                cfg.disconnected = false;
                cfg.paused = false;
                let _ = cfg.save();
                rt.device.record(json!({"event": "reconnect"}));
            } else {
                crate::pair::open(app, "link", false);
            }
        }
        "quit" => {
            rt.device.stop_all("quit");
            rt.stop.store(true, Ordering::SeqCst);
            app.exit(0);
        }
        "audit" => crate::tray::open_audit(rt),
        "pause" => rt.device.set_paused(arg.as_bool().unwrap_or(true)),
        "end_session" => {
            if let Some(session) = arg.as_str() {
                rt.device.end_session(session, "disconnected on the bar");
            }
        }
        "end_all" => {
            for s in rt.device.presence.snapshot(&[]) {
                rt.device.end_session(&s.session, "disconnected on the bar");
            }
        }
        "panel" => {
            ui.panel_open
                .store(arg.as_bool().unwrap_or(true), Ordering::SeqCst);
            crate::layout_main(app);
        }
        "share_on" => {
            let mut s = ui.share.lock().unwrap();
            s.fraud = true;
            s.message.clear();
        }
        "share_cancel" => ui.share.lock().unwrap().fraud = false,
        "share_confirm" => {
            ui.share.lock().unwrap().fraud = false;
            let cfg = rt.config.lock().unwrap().clone();
            let terms = ShareTerms {
                share: share::new_share_id(),
                access: if cfg.access == Access::Full {
                    "confirm".into()
                } else {
                    cfg.access.as_str().into()
                },
                folders: cfg.folders.clone(),
                expires_at: now_ms() + 3_600_000,
                approval: "every_session".into(),
                modes: vec!["remote".into()],
                delegate_cap_points: 0,
            };
            let answer = ui.shares.open(&terms);
            let mut s = ui.share.lock().unwrap();
            match answer {
                Ok(view) => {
                    rt.device.record(json!({"event": "share_start", "share": view.share, "access": view.access, "expiresAt": view.expires_at}));
                    s.view = Some(view);
                    s.message.clear();
                }
                Err(e) => s.message = e.message,
            }
        }
        "share_off" => {
            let view = ui.share.lock().unwrap().view.take();
            if let Some(view) = view {
                let _ = ui.shares.stop(&view.share, "person");
                rt.device.record(
                    json!({"event": "share_stop", "share": view.share, "reason": "person"}),
                );
            }
            ui.share.lock().unwrap().message.clear();
        }
        "share_rotate" => {
            let share = ui
                .share
                .lock()
                .unwrap()
                .view
                .as_ref()
                .map(|v| v.share.clone());
            if let Some(share) = share {
                let answer = ui.shares.rotate(&share);
                let mut s = ui.share.lock().unwrap();
                match answer {
                    Ok(view) => s.view = Some(view),
                    Err(e) => s.message = e.message,
                }
            }
        }
        "connect" => {
            let code = arg["code"].as_str().unwrap_or("");
            let password = arg["password"].as_str().unwrap_or("");
            ui.connect.lock().unwrap().busy = true;
            let answer = ui.shares.connect(code, password);
            let mut c = ui.connect.lock().unwrap();
            c.busy = false;
            c.message = match answer {
                Ok(_) => String::new(),
                Err(e) => e.message,
            };
        }
        "pair_retry" => crate::pair::start(app),
        "pair_cancel" => crate::pair::cancel(app),
        "pair_devices" => {
            let web = rt.config.lock().unwrap().web.clone();
            crate::open_in_web(
                app,
                &format!("{}/settings/devices", web.trim_end_matches('/')),
            );
            crate::pair::close(app);
        }
        "pair_chat" => {
            crate::show_main(app);
            crate::pair::close(app);
        }
        other => return Err(format!("unknown action {other}")),
    }
    Ok(json!({"ok": true}))
}

/// Size a local window to its content (CSS pixels).
#[tauri::command]
pub fn win_fit(webview: Webview, width: f64, height: f64) {
    let window = webview.window();
    let label = window.label().to_string();
    let _ = window.set_size(LogicalSize::new(
        width.clamp(120.0, 900.0),
        height.clamp(32.0, 900.0),
    ));
    match label.as_str() {
        "traypop" => crate::tray::place(&window),
        "bar" => crate::bar::place(&window),
        l if l.starts_with("consent-") => {
            let _ = window.center();
        }
        _ => {}
    }
}

/// The custom title bar's ×: a consent window counts it as "deny", the tray popup hides.
#[tauri::command]
pub fn win_close(webview: Webview) {
    let window = webview.window();
    let label = window.label().to_string();
    if label.starts_with("consent-") {
        crate::consent_ui::answer(&label, Decision::Deny);
    } else if label == "traypop" {
        let _ = window.hide();
    } else if label == "pair" {
        crate::pair::cancel(app());
    } else if label != "bar" && !label.starts_with("frame-") {
        let _ = window.close();
    }
}

#[tauri::command]
pub fn win_minimize(webview: Webview) {
    let window = webview.window();
    if window.label() == "pair" {
        let _ = window.minimize();
    }
}

#[tauri::command]
pub fn win_drag(webview: Webview) {
    let window = webview.window();
    if window.label() != "bar" {
        let _ = window.start_dragging();
    }
}
