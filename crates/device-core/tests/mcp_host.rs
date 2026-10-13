//! The local MCP host against a tiny stdio MCP server (Python, skipped where there is none): start, tool list,
//! a call with text and image content, secrets blanked, restart after a crash, stop.

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use agentrouter_device::config::Access;
use agentrouter_device::gate::Scope;
use agentrouter_device::mcp::{McpHost, resolve_command};
use serde_json::{Value, json};

const SERVER: &str = r#"
import sys, json, os
for line in sys.stdin:
    m = json.loads(line)
    i, method = m.get("id"), m.get("method")
    if i is None:
        continue
    if method == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        r = {"tools": [{"name": "echo", "description": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                       {"name": "die", "description": "exit", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        name = m["params"]["name"]
        if name == "die":
            sys.exit(3)
        text = m["params"]["arguments"].get("text", "") + " " + os.environ.get("FAKE_KEY", "")
        r = {"content": [{"type": "text", "text": text}, {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": i, "result": r}), flush=True)
"#;

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

fn wait_state(host: &McpHost, name: &str, state: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = host.list().unwrap();
        let server = list["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .cloned()
            .unwrap_or(Value::Null);
        if server["state"] == state || Instant::now() > deadline {
            return server;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_stdio_server_starts_answers_restarts_and_stops() {
    let Some(python) = python() else {
        eprintln!("no python: skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("ar-mcp-test-{}", std::process::id()));
    let host = McpHost::new(&dir);
    host.set(true);
    let scope = Scope {
        access: Access::Folders,
        folders: vec![],
        deny: vec![],
    };
    let cancel = AtomicBool::new(false);
    let start = json!({
        "name": "fake", "command": python, "args": ["-u", "-c", SERVER],
        "env": {"FAKE_KEY": "very-secret-1"}, "secretEnv": ["FAKE_KEY"], "hash": "h1", "waitMs": 20000,
    });
    let started = host.start(&scope, &start, &cancel).unwrap();
    assert_eq!(started["state"], "ready", "{started}");
    assert_eq!(started["tools"].as_array().unwrap().len(), 2);
    assert!(!started.to_string().contains("very-secret-1"));

    let call = |tool: &str| {
        host.call(
            &scope,
            &json!({"name": "fake", "tool": tool, "arguments": {"text": "hi"}, "hash": "h1", "timeoutMs": 10000}),
            &cancel,
        )
    };
    let answer = call("echo").unwrap();
    assert_eq!(answer["content"][0]["text"], "hi [secret]");
    assert_eq!(answer["content"][1]["type"], "image");

    // A crash: the call fails, the host starts it again.
    assert!(call("die").is_err());
    let again = wait_state(&host, "fake", "ready");
    assert_eq!(again["restarts"], 1, "{again}");
    assert_eq!(call("echo").unwrap()["content"][0]["text"], "hi [secret]");

    // Unknown server, then stop.
    let missing = host
        .call(&scope, &json!({"name": "nope", "tool": "echo"}), &cancel)
        .unwrap_err();
    assert_eq!(missing.code, "MCP_NOT_RUNNING");
    assert_eq!(
        host.stop(&json!({"name": "fake"})).unwrap()["stopped"],
        true
    );
    assert!(
        host.list().unwrap()["servers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    host.set(false);
    let _ = std::fs::remove_dir_all(&dir);
}
