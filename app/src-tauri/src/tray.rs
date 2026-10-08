//! The Windows tray: status at a glance (online, offline, in use by a conversation), the access level
//! and folders, pause, disconnect, link and the audit log. Every change that widens what a
//! cloud conversation can do asks first, in a native dialog on this computer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PostQuitMessage, SetTimer, TranslateMessage, WM_TIMER,
};

use crate::config::Access;
use crate::consent::{TITLE, inform, question, wide};
use crate::gate::display;
use crate::link;
use crate::net::{Conn, Runtime};
use crate::util::log;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Look {
    Online,
    InUse,
    Waiting,
    Paused,
    Stopped,
}

fn icon(look: Look) -> Icon {
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
    Icon::from_rgba(rgba, SIZE, SIZE).expect("a valid icon")
}

fn short(session: &str) -> String {
    session.chars().take(12).collect()
}

struct Items {
    status: MenuItem,
    folders: MenuItem,
    levels: Vec<(Access, CheckMenuItem)>,
    add_folder: MenuItem,
    clear_folders: MenuItem,
    pause: MenuItem,
    disconnect: MenuItem,
    link: MenuItem,
    audit: MenuItem,
    quit: MenuItem,
}

/// Run the tray on this thread until the person quits.
pub fn run(rt: Arc<Runtime>) {
    let cfg = rt.config.lock().unwrap().clone();
    let items = Items {
        status: MenuItem::new("AgentRouter 设备", false, None),
        folders: MenuItem::new("", false, None),
        levels: Access::ALL
            .iter()
            .map(|a| {
                (
                    *a,
                    CheckMenuItem::new(a.label(), true, cfg.access == *a, None),
                )
            })
            .collect(),
        add_folder: MenuItem::new("添加文件夹…", true, None),
        clear_folders: MenuItem::new("清空文件夹", true, None),
        pause: MenuItem::new("暂停", true, None),
        disconnect: MenuItem::new("断开（停止所有任务）", true, None),
        link: MenuItem::new("链接这台电脑…", true, None),
        audit: MenuItem::new("打开审计日志", true, None),
        quit: MenuItem::new("退出", true, None),
    };
    let access_menu = Submenu::new("访问级别", true);
    for (_, item) in &items.levels {
        let _ = access_menu.append(item);
    }
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &items.status,
        &items.folders,
        &PredefinedMenuItem::separator(),
        &access_menu,
        &items.add_folder,
        &items.clear_folders,
        &PredefinedMenuItem::separator(),
        &items.pause,
        &items.disconnect,
        &items.link,
        &PredefinedMenuItem::separator(),
        &items.audit,
        &PredefinedMenuItem::separator(),
        &items.quit,
    ]);
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(TITLE)
        .with_icon(icon(Look::Waiting))
        .build()
    {
        Ok(tray) => tray,
        Err(e) => {
            log(&format!("the tray icon could not be created: {e}"));
            return;
        }
    };
    let busy = Arc::new(AtomicBool::new(false));
    let mut shown: Option<(Look, String)> = None;
    refresh(&rt, &tray, &items, &mut shown);
    // SAFETY: a thread timer (no window) that posts WM_TIMER to this thread's queue every 500 ms.
    unsafe {
        SetTimer(std::ptr::null_mut(), 0, 500, None);
    }
    // SAFETY: the standard message loop on the thread that owns the tray icon.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let got = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if got <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            on_menu(&rt, &items, &busy, event.id);
        }
        while TrayIconEvent::receiver().try_recv().is_ok() {}
        if msg.message == WM_TIMER {
            refresh(&rt, &tray, &items, &mut shown);
        }
        if rt.stop.load(Ordering::SeqCst) {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
    }
}

fn refresh(
    rt: &Runtime,
    tray: &tray_icon::TrayIcon,
    items: &Items,
    shown: &mut Option<(Look, String)>,
) {
    let cfg = rt.config.lock().unwrap().clone();
    let (look, text) = match rt.conn() {
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
    if shown.as_ref() != Some(&(look, label.clone())) {
        let _ = tray.set_icon(Some(icon(look)));
        let _ = tray.set_tooltip(Some(format!("{TITLE} · {label}")));
        items.status.set_text(&label);
        *shown = Some((look, label));
    }
    items.folders.set_text(if cfg.folders.is_empty() {
        "文件夹：没有".to_string()
    } else {
        format!("文件夹：{}", cfg.folders.join("；"))
    });
    for (access, item) in &items.levels {
        item.set_checked(cfg.access == *access);
    }
    items
        .pause
        .set_text(if cfg.paused { "继续" } else { "暂停" });
    items.disconnect.set_text(if cfg.disconnected {
        "重新连接"
    } else {
        "断开（停止所有任务）"
    });
    items.link.set_enabled(rt.conn() == Conn::NotLinked);
    if rt.revoked_notice.swap(false, Ordering::SeqCst) {
        inform(
            TITLE,
            "这台电脑已在网页上断开，AI 起的任务都已停止。要再用，请在托盘里重新链接。",
        );
    }
}

/// Run a dialog flow off the tray thread (one at a time).
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

fn pick_folder() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择 AI 可以使用的文件夹")
        .pick_folder()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| display(&p))
}

fn apply(rt: &Runtime, access: Access, folders: Vec<String>) {
    let mut cfg = rt.config.lock().unwrap();
    cfg.access = access;
    cfg.folders = folders;
    if let Err(e) = cfg.save() {
        log(&format!("settings not saved: {e}"));
    }
    rt.device.set_access(cfg.access, &cfg.folders);
    rt.device.record(serde_json::json!({"event": "access", "access": cfg.access.as_str(), "folders": cfg.folders}));
    drop(cfg);
    // The next hello reports the new level to the web.
    rt.reconnect.store(true, Ordering::SeqCst);
}

fn on_menu(rt: &Arc<Runtime>, items: &Items, busy: &Arc<AtomicBool>, id: MenuId) {
    let rt = rt.clone();
    if let Some((access, _)) = items.levels.iter().find(|(_, item)| *item.id() == id) {
        let access = *access;
        flow(busy, move || {
            let mut folders = rt.config.lock().unwrap().folders.clone();
            if access.needs_folders() && folders.is_empty() {
                match pick_folder() {
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
    } else if id == *items.add_folder.id() {
        flow(busy, move || {
            let Some(folder) = pick_folder() else { return };
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
        });
    } else if id == *items.clear_folders.id() {
        let access = rt.config.lock().unwrap().access;
        apply(&rt, access, Vec::new());
    } else if id == *items.pause.id() {
        let mut cfg = rt.config.lock().unwrap();
        cfg.paused = !cfg.paused;
        let _ = cfg.save();
        rt.device
            .record(serde_json::json!({"event": if cfg.paused { "paused" } else { "resumed" }}));
    } else if id == *items.disconnect.id() {
        let mut cfg = rt.config.lock().unwrap();
        if cfg.disconnected {
            cfg.disconnected = false;
            rt.device.record(serde_json::json!({"event": "reconnect"}));
        } else {
            cfg.disconnected = true;
            rt.device.stop_all("disconnected from the tray");
        }
        let _ = cfg.save();
    } else if id == *items.link.id() {
        flow(busy, move || link_flow(&rt));
    } else if id == *items.audit.id() {
        let path = rt.device.audit_path();
        let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
    } else if id == *items.quit.id() {
        rt.device.stop_all("quit");
        rt.stop.store(true, Ordering::SeqCst);
    }
}

fn link_flow(rt: &Arc<Runtime>) {
    if crate::keystore::is_linked() {
        return;
    }
    let mut cfg = rt.config.lock().unwrap().clone();
    if cfg.access.needs_folders() && cfg.folders.is_empty() {
        match pick_folder() {
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
        if link::safe_url(&pairing.verify_url) {
            open_url(&pairing.verify_url);
        }
    });
    match result {
        Ok(device) => {
            rt.device.record(serde_json::json!({"event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders}));
            inform(TITLE, "已链接。你在网页上同意的对话现在可以使用这台电脑。");
        }
        Err(e) => inform(TITLE, &e),
    }
}

fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, target) = (wide("open"), wide(url));
    // SAFETY: NUL-terminated strings; the URL was checked to be a web page.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}
