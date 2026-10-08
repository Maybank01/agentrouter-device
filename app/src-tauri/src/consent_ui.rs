//! The consent dialog (03 樱粉): one always-on-top window per question, "拒绝" focused by default,
//! Esc / × / closing / two minutes without an answer all mean "no". "这个对话里同类命令以后直接允许"
//! is offered only for simple, non-destructive commands (`Ask::family`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use agentrouter_device::consent::{self, ASK_TIMEOUT, Ask, Confirm, Decision};
use agentrouter_device::util::short_id;
use serde_json::{Value, json};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::ui::{APP, Pending, ui};

/// What the dialog shows: plain sentences made here from the request, never text from the cloud
/// except the command or file itself.
pub fn view(ask: &Ask, device_name: &str) -> Value {
    let who = &ask.who;
    let subject = who.title();
    let (title, sentence) = match (ask.action, ask.destructive) {
        ("exec" | "exec_full", true) => (
            "要运行一条会删东西的命令".to_string(),
            format!("{subject}想在这台电脑上运行下面这条命令，它会删除或抹掉文件。"),
        ),
        ("exec_full", false) => (
            "要开始在这台电脑上跑命令".to_string(),
            format!(
                "{subject}想在这台电脑上运行命令。允许以后，这个对话里的命令不再逐条问你（完全访问）。"
            ),
        ),
        ("exec", false) => (
            "要在这台电脑上跑一条命令".to_string(),
            format!("{subject}想在这台电脑上运行下面这条命令。"),
        ),
        ("write", _) => (
            "要改一个文件".to_string(),
            format!("{subject}想写入下面这个文件。"),
        ),
        ("input", _) => (
            "要给正在跑的命令输入内容".to_string(),
            format!("{subject}想把下面的内容输入给正在运行的命令。"),
        ),
        _ => (
            "要在这台电脑上做一件事".to_string(),
            format!("{subject}想在这台电脑上做下面这件事。"),
        ),
    };
    let scope = if who.via == "local" {
        "这个连接"
    } else {
        "这个对话"
    };
    json!({
        "title": title,
        "sentence": sentence,
        "who": who,
        "whoTitle": subject,
        "client": who.client,
        "text": ask.text,
        "place": ask.place,
        "action": ask.action,
        "destructive": ask.destructive,
        "similar": ask.family.as_ref().filter(|_| !ask.destructive).map(|family| json!({
            "label": format!("{scope}里，同类命令以后直接允许"),
            "family": family,
        })),
        "device": device_name,
        "timeoutMs": ASK_TIMEOUT.as_millis() as u64,
        "openedAt": agentrouter_device::util::now_ms(),
    })
}

/// The person's answer from the dialog (or Deny when it was closed).
pub fn answer(label: &str, decision: Decision) {
    if let Some(p) = ui().consents.lock().unwrap().remove(label) {
        let _ = p.tx.send(decision);
    }
    if let Some(app) = APP.get()
        && let Some(w) = app.get_webview_window(label)
    {
        let _ = w.close();
    }
}

#[tauri::command]
pub fn consent_get(webview: tauri::Webview) -> Value {
    let label = webview.window().label().to_string();
    ui().consents
        .lock()
        .unwrap()
        .get(&label)
        .map(|p| p.view.clone())
        .unwrap_or(Value::Null)
}

/// Only the dialog's own window can answer its question.
#[tauri::command]
pub fn consent_answer(webview: tauri::Webview, decision: String) {
    let label = webview.window().label().to_string();
    if label.starts_with("consent-") {
        answer(&label, Decision::parse(&decision));
    }
}

/// The confirmer the device uses in the desktop app.
pub fn confirmer() -> Confirm {
    Arc::new(|ask: &Ask, cancel: &AtomicBool| {
        let Some(app) = APP.get() else {
            // Before the windows exist (should not happen): the plain system dialog.
            return (consent::native())(ask, cancel);
        };
        let ui = ui();
        let name = ui.rt.config.lock().unwrap().name.clone();
        let label = format!("consent-{}", short_id(""));
        let (tx, rx) = channel();
        ui.consents.lock().unwrap().insert(
            label.clone(),
            Pending {
                view: view(ask, &name),
                tx,
            },
        );
        let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("consent.html".into()))
            .title("AgentRouter · 需要你确认")
            .inner_size(460.0, 420.0)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .decorations(false)
            .transparent(true)
            .shadow(false)
            .always_on_top(true)
            .center()
            .focused(true)
            .build();
        if let Err(e) = built {
            agentrouter_device::util::log(&format!("consent window failed: {e}"));
            ui.consents.lock().unwrap().remove(&label);
            return (consent::native())(ask, cancel);
        }
        let started = Instant::now();
        let decision = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(d) => break d,
                Err(RecvTimeoutError::Disconnected) => break Decision::Deny,
                Err(RecvTimeoutError::Timeout) => {
                    if cancel.load(Ordering::SeqCst) || started.elapsed() >= ASK_TIMEOUT {
                        break Decision::Deny;
                    }
                }
            }
        };
        answer(&label, Decision::Deny);
        decision
    })
}
