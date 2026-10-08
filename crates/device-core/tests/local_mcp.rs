//! The local MCP end to end (LINKED-DEVICES.md §14.3): an AI client's `mcp` front end talks to the
//! running app over the current-user IPC (named pipe / Unix socket) with the token from the token file,
//! and the call goes through the same local gate, confirmation and presence as a cloud request.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentrouter_device::config::Access;
use agentrouter_device::consent::{Ask, Decision};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::local_ipc::{self, ClientInfo, Server};
use agentrouter_device::mcp::LocalHub;
use agentrouter_device::presence::Terminal;
use common::*;
use serde_json::{Value, json};

fn call(client: &mut local_ipc::Client, id: i64, name: &str, args: Value) -> Value {
    let reply = client
        .request(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}))
        .unwrap();
    assert_eq!(reply["id"], id);
    reply["result"].clone()
}

#[test]
fn a_local_ai_uses_this_computer_through_the_local_ipc() {
    // A short home: a Unix socket path must stay under ~100 bytes.
    let home = std::env::temp_dir().join(format!("arm{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this test binary has only this test; nothing else reads the environment concurrently.
    unsafe { std::env::set_var("AGENTROUTER_DEVICE_HOME", &home) };
    let folder = temp_dir("mcp-folder");
    std::fs::write(folder.join("notes.txt"), "你好").unwrap();

    let asks: Arc<Mutex<Vec<Ask>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = asks.clone();
    let device = Arc::new(Device::new(Options {
        data_dir: home.join("data"),
        access: Access::Folders,
        folders: vec![folder.display().to_string()],
        confirm: Arc::new(move |ask: &Ask, _: &AtomicBool| {
            recorded.lock().unwrap().push(ask.clone());
            Decision::Once
        }),
        indicator: Arc::new(Terminal),
        home: home.clone(),
        shell: Shell::default_for_os(),
    }));
    let hub = Arc::new(LocalHub::new(
        device.clone(),
        Arc::new(|| "测试机".to_string()),
    ));

    // Not running yet: the front end cannot connect.
    let me = ClientInfo {
        name: "claude-code".into(),
        version: "2.1.0".into(),
    };
    assert!(local_ipc::Client::connect(&me).is_err());

    let _server = Server::start(hub.clone()).unwrap();
    let mut client = local_ipc::Client::connect(&me).unwrap();
    assert_eq!(hub.clients()[0]["name"], "Claude Code");

    let list = call(&mut client, 1, "list_devices", json!({}));
    let this = &list["structuredContent"]["devices"][0];
    assert_eq!(this["device"], "this");
    assert_eq!(this["name"], "测试机");
    assert_eq!(this["access"], "folders");

    let read = call(
        &mut client,
        2,
        "read_file",
        json!({"device": "this", "path": folder.join("notes.txt").display().to_string()}),
    );
    assert_eq!(read["structuredContent"]["content"], "你好");

    let outside = call(
        &mut client,
        3,
        "read_file",
        json!({"device": "this", "path": home.join("data").join("audit.log").display().to_string()}),
    );
    assert_eq!(outside["isError"], true);

    let ran = call(
        &mut client,
        4,
        "exec",
        json!({"device": "this", "command": echo("from-local-ai"), "timeout": 20}),
    );
    assert!(
        ran["structuredContent"]["output"]
            .as_str()
            .unwrap()
            .contains("from-local-ai"),
        "{ran}"
    );
    {
        let asks = asks.lock().unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].who.via, "local");
        assert_eq!(asks[0].who.name, "Claude Code");
    }
    let sessions = device.presence.snapshot(&[]);
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].session.starts_with("lcl_"));

    let other = call(
        &mut client,
        5,
        "exec",
        json!({"device": "rd_9", "command": echo("x")}),
    );
    assert_eq!(
        other["structuredContent"]["error"]["code"],
        "DEVICE_NOT_FOUND"
    );

    // Pause on the bar: the local AI is told so and nothing is asked.
    device.set_paused(true);
    let paused = call(
        &mut client,
        6,
        "exec",
        json!({"device": "this", "command": echo("x")}),
    );
    assert_eq!(paused["structuredContent"]["error"]["code"], "PAUSED");
    device.set_paused(false);
    assert_eq!(asks.lock().unwrap().len(), 1);

    let done = call(
        &mut client,
        7,
        "disconnect_device",
        json!({"device": "this"}),
    );
    assert_eq!(done["structuredContent"]["ended"], true);
    assert!(device.presence.snapshot(&[]).is_empty());

    // A wrong token is refused.
    let path = local_ipc::token_path();
    let real = std::fs::read_to_string(&path).unwrap();
    let mut forged: Value = serde_json::from_str(&real).unwrap();
    forged["token"] = json!("0".repeat(64));
    std::fs::write(&path, forged.to_string()).unwrap();
    assert!(local_ipc::Client::connect(&me).is_err());
    std::fs::write(&path, real).unwrap();

    // Closing the connection removes the client from the panel.
    drop(client);
    let gone = Arc::new(AtomicUsize::new(0));
    for _ in 0..50 {
        if hub.clients().is_empty() {
            gone.fetch_add(1, Ordering::SeqCst);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(gone.load(Ordering::SeqCst), 1);
}
