//! The local MCP host against a tiny stdio MCP server (Python, skipped where there is none): start, tool list,
//! a call with text and image content, secrets blanked, restart after a crash, stop.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use agentrouter_device::config::Access;
use agentrouter_device::gate::Scope;
use agentrouter_device::mcp::{McpHost, resolve_command};
use serde_json::json;

fn python() -> Option<String> {
    // A real interpreter (Windows has python3.exe stubs that only open the Store).
    ["python", "python3"]
        .iter()
        .filter_map(|p| resolve_command(p))
        .find(|p| {
            std::process::Command::new(p)
                .args(["-c", "print(1)"])
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .map(|p| p.display().to_string())
}

#[test]
fn without_the_sandbox_a_server_does_not_start_at_all() {
    // #42: every server runs inside the OS sandbox; a device without it refuses, never runs unconfined. (The host's
    // relay behaviour is replaced by the aggregator, which has its own tests.)
    let Some(python) = python() else {
        eprintln!("no python: skipped");
        return;
    };
    let exe = std::env::current_exe().unwrap();
    let dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap()
        .join(format!("ar-mcp-test-{}", std::process::id()));
    let marker = dir.join("ran.txt");
    let host = McpHost::new(&dir.join("data"));
    host.set(true);
    let scope = Scope {
        access: Access::Folders,
        folders: vec![],
        deny: vec![],
    };
    let cancel = AtomicBool::new(false);
    let script = format!("open(r'{}','w').write('ran')", marker.display());
    let start = json!({
        "name": "fake", "command": python, "args": ["-u", "-c", script], "hash": "h1", "waitMs": 5000,
    });
    let refused = host.start(&scope, "s1", &start, &cancel, None).unwrap_err();
    assert_eq!(refused.code, "SANDBOX_UNAVAILABLE", "{refused:?}");
    assert!(refused.next.is_some(), "tells the person what to run");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "nothing ran unconfined");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_command_resolves_to_its_windows_program_not_the_shell_script_beside_it() {
    let dir = std::env::temp_dir().join(format!("ar-mcp-resolve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("npx"), "#!/bin/sh").unwrap();
    std::fs::write(dir.join("npx.cmd"), "@echo off").unwrap();
    let found = resolve_command(&dir.join("npx").display().to_string());
    assert_eq!(found, Some(dir.join("npx.cmd")));
    let _ = std::fs::remove_dir_all(&dir);
}
