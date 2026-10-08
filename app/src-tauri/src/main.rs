//! AgentRouter desktop: a small shell around the cloud web app (the same pages as in the browser),
//! with the linked-device connector built in. Everything that needs this computer lives here: the
//! connector, the tray, local confirmation dialogs; everything else is the web app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
mod tray;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agentrouter_device::config::Config;
use agentrouter_device::consent;
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::net::{self, Runtime};
use agentrouter_device::util::{data_dir, home_dir, log};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

pub const MAIN: &str = "main";

/// Show (or bring back) the main window.
pub fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window(MAIN) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn main() {
    let cfg = Config::load();
    let device = Arc::new(Device::new(Options {
        data_dir: data_dir(),
        access: cfg.access,
        folders: cfg.folders.clone(),
        confirm: consent::native(),
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

    let result = tauri::Builder::default()
        // A second launch brings the running window forward instead of starting another connector.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .manage(rt.clone())
        .invoke_handler(tauri::generate_handler![bridge::device_status, bridge::device_link])
        .setup(move |app| {
            let url = web
                .parse()
                .map_err(|e| format!("bad web address {web}: {e}"))?;
            WebviewWindowBuilder::new(app, MAIN, WebviewUrl::External(url))
                .title("AgentRouter")
                .inner_size(1200.0, 800.0)
                .min_inner_size(400.0, 500.0)
                .build()?;
            tray::build(app.handle(), app.state::<Arc<Runtime>>().inner().clone())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app (and the device connection) in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == MAIN {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!());
    match result {
        Ok(app) => app.run(move |_app, event| {
            if let tauri::RunEvent::Exit = event {
                rt.stop.store(true, Ordering::SeqCst);
                device.stop_all("quit");
                log("desktop stopped");
            }
        }),
        Err(e) => log(&format!("the desktop app could not start: {e}")),
    }
}
