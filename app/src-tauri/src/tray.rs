//! The tray (03 樱粉): the brand mark with a status dot (green idle, pink working, amber reconnecting,
//! grey disconnected), and on click a Windows 11-proportioned popup (`traypop.html`): device name and
//! state, who is doing what, then 打开 AgentRouter, 看记录, 停下所有任务, 分享这台电脑, 连接别人的电脑,
//! 访问级别, 断开, 退出; only "已断开" shows the filled 重新链接 button.

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agentrouter_device::consent::{TITLE, inform};
use agentrouter_device::gate::display;
use agentrouter_device::net::Runtime;
use tauri::image::Image;
use tauri::tray::{MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, PhysicalPosition, Rect, WebviewUrl, WebviewWindowBuilder, Window};
use tauri_plugin_dialog::DialogExt;

use crate::ui::ui;

pub const TRAY: &str = "device";
pub const POPUP: &str = "traypop";

/// The tray icon's rectangle at the last click (physical pixels), to place the popup next to it.
static ANCHOR: Mutex<Option<(f64, f64, f64, f64)>> = Mutex::new(None);

/// The brand mark (pink rounded square with a white diamond) and a status dot, 32×32.
pub fn icon(look: &str) -> Image<'static> {
    const SIZE: usize = 32;
    let pink = [0xe0u8, 0x48, 0x7e];
    let dot = match look {
        "idle" => Some([0x22u8, 0xc5, 0x5e]),
        "working" => Some(pink),
        "reconnecting" => Some([0xf5, 0x9e, 0x0b]),
        _ => Some([0x9c, 0xa3, 0xaf]),
    };
    let dim = matches!(look, "disconnected" | "unlinked");
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let mut put = |x: usize, y: usize, c: [u8; 3], a: f32| {
        let i = (y * SIZE + x) * 4;
        let a0 = rgba[i + 3] as f32 / 255.0;
        let out = a + a0 * (1.0 - a);
        if out <= 0.0 {
            return;
        }
        for k in 0..3 {
            rgba[i + k] =
                ((c[k] as f32 * a + rgba[i + k] as f32 * a0 * (1.0 - a)) / out).round() as u8;
        }
        rgba[i + 3] = (out * 255.0).round() as u8;
    };
    // Rounded square 2..28 with radius 7.
    let (lo, hi, r) = (2.0f32, 28.0f32, 7.0f32);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let cx = px.clamp(lo + r, hi - r);
            let cy = py.clamp(lo + r, hi - r);
            let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            let inside = (r + 0.5 - d).clamp(0.0, 1.0)
                * if (lo..=hi).contains(&px) && (lo..=hi).contains(&py) {
                    1.0
                } else {
                    0.0
                };
            if inside > 0.0 {
                put(x, y, pink, inside * if dim { 0.45 } else { 1.0 });
                // The white diamond in the middle: |dx| + |dy| <= 6.
                let m = ((px - 15.0).abs() + (py - 15.0).abs() - 6.0).clamp(-0.5, 0.5);
                let w = 0.5 - m;
                if w > 0.0 {
                    put(x, y, [255, 255, 255], w * if dim { 0.6 } else { 1.0 });
                }
            }
        }
    }
    if let Some(c) = dot {
        for y in 0..SIZE {
            for x in 0..SIZE {
                let d = ((x as f32 + 0.5 - 25.0).powi(2) + (y as f32 + 0.5 - 25.0).powi(2)).sqrt();
                let ring = (7.5 - d).clamp(0.0, 1.0);
                if ring > 0.0 {
                    put(x, y, [255, 255, 255], ring);
                }
                let fill = (5.5 - d).clamp(0.0, 1.0);
                if fill > 0.0 {
                    put(x, y, c, fill);
                }
            }
        }
    }
    Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}

/// Choose a folder the AI may use (a worker thread only: the dialog blocks).
pub fn pick_folder(app: &AppHandle) -> Option<String> {
    app.dialog()
        .file()
        .set_title("选择 AI 可以用的文件夹")
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| display(&p))
}

fn popup(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(POPUP) {
        return Ok(w);
    }
    WebviewWindowBuilder::new(app, POPUP, WebviewUrl::App("traypop.html".into()))
        .title("AgentRouter")
        .inner_size(280.0, 420.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .build()
}

/// Put the popup next to the tray icon, above the taskbar (or below a top taskbar).
pub fn place(window: &Window) {
    let Ok(size) = window.outer_size() else {
        return;
    };
    let anchor = *ANCHOR.lock().unwrap();
    let monitor = match anchor {
        Some((x, y, _, _)) => window.monitor_from_point(x, y).ok().flatten(),
        None => None,
    }
    .or_else(|| window.primary_monitor().ok().flatten());
    let Some(m) = monitor else { return };
    let area = m.work_area();
    let (ax, ay, aw, _) = anchor.unwrap_or((
        (area.position.x + area.size.width as i32) as f64 - 160.0,
        (area.position.y + area.size.height as i32) as f64,
        32.0,
        32.0,
    ));
    let gap = (8.0 * m.scale_factor()) as i32;
    let (w, h) = (size.width as i32, size.height as i32);
    let left = area.position.x + gap;
    let right = area.position.x + area.size.width as i32 - w - gap;
    let x = ((ax + aw / 2.0) as i32 - w / 2).clamp(left, right.max(left));
    let bottom_half = ay > (area.position.y + area.size.height as i32 / 2) as f64;
    let y = if bottom_half {
        area.position.y + area.size.height as i32 - h - gap
    } else {
        area.position.y + gap
    };
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// Show the popup; on a worker thread (building a WebView2 window never runs on the event loop).
pub fn show_popup(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Ok(w) = popup(&app) {
            if let Some(window) = app.get_window(POPUP) {
                place(&window);
            }
            let _ = w.show();
            let _ = w.set_always_on_top(true);
            let _ = w.set_focus();
        }
    });
}

fn rect_tuple(rect: &Rect, scale: f64) -> (f64, f64, f64, f64) {
    let p = rect.position.to_physical::<f64>(scale);
    let s = rect.size.to_physical::<f64>(scale);
    (p.x, p.y, s.width, s.height)
}

/// Create the tray and keep its icon current.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let tray = TrayIconBuilder::with_id(TRAY)
        .icon(icon("unlinked"))
        .tooltip("AgentRouter")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                rect,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                let scale = app
                    .primary_monitor()
                    .ok()
                    .flatten()
                    .map(|m| m.scale_factor())
                    .unwrap_or(1.0);
                *ANCHOR.lock().unwrap() = Some(rect_tuple(&rect, scale));
                let open = app
                    .get_webview_window(POPUP)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                if open {
                    if let Some(w) = app.get_webview_window(POPUP) {
                        let _ = w.hide();
                    }
                } else {
                    show_popup(app);
                }
            }
        })
        .build(app)?;
    let app = app.clone();
    std::thread::spawn(move || {
        let ui = ui();
        let mut shown = String::new();
        while !ui.rt.stop.load(Ordering::SeqCst) {
            let state = ui.state();
            let look = state["look"].as_str().unwrap_or("unlinked").to_string();
            let text = match look.as_str() {
                "idle" => "在线 · 空闲".to_string(),
                "working" => format!(
                    "正在干活 · {} 个连接",
                    state["sessions"].as_array().map(|a| a.len()).unwrap_or(0)
                ),
                "reconnecting" => "连不上，正在重连".to_string(),
                "disconnected" => "已断开".to_string(),
                _ => "还没链接".to_string(),
            };
            let key = format!("{look}|{text}");
            if key != shown {
                let _ = tray.set_icon(Some(icon(&look)));
                let _ = tray.set_tooltip(Some(format!(
                    "AgentRouter · {} · {text}",
                    state["name"].as_str().unwrap_or("")
                )));
                shown = key;
            }
            if ui.rt.revoked_notice.swap(false, Ordering::SeqCst) {
                inform(
                    TITLE,
                    "这台电脑已在网页上断开，AI 起的任务都已停止。要再用，请在托盘里重新链接。",
                );
            }
            if ui.indicator_failed.swap(false, Ordering::SeqCst) {
                inform(
                    TITLE,
                    "“正在使用”提示条显示不出来，所以 AI 的请求都被拒绝了。重新打开 AgentRouter 再试。",
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let _ = app;
    });
    Ok(())
}

#[cfg(windows)]
pub fn open_audit(rt: &Runtime) {
    let path = rt.device.audit_path();
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}

#[cfg(not(windows))]
pub fn open_audit(rt: &Runtime) {
    inform(
        TITLE,
        &format!("记录在 {}", rt.device.audit_path().display()),
    );
}
