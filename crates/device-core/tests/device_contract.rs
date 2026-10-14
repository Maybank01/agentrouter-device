//! The device contract (DEVICE-PROTOCOL.md §5, §18.4): signature checks first, then the local gate, then the
//! action. The device is the link: it serves `info` and the local MCP actions, nothing of its own.

mod common;

use std::sync::atomic::AtomicBool;

use agentrouter_device::config::Access;
use agentrouter_device::util::now_ms;
use common::*;
use serde_json::json;

fn code(r: Result<serde_json::Value, (String, String)>) -> String {
    r.expect_err("expected a refusal").0
}

#[test]
fn signature_checks_come_first() {
    let cp = ControlPlane::new();
    let data = temp_dir("sig");
    let d = device(&data, Access::Full, &[], &cp);
    let args = json!({});
    let cancel = AtomicBool::new(false);

    // Signed by another key.
    let other = ControlPlane::new();
    let mut forged = other.sign("ags_x", "info", &args);
    forged["kid"] = json!(cp.control_key().kid);
    assert_eq!(
        d.serve(&forged, &args, &cancel).unwrap_err().code,
        "SIGNATURE_INVALID"
    );
    // Unknown kid.
    assert_eq!(
        d.serve(&other.sign("ags_x", "info", &args), &args, &cancel)
            .unwrap_err()
            .code,
        "SIGNATURE_INVALID"
    );
    // Another device.
    let wrong = cp.sign_at(
        "lnd_ffffffffffffffffffffffffffffffff",
        "ags_x",
        "info",
        &args,
        now_ms() + 30_000,
    );
    assert_eq!(
        d.serve(&wrong, &args, &cancel).unwrap_err().code,
        "WRONG_DEVICE"
    );
    // Arguments changed after signing.
    let signed = cp.sign(
        "ags_x",
        "mcp_call",
        &json!({"name": "files", "tool": "read_text_file", "arguments": {"path": "a.txt"}}),
    );
    assert_eq!(
        d.serve(
            &signed,
            &json!({"name": "files", "tool": "write_file", "arguments": {"path": "a.txt"}}),
            &cancel
        )
        .unwrap_err()
        .code,
        "SIGNATURE_INVALID"
    );
    // Expired, and living too long.
    let old = cp.sign_at(DEVICE_ID, "ags_x", "info", &args, now_ms() - 1);
    assert_eq!(
        d.serve(&old, &args, &cancel).unwrap_err().code,
        "REQUEST_EXPIRED"
    );
    let long = cp.sign_at(DEVICE_ID, "ags_x", "info", &args, now_ms() + 10 * 60_000);
    assert_eq!(
        d.serve(&long, &args, &cancel).unwrap_err().code,
        "REQUEST_EXPIRED"
    );
    // Replay.
    let once = cp.sign("ags_x", "info", &args);
    assert!(d.serve(&once, &args, &cancel).is_ok());
    assert_eq!(d.serve(&once, &args, &cancel).unwrap_err().code, "REPLAYED");
    // Unknown action (signed): refused after the checks.
    assert_eq!(
        code(call(&d, &cp, "format_disk", json!({}))),
        "UNKNOWN_ACTION"
    );
    // Every one of them is in the audit log, and the chain holds.
    let entries = agentrouter_device::audit::verify(&d.audit_path()).unwrap();
    assert!(entries >= 9, "{entries}");
}

#[test]
fn info_reports_the_local_choices_and_only_the_mcp_actions() {
    let cp = ControlPlane::new();
    let data = temp_dir("info");
    let folder = temp_dir("info-folder");
    let d = device(&data, Access::Folders, &[&folder], &cp);
    let v = call(&d, &cp, "info", json!({})).unwrap();
    assert_eq!(v["device"]["access"], "folders");
    assert_eq!(
        v["device"]["shell"],
        if cfg!(windows) { "powershell" } else { "sh" }
    );
    assert_eq!(v["device"]["folders"].as_array().unwrap().len(), 1);
    assert!(v["jobs"].as_array().unwrap().is_empty());
    // The cloud tells a link-only device by this list (LINKED-DEVICES.md §23.8): MCP actions, no built-in ones.
    let actions: Vec<&str> = v["device"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.as_str())
        .collect();
    assert_eq!(
        actions,
        [
            "info",
            "mcp_start",
            "mcp_stop",
            "mcp_list",
            "mcp_call",
            "mcp_logs",
            "mcp_package"
        ]
    );
}

#[test]
fn the_built_in_file_command_and_screen_actions_are_gone() {
    let cp = ControlPlane::new();
    let data = temp_dir("gone");
    let folder = temp_dir("gone-folder");
    std::fs::write(folder.join("a.txt"), "hello").unwrap();
    let d = device(&data, Access::Full, &[&folder], &cp);
    for (action, args) in [
        ("exec", json!({"command": "echo hi", "timeout": 1})),
        (
            "job",
            json!({"job": "job_x", "action": "output", "timeout": 0}),
        ),
        (
            "read_file",
            json!({"path": folder.join("a.txt").display().to_string()}),
        ),
        (
            "write_file",
            json!({"path": folder.join("b.txt").display().to_string(), "content": "x"}),
        ),
        ("edit_file", json!({})),
        ("apply_patch", json!({})),
        ("search", json!({})),
        ("read_files", json!({})),
        ("list_dir", json!({})),
        ("screenshot", json!({})),
        ("windows", json!({})),
        ("gui", json!({})),
        ("app", json!({})),
        ("screen_record", json!({})),
    ] {
        assert_eq!(
            code(call(&d, &cp, action, args)),
            "UNKNOWN_ACTION",
            "{action}"
        );
    }
    assert!(!folder.join("b.txt").exists());
}
