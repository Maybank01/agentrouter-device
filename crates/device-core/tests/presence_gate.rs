//! The presence gate (LINKED-DEVICES.md §17) and the consent decisions: nothing runs unless the
//! "being controlled" indicator is on screen, pause refuses new requests, "allow the same kind from now
//! on" only covers simple, non-destructive commands of the same conversation.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentrouter_device::config::Access;
use agentrouter_device::consent::{Ask, Confirm, Decision};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::keystore::Identity;
use agentrouter_device::presence::{Indicator, Terminal};
use agentrouter_device::protocol::DeviceKey;
use common::*;
use serde_json::json;

/// An indicator the test switches on and off.
struct Switch(AtomicBool);

impl Indicator for Switch {
    fn ensure(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Answers with `answer` and keeps every ask.
fn recording(answer: Decision, asks: Arc<Mutex<Vec<Ask>>>) -> Confirm {
    Arc::new(move |ask: &Ask, _: &AtomicBool| {
        asks.lock().unwrap().push(ask.clone());
        answer
    })
}

fn device_with(
    data: &std::path::Path,
    folder: &std::path::Path,
    confirm: Confirm,
    indicator: Arc<dyn Indicator>,
    cp: &ControlPlane,
) -> Device {
    let device = Device::new(Options {
        data_dir: data.to_path_buf(),
        access: Access::Folders,
        folders: vec![folder.display().to_string()],
        confirm,
        indicator,
        home: data.to_path_buf(),
        shell: Shell::default_for_os(),
    });
    device.set_identity(Some(Identity {
        key: DeviceKey::generate(),
        device: DEVICE_ID.to_string(),
        control: cp.control_key(),
    }));
    device
}

fn code(r: Result<serde_json::Value, (String, String)>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err((code, _)) => code,
    }
}

#[test]
fn nothing_runs_without_the_indicator() {
    let cp = ControlPlane::new();
    let folder = temp_dir("indicator-folder");
    std::fs::write(folder.join("a.txt"), "hello").unwrap();
    let asks = Arc::new(Mutex::new(Vec::new()));
    let switch = Arc::new(Switch(AtomicBool::new(false)));
    let device = device_with(
        &temp_dir("indicator"),
        &folder,
        recording(Decision::Once, asks.clone()),
        switch.clone(),
        &cp,
    );
    let exec = json!({"command": echo("x"), "timeout": 10});
    assert_eq!(
        code(call(&device, &cp, "exec", exec.clone())),
        "INDICATOR_UNAVAILABLE"
    );
    let read = json!({"path": folder.join("a.txt").display().to_string()});
    assert_eq!(
        code(call(&device, &cp, "read_file", read.clone())),
        "INDICATOR_UNAVAILABLE"
    );
    // Refused before anyone was asked and before any shell started.
    assert!(asks.lock().unwrap().is_empty());
    assert_eq!(device.jobs.running(), 0);
    assert!(device.jobs.list("ags_test").is_empty());
    // `info` stays available (it does nothing on the computer).
    assert_eq!(code(call(&device, &cp, "info", json!({}))), "ok");

    switch.0.store(true, Ordering::SeqCst);
    assert_eq!(code(call(&device, &cp, "read_file", read)), "ok");
    assert_eq!(code(call(&device, &cp, "exec", exec)), "ok");
    let sessions = device.presence.snapshot(&device.jobs.running_sessions());
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session, "ags_test");
    assert_eq!(sessions[0].who.via, "conversation");
}

#[test]
fn pause_refuses_new_requests() {
    let cp = ControlPlane::new();
    let folder = temp_dir("pause-folder");
    let asks = Arc::new(Mutex::new(Vec::new()));
    let device = device_with(
        &temp_dir("pause"),
        &folder,
        recording(Decision::Once, asks.clone()),
        Arc::new(Terminal),
        &cp,
    );
    device.set_paused(true);
    assert_eq!(
        code(call(
            &device,
            &cp,
            "exec",
            json!({"command": echo("x"), "timeout": 5})
        )),
        "PAUSED"
    );
    assert!(asks.lock().unwrap().is_empty());
    device.set_paused(false);
    assert_eq!(
        code(call(
            &device,
            &cp,
            "exec",
            json!({"command": echo("x"), "timeout": 5})
        )),
        "ok"
    );
}

#[test]
fn similar_commands_only_for_simple_safe_ones() {
    let cp = ControlPlane::new();
    let folder = temp_dir("similar-folder");
    std::fs::create_dir_all(folder.join("old")).unwrap();
    let asks = Arc::new(Mutex::new(Vec::new()));
    let device = device_with(
        &temp_dir("similar"),
        &folder,
        recording(Decision::Similar, asks.clone()),
        Arc::new(Terminal),
        &cp,
    );
    let run = |command: String| {
        code(call(
            &device,
            &cp,
            "exec",
            json!({"command": command, "timeout": 10}),
        ))
    };
    assert_eq!(run(echo("one")), "ok");
    assert_eq!(asks.lock().unwrap().len(), 1);
    let first = asks.lock().unwrap()[0].clone();
    assert!(first.family.is_some());
    assert!(!first.destructive);
    assert_eq!(first.who.via, "conversation");
    // The same kind is not asked again in this conversation.
    assert_eq!(run(echo("two")), "ok");
    assert_eq!(asks.lock().unwrap().len(), 1);
    // A compound command is always asked, and has no kind to allow.
    assert_eq!(run(format!("{}; {}", echo("a"), echo("b"))), "ok");
    assert_eq!(asks.lock().unwrap().len(), 2);
    assert!(asks.lock().unwrap()[1].family.is_none());
    // A deleting command is asked every time, marked destructive.
    let delete = if cfg!(windows) {
        format!("Remove-Item '{}' -Recurse", folder.join("old").display())
    } else {
        format!("rm -r '{}'", folder.join("old").display())
    };
    assert_eq!(run(delete.clone()), "ok");
    assert_eq!(run(delete), "ok");
    let all = asks.lock().unwrap();
    assert_eq!(all.len(), 4);
    // Nothing is going on any more: no activity line is left behind.
    let views = device.presence.snapshot(&[]);
    assert!(views.iter().all(|v| v.activity.is_empty()), "{views:?}");
    assert!(all[2].destructive && all[3].destructive);
    assert!(all[2].family.is_none());
}

#[test]
fn a_session_ended_on_the_bar_is_refused_and_its_jobs_die() {
    let cp = ControlPlane::new();
    let folder = temp_dir("end-folder");
    let device = device_with(
        &temp_dir("end"),
        &folder,
        recording(Decision::Once, Arc::new(Mutex::new(Vec::new()))),
        Arc::new(Terminal),
        &cp,
    );
    let started = call(
        &device,
        &cp,
        "exec",
        json!({"command": long_tree(30), "timeout": 0}),
    )
    .unwrap();
    let job = started["job"].as_str().unwrap().to_string();
    assert!(device.presence.glowing(device.jobs.running()));
    device.end_session("ags_test", "test");
    let ended = device.jobs.get(&job).unwrap();
    ended.wait(10.0, &AtomicBool::new(false));
    assert_eq!(ended.view(0)["status"], "killed");
    assert_eq!(
        code(call(&device, &cp, "info", json!({})).map(|_| json!({}))),
        "SESSION_ENDED"
    );
}
