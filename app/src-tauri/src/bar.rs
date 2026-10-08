//! The "being controlled" indicator (LINKED-DEVICES.md §17, a hard requirement): a topmost bar at the
//! top of the screen while anyone is connected (who, AI client, activity, timer, 暂停, 断开), and a
//! pink glow around every screen while commands run. It cannot be closed or minimised; a watchdog
//! brings it back. **The device runs nothing unless [`BarIndicator::ensure`] says it is on screen.**

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agentrouter_device::presence::Indicator;
use serde_json::{Value, json};
use tauri::{
    AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Window,
};

use crate::ui::{APP, ui};

pub const BAR: &str = "bar";
/// Logical size of the full bar and of the compact pill (with room for the shadow).
const FULL: (f64, f64) = (660.0, 76.0);
const COMPACT: (f64, f64) = (300.0, 76.0);
/// Gap between the top of the work area and the bar, logical pixels.
const TOP: f64 = 6.0;
/// Closer than this to a screen edge, the bar becomes the compact pill.
const EDGE: i32 = 24;

#[derive(Default)]
pub struct BarUi {
    pub compact: bool,
    /// Window x when a drag started (physical).
    drag_from: Option<i32>,
}

impl BarUi {
    pub fn view(&self) -> Value {
        json!({"compact": self.compact})
    }
}

/// Serialises creating and showing the bar (requests arrive on several threads).
static SHOWING: Mutex<()> = Mutex::new(());

fn create(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(app, BAR, WebviewUrl::App("bar.html".into()))
        .title("AgentRouter · 正在使用这台电脑")
        .inner_size(FULL.0, FULL.1)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .closable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        .build()?;
    home(&window);
    Ok(window)
}

/// Top centre of the monitor the bar is on (the primary one at first).
fn home(window: &WebviewWindow) {
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    let (Some(m), Ok(size)) = (monitor, window.outer_size()) else {
        return;
    };
    let area = m.work_area();
    let x = area.position.x + (area.size.width as i32 - size.width as i32) / 2;
    let y = area.position.y + (TOP * m.scale_factor()) as i32;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// Keep the bar on the top edge and inside its monitor (after a resize or a drag).
pub fn place(window: &Window) {
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return;
    };
    let Some(m) = window
        .monitor_from_point(pos.x as f64 + size.width as f64 / 2.0, pos.y as f64 + 4.0)
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten())
    else {
        return;
    };
    let area = m.work_area();
    let left = area.position.x;
    let right = area.position.x + area.size.width as i32 - size.width as i32;
    let x = pos.x.clamp(left, right.max(left));
    let y = area.position.y + (TOP * m.scale_factor()) as i32;
    if (x, y) != (pos.x, pos.y) {
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }
}

/// Show the bar (creating it on first use); true when it is visible.
pub fn show(app: &AppHandle) -> bool {
    let _one = SHOWING.lock().unwrap();
    let window = match app.get_webview_window(BAR) {
        Some(w) => w,
        None => match create(app) {
            Ok(w) => w,
            Err(e) => {
                agentrouter_device::util::log(&format!("the bar could not be created: {e}"));
                return false;
            }
        },
    };
    if window.is_minimized().unwrap_or(false) {
        let _ = window.unminimize();
    }
    if !window.is_visible().unwrap_or(false) {
        let _ = window.show();
    }
    let _ = window.set_always_on_top(true);
    window.is_visible().unwrap_or(false)
}

fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(BAR)
        && w.is_visible().unwrap_or(false)
    {
        let _ = w.hide();
    }
}

/// The indicator the device checks before every action.
pub struct BarIndicator;

impl Indicator for BarIndicator {
    fn ensure(&self) -> bool {
        let Some(app) = APP.get() else {
            return false;
        };
        let ui = ui();
        if ui.break_indicator.load(Ordering::SeqCst) {
            ui.indicator_failed.store(true, Ordering::SeqCst);
            return false;
        }
        let shown = show(app);
        ui.indicator_failed.store(!shown, Ordering::SeqCst);
        shown
    }
}

/// Keeps the bar and the edge glow in step with the sessions, and the bar on top and unminimised.
pub fn watchdog(app: AppHandle) {
    std::thread::spawn(move || {
        let ui = ui();
        let mut frames_on = false;
        while !ui.rt.stop.load(Ordering::SeqCst) {
            let device = &ui.rt.device;
            let active = !device
                .presence
                .snapshot(&device.jobs.running_sessions())
                .is_empty();
            if active {
                if !ui.break_indicator.load(Ordering::SeqCst) {
                    let shown = show(&app);
                    ui.indicator_failed.store(!shown, Ordering::SeqCst);
                }
            } else {
                hide(&app);
            }
            let glow = active && device.presence.glowing(device.jobs.running());
            if glow != frames_on {
                frames(&app, glow);
                frames_on = glow;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

/// One click-through, transparent, topmost window per monitor with the pink edge glow.
fn frames(app: &AppHandle, on: bool) {
    let monitors = app.available_monitors().unwrap_or_default();
    for (i, m) in monitors.iter().enumerate() {
        let label = format!("frame-{i}");
        if !on {
            if let Some(w) = app.get_webview_window(&label) {
                let _ = w.hide();
            }
            continue;
        }
        let window = match app.get_webview_window(&label) {
            Some(w) => w,
            None => {
                let built =
                    WebviewWindowBuilder::new(app, &label, WebviewUrl::App("frame.html".into()))
                        .title("AgentRouter · 正在使用这台电脑")
                        .decorations(false)
                        .transparent(true)
                        .shadow(false)
                        .resizable(false)
                        .closable(false)
                        .minimizable(false)
                        .maximizable(false)
                        .always_on_top(true)
                        .skip_taskbar(true)
                        .focused(false)
                        .focusable(false)
                        .visible(false)
                        .build();
                match built {
                    Ok(w) => w,
                    Err(_) => continue,
                }
            }
        };
        let _ = window.set_ignore_cursor_events(true);
        let _ = window.set_position(*m.position());
        let _ = window.set_size(*m.size());
        let _ = window.show();
        let _ = window.set_always_on_top(true);
    }
    // The bar stays above the glow.
    if on && let Some(bar) = app.get_webview_window(BAR) {
        let _ = bar.set_always_on_top(true);
    }
}

/// Drag along the top edge only: the page sends the pointer's horizontal movement.
#[tauri::command]
pub fn bar_drag_start(webview: tauri::Webview) {
    let window = webview.window();
    if window.label() != BAR {
        return;
    }
    if let Ok(pos) = window.outer_position() {
        ui().bar.lock().unwrap().drag_from = Some(pos.x);
    }
}

#[tauri::command]
pub fn bar_drag(webview: tauri::Webview, dx: f64) {
    let window = webview.window();
    if window.label() != BAR {
        return;
    }
    let Some(from) = ui().bar.lock().unwrap().drag_from else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let (Ok(pos), Ok(size)) = (window.outer_position(), window.outer_size()) else {
        return;
    };
    let x = from + (dx * scale) as i32;
    let _ = window.set_position(PhysicalPosition::new(x, pos.y));
    place(&window);
    // Near a screen edge the bar turns into the compact pill (still with 断开).
    if let (Ok(pos), Some(m)) = (
        window.outer_position(),
        window
            .monitor_from_point(pos.x as f64 + 4.0, pos.y as f64 + 4.0)
            .ok()
            .flatten(),
    ) {
        let area = m.work_area();
        let near = pos.x - area.position.x < EDGE
            || area.position.x + area.size.width as i32 - (pos.x + size.width as i32) < EDGE;
        let mut bar = ui().bar.lock().unwrap();
        if near != bar.compact {
            bar.compact = near;
            let (w, h) = if near { COMPACT } else { FULL };
            drop(bar);
            let right_edge = pos.x + size.width as i32;
            let _ = window.set_size(tauri::LogicalSize::new(w, h));
            // Keep the pill against the right edge when that is where it was dragged.
            if pos.x - area.position.x >= EDGE
                && let Ok(new) = window.outer_size()
            {
                let _ = window
                    .set_position(PhysicalPosition::new(right_edge - new.width as i32, pos.y));
            }
            ui().bar.lock().unwrap().drag_from = window
                .outer_position()
                .ok()
                .map(|p| p.x - (dx * scale) as i32);
            place(&window);
        }
    }
}
