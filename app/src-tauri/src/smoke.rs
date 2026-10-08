//! GUI smoke test for CI (feature `smoke`, debug builds on a GitHub Windows runner only). It walks
//! through the windows in their states and, between scenes, waits for the workflow to take a
//! screenshot: it writes the scene name to `$AGENTROUTER_SMOKE_DIR/scene.txt` and waits for
//! `<scene>.shot`. Along the way it drives the real device through the local gate and checks the hard
//! rules (nothing runs without the bar, pause refuses, destructive commands are asked and can be
//! refused). Results go to `results.json`; the app exits non-zero when a check failed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agentrouter_device::consent::Decision;
use agentrouter_device::local_ipc::{self, ClientInfo};
use agentrouter_device::presence::Who;
use agentrouter_device::protocol::DeviceError;
use agentrouter_device::share::{
    self, ShareError, ShareService, ShareTerms, ShareView, display_code, display_password,
    generate_password,
};
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};

use crate::ui::{act, ui};

/// The sample share of the owner's draft (a demo value; real codes come from the control plane).
pub struct DemoShares;

impl ShareService for DemoShares {
    fn open(&self, terms: &ShareTerms) -> Result<ShareView, ShareError> {
        Ok(ShareView {
            share: terms.share.clone(),
            code: display_code("482913706"),
            password: display_password(&generate_password()),
            expires_at: terms.expires_at,
            access: terms.access.clone(),
        })
    }
    fn rotate(&self, share: &str) -> Result<ShareView, ShareError> {
        Ok(ShareView {
            share: share.to_string(),
            code: display_code("482913706"),
            password: display_password(&generate_password()),
            expires_at: 0,
            access: "confirm".into(),
        })
    }
    fn stop(&self, _: &str, _: &str) -> Result<(), ShareError> {
        Ok(())
    }
    fn connect(&self, code: &str, password: &str) -> Result<Value, ShareError> {
        share::Unavailable.connect(code, password)
    }
}

struct Run {
    dir: PathBuf,
    checks: Vec<Value>,
}

impl Run {
    fn shot(&self, scene: &str) {
        // Let the pages poll and paint first.
        std::thread::sleep(Duration::from_millis(1500));
        let _ = std::fs::write(self.dir.join("scene.txt"), scene);
        let done = self.dir.join(format!("{scene}.shot"));
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline && !done.exists() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        agentrouter_device::util::log(&format!("smoke check {name}: {ok} {detail}"));
        self.checks
            .push(json!({"name": name, "ok": ok, "detail": detail}));
    }
}

fn fake(value: Value) {
    *ui().fake.lock().unwrap() = value;
}

/// Run a local-AI request on a worker thread, as the local MCP would.
fn request(
    session: &'static str,
    action: &'static str,
    args: Value,
) -> std::thread::JoinHandle<Result<Value, DeviceError>> {
    std::thread::spawn(move || {
        ui().rt.device.serve_local(
            session,
            &Who::local("Claude Code"),
            action,
            &args,
            &AtomicBool::new(false),
        )
    })
}

/// Wait for a consent window and return its label.
fn consent_open() -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(label) = ui().consents.lock().unwrap().keys().next().cloned() {
            return Some(label);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn code_of(r: &Result<Value, DeviceError>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => e.code.to_string(),
    }
}

fn quote(p: &Path) -> String {
    format!("'{}'", p.display())
}

pub fn start(app: AppHandle) {
    let Some(dir) = std::env::var_os("AGENTROUTER_SMOKE_DIR").map(PathBuf::from) else {
        return;
    };
    std::thread::spawn(move || {
        let mut run = Run {
            dir: dir.clone(),
            checks: Vec::new(),
        };
        scenes(&app, &mut run);
        let ok = run.checks.iter().all(|c| c["ok"] == true);
        let _ = std::fs::write(
            dir.join("results.json"),
            serde_json::to_string_pretty(&json!({"ok": ok, "checks": run.checks})).unwrap(),
        );
        let _ = std::fs::write(dir.join("scene.txt"), "done");
        std::thread::sleep(Duration::from_secs(2));
        app.exit(if ok { 0 } else { 1 });
    });
}

fn scenes(app: &AppHandle, run: &mut Run) {
    std::thread::sleep(Duration::from_secs(5));
    let ui = ui();
    let work = run.dir.join("work");
    let _ = std::fs::create_dir_all(work.join("素材"));
    let work = std::fs::canonicalize(&work).unwrap_or(work);
    let work_text = agentrouter_device::gate::display(&work);

    // Fit the main window to the screen of the runner.
    if let Some(w) = app.get_window(crate::MAIN) {
        if let Ok(Some(m)) = w.primary_monitor() {
            let area = m.work_area();
            let _ = w.set_position(area.position);
            let _ = w.set_size(area.size);
        }
        crate::show_main(app);
    }

    // The 电脑 panel before linking.
    run.shot("main-unlinked");

    // Pairing window, all stages.
    crate::pair::open(app, "link", false);
    {
        let mut p = ui.pair.lock().unwrap();
        p.name = "台式机".into();
        p.folders = vec![work_text.clone()];
    }
    run.shot("pair-choose");
    let stage = |stage: &str, step: u8, error: &str, message: &str| {
        let mut p = ui.pair.lock().unwrap();
        p.stage = stage.into();
        p.step = step;
        p.code = "482 913".into();
        p.error = error.into();
        p.message = message.into();
    };
    stage("linking", 1, "", "");
    run.shot("pair-linking");
    stage("done", 3, "", "");
    run.shot("pair-done");
    stage(
        "error",
        0,
        "network",
        "这台电脑连不上云端。看看网络，再试一次。",
    );
    run.shot("pair-error-network");
    stage(
        "error",
        0,
        "expired",
        "配对码 10 分钟内有效。换一个新的，再填到网页上。",
    );
    run.shot("pair-error-expired");
    ui.pair.lock().unwrap().stage = "done".into();
    crate::pair::close(app);

    // A linked, online device from here on (the runner has no account): shown values only, the
    // device itself still goes through the real gate below.
    let online = json!({"linked": true, "conn": "online", "name": "台式机"});
    fake(online.clone());
    ui.rt.device.set_access(
        agentrouter_device::config::Access::Folders,
        std::slice::from_ref(&work_text),
    );
    {
        let mut cfg = ui.rt.config.lock().unwrap();
        cfg.access = agentrouter_device::config::Access::Folders;
        cfg.folders = vec![work_text.clone()];
        cfg.name = "台式机".into();
    }

    // Tray popup, four states.
    for (look, scene) in [
        ("idle", "tray-idle"),
        ("reconnecting", "tray-reconnecting"),
        ("disconnected", "tray-disconnected"),
    ] {
        let mut v = online.clone();
        v["look"] = json!(look);
        fake(v);
        crate::tray::show_popup(app);
        run.shot(scene);
    }
    let mut working = online.clone();
    working["look"] = json!("working");
    working["sessions"] = json!([
        {"session": "lcl_a", "who": {"via": "local", "name": "Claude Code", "title": "本机的 Claude Code"}, "activity": "运行：npm test", "startedAt": 0},
        {"session": "ags_b", "who": {"via": "conversation", "name": "你的对话", "title": "你的对话"}, "activity": "整理素材文件夹", "startedAt": 0}
    ]);
    fake(working);
    crate::tray::show_popup(app);
    run.shot("tray-working");
    if let Some(w) = app.get_webview_window(crate::tray::POPUP) {
        let _ = w.hide();
    }
    fake(online.clone());
    crate::show_main(app);

    // A local AI asks to run a command: consent window + bar "等你确认".
    let first = request(
        "lcl_smoke_1",
        "exec",
        json!({"command": "Get-ChildItem", "cwd": work_text, "timeout": 20}),
    );
    match consent_open() {
        Some(label) => {
            let view = ui
                .consents
                .lock()
                .unwrap()
                .get(&label)
                .map(|p| p.view.clone());
            let offered = view.as_ref().is_some_and(|v| !v["similar"].is_null());
            run.check(
                "consent offers 'allow similar' for a simple command",
                offered,
                label.clone(),
            );
            run.check(
                "bar is shown while asking",
                app.get_webview_window(crate::bar::BAR)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false),
                "",
            );
            run.shot("consent-normal");
            crate::consent_ui::answer(&label, Decision::Similar);
        }
        None => run.check("consent window opens", false, "no consent window"),
    }
    let r = first.join().unwrap();
    run.check("allowed command runs", r.is_ok(), code_of(&r));

    // A longer command: the bar shows the activity and the screen edges glow.
    let long = request(
        "lcl_smoke_1",
        "exec",
        json!({"command": "Start-Sleep -Seconds 9", "cwd": work_text, "timeout": 30}),
    );
    if let Some(label) = consent_open() {
        crate::consent_ui::answer(&label, Decision::Once);
    }
    std::thread::sleep(Duration::from_millis(1500));
    run.check(
        "edges glow while a command runs",
        ui.state()["glowing"] == true,
        "",
    );
    run.shot("controlling");
    let r = long.join().unwrap();
    run.check("long command finishes", r.is_ok(), code_of(&r));

    // A deleting command: red dialog, no "allow similar", refused here.
    let junk = std::path::PathBuf::from(&work_text).join("junk");
    let _ = std::fs::create_dir_all(&junk);
    let _ = std::fs::write(junk.join("a.txt"), "keep me");
    let delete = request(
        "lcl_smoke_1",
        "exec",
        json!({"command": format!("Remove-Item {} -Recurse", quote(&junk)), "cwd": work_text, "timeout": 20}),
    );
    match consent_open() {
        Some(label) => {
            let view = ui
                .consents
                .lock()
                .unwrap()
                .get(&label)
                .map(|p| p.view.clone());
            run.check(
                "destructive command is marked and not 'similar'",
                view.as_ref()
                    .is_some_and(|v| v["destructive"] == true && v["similar"].is_null()),
                "",
            );
            run.shot("consent-destructive");
            crate::consent_ui::answer(&label, Decision::Deny);
        }
        None => run.check("destructive consent opens", false, ""),
    }
    let r = delete.join().unwrap();
    run.check(
        "refused delete does not run",
        code_of(&r) == "DENIED" && junk.join("a.txt").exists(),
        code_of(&r),
    );

    // Connected, nothing running.
    std::thread::sleep(Duration::from_secs(3));
    run.shot("bar-idle");

    // Pause on the bar: new requests are refused.
    let _ = act("pause", &json!(true));
    let r = request(
        "lcl_smoke_1",
        "exec",
        json!({"command": "Get-ChildItem", "cwd": work_text}),
    )
    .join()
    .unwrap();
    run.check(
        "paused device refuses",
        code_of(&r) == "PAUSED",
        code_of(&r),
    );
    run.shot("bar-paused");
    let _ = act("pause", &json!(false));

    // Local AIs connected through the local MCP, share on (demo code), the panel at home.
    let mut clients = Vec::new();
    for name in ["claude-code", "codex"] {
        match local_ipc::Client::connect(&ClientInfo {
            name: name.into(),
            version: "1.0".into(),
        }) {
            Ok(c) => clients.push(c),
            Err(e) => run.check("local MCP connects", false, e.to_string()),
        }
    }
    run.check(
        "panel lists local AIs",
        ui.hub.clients().len() == clients.len() && !clients.is_empty(),
        format!("{}", ui.hub.clients().len()),
    );
    if let Some(c) = clients.first_mut() {
        let list = c.request(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "list_devices", "arguments": {}}}));
        let ok = list
            .as_ref()
            .is_ok_and(|v| v["result"]["structuredContent"]["devices"][0]["device"] == "this");
        run.check("local MCP lists this computer", ok, "");
    }
    let _ = act("share_on", &Value::Null);
    run.shot("panel-fraud");
    let _ = act("share_confirm", &Value::Null);
    let r = request(
        "lcl_smoke_1",
        "exec",
        json!({"command": "Start-Sleep -Seconds 6", "cwd": work_text, "timeout": 30}),
    );
    if let Some(label) = consent_open() {
        crate::consent_ui::answer(&label, Decision::Once);
    }
    run.shot("main-home");
    let _ = r.join();
    let _ = act(
        "connect",
        &json!({"code": "123 456 789", "password": "ABCD-EFGH"}),
    );
    run.shot("panel-connect");
    let _ = act("panel", &json!(false));
    run.shot("main-panel-folded");
    let _ = act("panel", &json!(true));
    drop(clients);

    // The hard rule: without the bar nothing runs, nobody is asked.
    let _ = act("end_all", &Value::Null);
    ui.break_indicator.store(true, Ordering::SeqCst);
    let r = request(
        "lcl_smoke_9",
        "exec",
        json!({"command": "Get-ChildItem", "cwd": work_text}),
    )
    .join()
    .unwrap();
    run.check(
        "no indicator, no execution",
        code_of(&r) == "INDICATOR_UNAVAILABLE"
            && ui.consents.lock().unwrap().is_empty()
            && ui.rt.device.jobs.running() == 0,
        code_of(&r),
    );
    run.shot("indicator-broken");
    ui.break_indicator.store(false, Ordering::SeqCst);
}
