//! The tray: status at a glance (online, offline, in use by a conversation), the access level and
//! folders, pause, disconnect, link and the audit log. Every change that widens what a cloud
//! conversation can do asks first, in a native dialog on this computer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentrouter_device::config::Access;
use agentrouter_device::consent::{TITLE, inform, question};
use agentrouter_device::gate::display;
use agentrouter_device::keystore;
use agentrouter_device::link;
use agentrouter_device::net::{Conn, Runtime};
use agentrouter_device::util::log;
use serde_json::json;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Online,
    InUse,
    Waiting,
    Paused,
    Stopped,
}

fn icon(look: Look) -> Image<'static> {
    let (r, g, b) = match look {
        Look::Online => (0x22, 0xc5, 0x5e),
        Look::InUse => (0x3b, 0x82, 0xf6),
        Look::Waiting => (0x9c, 0xa3, 0xaf),
        Look::Paused => (0xf5, 0x9e, 0x0b),
        Look::Stopped => (0xef, 0x44, 0x44),
    };
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let c = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // A filled dot with a soft edge and a light ring.
            let (pr, pg, pb, a) = if d <= 11.5 {
                (r, g, b, 255.0)
            } else if d <= 14.5 {
                (255, 255, 255, 230.0)
            } else if d <= 15.5 {
                (255, 255, 255, 230.0 * (15.5 - d))
            } else {
                (0, 0, 0, 0.0)
            };
            rgba.extend_from_slice(&[pr, pg, pb, a as u8]);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}

fn short(session: &str) -> String {
    session.chars().take(12).collect()
}

const TRAY: &str = "device";

struct Items {
    status: MenuItem<tauri::Wry>,
    folders: MenuItem<tauri::Wry>,
    levels: Vec<(Access, CheckMenuItem<tauri::Wry>)>,
    pause: MenuItem<tauri::Wry>,
    disconnect: MenuItem<tauri::Wry>,
    link: MenuItem<tauri::Wry>,
}

/// Choose a folder the AI may use (a worker thread only: the dialog blocks).
pub fn pick_folder(app: &AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .set_title("选择 AI 可以使用的文件夹")
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| display(&p))
}

/// Create the tray and keep it current.
pub fn build(app: &AppHandle, rt: Arc<Runtime>) -> tauri::Result<()> {
    let cfg = rt.config.lock().unwrap().clone();
    let items = Arc::new(Items {
        status: MenuItem::with_id(app, "status", TITLE, false, None::<&str>)?,
        folders: MenuItem::with_id(app, "folders", "", false, None::<&str>)?,
        levels: Access::ALL
            .iter()
            .map(|a| {
                CheckMenuItem::with_id(
                    app,
                    format!("access:{}", a.as_str()),
                    a.label(),
                    true,
                    cfg.access == *a,
                    None::<&str>,
                )
                .map(|item| (*a, item))
            })
            .collect::<tauri::Result<_>>()?,
        pause: MenuItem::with_id(app, "pause", "暂停", true, None::<&str>)?,
        disconnect: MenuItem::with_id(
            app,
            "disconnect",
            "断开（停止所有任务）",
            true,
            None::<&str>,
        )?,
        link: MenuItem::with_id(app, "link", "链接这台电脑…", true, None::<&str>)?,
    });
    let access_menu = Submenu::with_id(app, "access", "访问级别", true)?;
    for (_, item) in &items.levels {
        access_menu.append(item)?;
    }
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "open", "打开 AgentRouter", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &items.status,
            &items.folders,
            &PredefinedMenuItem::separator(app)?,
            &access_menu,
            &MenuItem::with_id(app, "add_folder", "添加文件夹…", true, None::<&str>)?,
            &MenuItem::with_id(app, "clear_folders", "清空文件夹", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &items.pause,
            &items.disconnect,
            &items.link,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "audit", "打开审计日志", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?,
        ],
    )?;
    let busy = Arc::new(AtomicBool::new(false));
    let on_rt = rt.clone();
    let tray = TrayIconBuilder::with_id(TRAY)
        .icon(icon(Look::Waiting))
        .tooltip(TITLE)
        .menu(&menu)
        .on_menu_event(move |app, event| on_menu(app, &on_rt, &busy, event.id().as_ref()))
        .build(app)?;

    std::thread::spawn(move || {
        let mut shown: Option<(Look, String, String)> = None;
        while !rt.stop.load(Ordering::SeqCst) {
            refresh(&rt, &tray, &items, &mut shown);
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    Ok(())
}

fn refresh(
    rt: &Runtime,
    tray: &tauri::tray::TrayIcon,
    items: &Items,
    shown: &mut Option<(Look, String, String)>,
) {
    let cfg = rt.config.lock().unwrap().clone();
    let conn = rt.conn();
    let (look, text) = match &conn {
        Conn::Online => match rt.device.in_use_by() {
            Some(session) => (Look::InUse, format!("正在被对话 {} 使用", short(&session))),
            None => (Look::Online, "在线".to_string()),
        },
        Conn::Connecting => (Look::Waiting, "正在连接…".to_string()),
        Conn::Offline(_) => (Look::Waiting, "离线，正在重连".to_string()),
        Conn::Paused => (Look::Paused, "已暂停".to_string()),
        Conn::Disconnected => (Look::Stopped, "已断开".to_string()),
        Conn::NotLinked => (Look::Waiting, "未链接".to_string()),
    };
    let label = format!("{text} · {}", cfg.access.label());
    let folders = if cfg.folders.is_empty() {
        "文件夹：没有".to_string()
    } else {
        format!("文件夹：{}", cfg.folders.join("；"))
    };
    let now = (look, label.clone(), folders.clone());
    if shown.as_ref() != Some(&now) {
        let _ = tray.set_icon(Some(icon(look)));
        let _ = tray.set_tooltip(Some(format!("{TITLE} · {label}")));
        let _ = items.status.set_text(&label);
        let _ = items.folders.set_text(&folders);
        for (access, item) in &items.levels {
            let _ = item.set_checked(cfg.access == *access);
        }
        let _ = items
            .pause
            .set_text(if cfg.paused { "继续" } else { "暂停" });
        let _ = items.disconnect.set_text(if cfg.disconnected {
            "重新连接"
        } else {
            "断开（停止所有任务）"
        });
        let _ = items.link.set_enabled(conn == Conn::NotLinked);
        *shown = Some(now);
    }
    if rt.revoked_notice.swap(false, Ordering::SeqCst) {
        inform(
            TITLE,
            "这台电脑已在网页上断开，AI 起的任务都已停止。要再用，请在托盘里重新链接。",
        );
    }
}

/// Run a dialog flow off the event thread (one at a time).
fn flow(busy: &Arc<AtomicBool>, work: impl FnOnce() + Send + 'static) {
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let busy = busy.clone();
    std::thread::spawn(move || {
        work();
        busy.store(false, Ordering::SeqCst);
    });
}

fn apply(rt: &Runtime, access: Access, folders: Vec<String>) {
    let mut cfg = rt.config.lock().unwrap();
    cfg.access = access;
    cfg.folders = folders;
    if let Err(e) = cfg.save() {
        log(&format!("settings not saved: {e}"));
    }
    rt.device.set_access(cfg.access, &cfg.folders);
    rt.device
        .record(json!({"event": "access", "access": cfg.access.as_str(), "folders": cfg.folders}));
    drop(cfg);
    // The next hello reports the new level to the web.
    rt.reconnect.store(true, Ordering::SeqCst);
}

fn on_menu(app: &AppHandle, rt: &Arc<Runtime>, busy: &Arc<AtomicBool>, id: &str) {
    let rt = rt.clone();
    let app = app.clone();
    if let Some(level) = id.strip_prefix("access:") {
        let Some(access) = Access::parse(level) else {
            return;
        };
        flow(busy, move || {
            let mut folders = rt.config.lock().unwrap().folders.clone();
            if access.needs_folders() && folders.is_empty() {
                match pick_folder(&app) {
                    Some(f) => folders.push(f),
                    None => return,
                }
            }
            if question(
                TITLE,
                &link::change_question(access, &folders),
                &AtomicBool::new(false),
            ) {
                apply(&rt, access, folders);
            }
        });
        return;
    }
    match id {
        "open" => crate::show_main(&app),
        "add_folder" => flow(busy, move || {
            let Some(folder) = pick_folder(&app) else {
                return;
            };
            let (access, mut folders) = {
                let c = rt.config.lock().unwrap();
                (c.access, c.folders.clone())
            };
            if folders.contains(&folder) {
                return;
            }
            folders.push(folder);
            if question(
                TITLE,
                &link::change_question(access, &folders),
                &AtomicBool::new(false),
            ) {
                apply(&rt, access, folders);
            }
        }),
        "clear_folders" => {
            let access = rt.config.lock().unwrap().access;
            apply(&rt, access, Vec::new());
        }
        "pause" => {
            let mut cfg = rt.config.lock().unwrap();
            cfg.paused = !cfg.paused;
            let _ = cfg.save();
            rt.device
                .record(json!({"event": if cfg.paused { "paused" } else { "resumed" }}));
        }
        "disconnect" => {
            let mut cfg = rt.config.lock().unwrap();
            if cfg.disconnected {
                cfg.disconnected = false;
                rt.device.record(json!({"event": "reconnect"}));
            } else {
                cfg.disconnected = true;
                rt.device.stop_all("disconnected from the tray");
            }
            let _ = cfg.save();
        }
        "link" => flow(busy, move || link_flow(&app, &rt)),
        "audit" => open_audit(&rt),
        "quit" => {
            rt.device.stop_all("quit");
            rt.stop.store(true, Ordering::SeqCst);
            app.exit(0);
        }
        _ => {}
    }
}

/// Link from the tray: approve here, then confirm the code in the app window (the cloud page, signed
/// in with the person's own session).
fn link_flow(app: &AppHandle, rt: &Arc<Runtime>) {
    if keystore::is_linked() {
        return;
    }
    let mut cfg = rt.config.lock().unwrap().clone();
    if cfg.access.needs_folders() && cfg.folders.is_empty() {
        match pick_folder(app) {
            Some(f) => cfg.folders.push(f),
            None => return,
        }
    }
    if !question(TITLE, &link::link_question(&cfg), &AtomicBool::new(false)) {
        return;
    }
    cfg.disconnected = false;
    cfg.paused = false;
    let _ = cfg.save();
    rt.device.set_access(cfg.access, &cfg.folders);
    *rt.config.lock().unwrap() = cfg.clone();
    let result = link::link(&cfg, &rt.stop, |pairing| {
        inform(TITLE, &link::code_text(pairing));
        show_verify_page(app, &cfg.web, &pairing.verify_url);
    });
    match result {
        Ok(device) => {
            rt.device.record(json!({"event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders}));
            inform(TITLE, "已链接。你在网页上同意的对话现在可以使用这台电脑。");
        }
        Err(e) => inform(TITLE, &e),
    }
}

/// Open the confirmation page in the app window, only when it is a page of the app's own web origin.
fn show_verify_page(app: &AppHandle, web: &str, verify_url: &str) {
    if !link::safe_url(verify_url) || !same_origin(web, verify_url) {
        return;
    }
    let Ok(url) = verify_url.parse() else { return };
    if let Some(window) = app.get_webview_window(crate::MAIN) {
        let _ = window.navigate(url);
    }
    crate::show_main(app);
}

fn same_origin(a: &str, b: &str) -> bool {
    fn origin(u: &str) -> Option<&str> {
        let rest = u.split_once("://")?.1;
        let host_end = rest.find('/').unwrap_or(rest.len());
        Some(&u[..u.len() - rest.len() + host_end])
    }
    matches!((origin(a), origin(b)), (Some(x), Some(y)) if x.eq_ignore_ascii_case(y))
}

#[cfg(windows)]
fn open_audit(rt: &Runtime) {
    let path = rt.device.audit_path();
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}

#[cfg(not(windows))]
fn open_audit(rt: &Runtime) {
    inform(
        TITLE,
        &format!("审计日志在 {}", rt.device.audit_path().display()),
    );
}

#[cfg(test)]
mod tests {
    use super::same_origin;

    #[test]
    fn verify_page_must_be_the_app_origin() {
        assert!(same_origin(
            "https://a.example",
            "https://a.example/settings/devices?code=1"
        ));
        assert!(!same_origin(
            "https://a.example",
            "https://a.example.evil/x"
        ));
        assert!(!same_origin("https://a.example", "http://a.example/x"));
    }
}
