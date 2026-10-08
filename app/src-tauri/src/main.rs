//! AgentRouter desktop: the cloud web app (the same pages as in the browser, the source of truth) with
//! a native 电脑 panel on the right, and the linked-device connector built in. Everything that needs
//! this computer lives here: the connector, the tray, the pairing and consent windows, the "being
//! controlled" bar and the local MCP for AIs on this computer.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bar;
mod bridge;
mod consent_ui;
mod pair;
#[cfg(feature = "smoke")]
mod smoke;
mod tray;
mod ui;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agentrouter_device::config::Config;
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::link;
use agentrouter_device::local_ipc;
use agentrouter_device::mcp::LocalHub;
use agentrouter_device::net::{self, Runtime};
use agentrouter_device::share::ShareService;
use agentrouter_device::util::{data_dir, home_dir, log};
use tauri::ipc::CapabilityBuilder;
use tauri::webview::WebviewBuilder;
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WindowEvent};

pub const MAIN: &str = "main";
/// The cloud page and the native panel inside the main window.
pub const WEB: &str = "web";
pub const PANEL: &str = "panel";
/// Width of the 电脑 panel, open and folded to a rail (logical pixels).
const PANEL_OPEN: f64 = 360.0;
const PANEL_RAIL: f64 = 52.0;

/// Show (or bring back) the main window.
pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_window(MAIN) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Put the cloud page and the panel side by side in the main window.
pub fn layout_main(app: &AppHandle) {
    let Some(window) = app.get_window(MAIN) else {
        return;
    };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let size = size.to_logical::<f64>(scale);
    let panel = if ui::ui().panel_open.load(Ordering::SeqCst) {
        PANEL_OPEN
    } else {
        PANEL_RAIL
    }
    .min(size.width);
    if let Some(web) = app.get_webview(WEB) {
        let _ = web.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = web.set_size(LogicalSize::new((size.width - panel).max(0.0), size.height));
    }
    if let Some(p) = app.get_webview(PANEL) {
        let _ = p.set_position(LogicalPosition::new(size.width - panel, 0.0));
        let _ = p.set_size(LogicalSize::new(panel, size.height));
    }
}

/// Open a page of the app's own web origin in the main window (never anything else).
pub fn open_in_web(app: &AppHandle, url: &str) {
    let web = ui::ui().rt.config.lock().unwrap().web.clone();
    if !link::safe_url(url) || !same_origin(&web, url) {
        return;
    }
    let Ok(parsed) = url.parse() else { return };
    if let Some(view) = app.get_webview(WEB) {
        let _ = view.navigate(parsed);
    }
    show_main(app);
}

fn same_origin(a: &str, b: &str) -> bool {
    fn origin(u: &str) -> Option<&str> {
        let rest = u.split_once("://")?.1;
        let host_end = rest.find('/').unwrap_or(rest.len());
        Some(&u[..u.len() - rest.len() + host_end])
    }
    matches!((origin(a), origin(b)), (Some(x), Some(y)) if x.eq_ignore_ascii_case(y))
}

/// The cloud page may go to any web page, never to the app's own local pages or other schemes.
fn web_may_open(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "https" | "http")
        && !matches!(url.host_str(), Some(h) if h.eq_ignore_ascii_case("tauri.localhost"))
}

/// Windows 11: the native title bar takes the window's own colour (03: white, or the dark surface).
#[cfg(windows)]
fn paint_caption(window: &tauri::Window) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    let Ok(hwnd) = window.hwnd() else { return };
    let dark = matches!(window.theme(), Ok(tauri::Theme::Dark));
    // COLORREF is 0x00BBGGRR.
    let (caption, text): (u32, u32) = if dark {
        (0x0027_2428, 0x00F2_F0F2)
    } else {
        (0x00FF_FFFF, 0x0040_3A3D)
    };
    let dark_mode: u32 = dark as u32;
    // SAFETY: plain values for documented DWM attributes (20 dark mode, 35 caption, 36 text colour) on
    // this window's own handle; failures (Windows 10) are ignored.
    unsafe {
        for (attr, value) in [(20u32, dark_mode), (35, caption), (36, text)] {
            DwmSetWindowAttribute(
                hwnd.0 as _,
                attr,
                &value as *const u32 as *const _,
                std::mem::size_of::<u32>() as u32,
            );
        }
    }
}

#[cfg(not(windows))]
fn paint_caption(_window: &tauri::Window) {}

/// Commands the app's own local pages may call (never the cloud page).
const LOCAL_COMMANDS: &[&str] = &[
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
];

fn main() {
    agentrouter_device::util::set_app_kind("desktop");
    let cfg = Config::load();
    let device = Arc::new(Device::new(Options {
        data_dir: data_dir(),
        access: cfg.access,
        folders: cfg.folders.clone(),
        confirm: consent_ui::confirmer(),
        // Nothing runs unless the "being controlled" bar is on screen.
        indicator: Arc::new(bar::BarIndicator),
        home: home_dir(),
        shell: Shell::default_for_os(),
    }));
    log(&format!(
        "desktop {} starting (web {}, gateway {}, access {})",
        env!("CARGO_PKG_VERSION"),
        cfg.web,
        cfg.gateway,
        cfg.access
    ));
    let web = cfg.web.clone();
    let rt = Runtime::new(device.clone(), cfg);
    {
        let rt = rt.clone();
        std::thread::spawn(move || net::run(rt));
    }
    {
        let rt = rt.clone();
        std::thread::spawn(move || net::watch_config(rt));
    }

    let names = rt.clone();
    let hub = Arc::new(LocalHub::new(
        device.clone(),
        Arc::new(move || names.config.lock().unwrap().name.clone()),
    ));
    // The local MCP: AIs on this computer reach this device through a current-user-only pipe/socket.
    let local = match local_ipc::Server::start(hub.clone()) {
        Ok(server) => Some(server),
        Err(e) => {
            log(&format!("local MCP not available: {e}"));
            None
        }
    };
    #[cfg(feature = "smoke")]
    let shares: Box<dyn ShareService> = Box::new(smoke::DemoShares);
    #[cfg(not(feature = "smoke"))]
    let shares: Box<dyn ShareService> = Box::new(agentrouter_device::share::Unavailable);
    ui::install(Arc::new(ui::Ui::new(rt.clone(), hub, shares)));

    let result = tauri::Builder::default()
        // A second launch brings the running window forward instead of starting another connector.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            bridge::device_status,
            bridge::device_link,
            ui::ui_state,
            ui::ui_action,
            ui::win_fit,
            ui::win_close,
            ui::win_minimize,
            ui::win_drag,
            pair::pair_get,
            pair::pair_submit,
            pair::pair_pick_folder,
            consent_ui::consent_get,
            consent_ui::consent_answer,
            bar::bar_drag_start,
            bar::bar_drag,
        ])
        .setup(move |app| {
            let _ = ui::APP.set(app.handle().clone());
            let url: tauri::Url = web
                .parse()
                .map_err(|e| format!("bad web address {web}: {e}"))?;
            let window = tauri::window::WindowBuilder::new(app, MAIN)
                .title("AgentRouter")
                .inner_size(1280.0, 820.0)
                .min_inner_size(760.0, 520.0)
                .build()?;
            paint_caption(&window);
            window.add_child(
                WebviewBuilder::new(WEB, WebviewUrl::External(url)).on_navigation(web_may_open),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(1280.0 - PANEL_OPEN, 820.0),
            )?;
            window.add_child(
                WebviewBuilder::new(PANEL, WebviewUrl::App("panel.html".into())),
                LogicalPosition::new(1280.0 - PANEL_OPEN, 0.0),
                LogicalSize::new(PANEL_OPEN, 820.0),
            )?;
            layout_main(app.handle());
            // The cloud page: status and "link this computer", for the configured web origin only.
            app.add_capability(
                CapabilityBuilder::new("cloud-web")
                    .remote(format!("{}/*", web.trim_end_matches('/')))
                    .local(false)
                    .webview(WEB)
                    .permission("allow-device-status")
                    .permission("allow-device-link"),
            )?;
            // The app's own pages (bundled, local origin only).
            let mut local_ui = CapabilityBuilder::new("local-ui")
                .local(true)
                .webview(PANEL)
                .windows(["pair", "consent-*", "bar", "frame-*", "traypop"]);
            for command in LOCAL_COMMANDS {
                local_ui = local_ui.permission(format!("allow-{}", command.replace('_', "-")));
            }
            app.add_capability(local_ui)?;
            tray::build(app.handle())?;
            bar::watchdog(app.handle().clone());
            #[cfg(feature = "smoke")]
            smoke::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            let label = window.label().to_string();
            match event {
                // Closing the window keeps the app (and the device connection) in the tray.
                WindowEvent::CloseRequested { api, .. } if label == MAIN => {
                    let _ = window.hide();
                    api.prevent_close();
                }
                WindowEvent::CloseRequested { api, .. }
                    if label == bar::BAR || label.starts_with("frame-") =>
                {
                    api.prevent_close();
                }
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
                    if label == MAIN =>
                {
                    layout_main(window.app_handle());
                }
                WindowEvent::ThemeChanged(_) if label == MAIN => paint_caption(window),
                WindowEvent::Moved(_) if label == bar::BAR => bar::place(window),
                WindowEvent::Focused(false) if label == tray::POPUP => {
                    let _ = window.hide();
                }
                WindowEvent::Destroyed if label.starts_with("consent-") => {
                    consent_ui::answer(&label, agentrouter_device::consent::Decision::Deny);
                }
                WindowEvent::Destroyed
                    if label == pair::PAIR && ui::ui().pair.lock().unwrap().stage == "linking" =>
                {
                    pair::cancel(window.app_handle());
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!());
    match result {
        Ok(app) => app.run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                rt.stop.store(true, Ordering::SeqCst);
                device.stop_all("quit");
                let _ = &local;
                log("desktop stopped");
            }
        }),
        Err(e) => log(&format!("the desktop app could not start: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{same_origin, web_may_open};

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

    #[test]
    fn the_cloud_page_never_opens_local_pages() {
        assert!(web_may_open(
            &"https://agentrouter.top/chat".parse().unwrap()
        ));
        assert!(!web_may_open(
            &"http://tauri.localhost/panel.html".parse().unwrap()
        ));
        assert!(!web_may_open(
            &"tauri://localhost/panel.html".parse().unwrap()
        ));
        assert!(!web_may_open(&"file:///C:/x".parse().unwrap()));
    }
}
