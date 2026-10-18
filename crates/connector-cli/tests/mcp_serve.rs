//! The device's workspace MCP service as the cloud runs it: a stdio server started with
//! `agentrouter mcp-serve workspace`, `AGENTROUTER_DEVICE_HOME` and `AGENTROUTER_SESSION`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next: u64,
}

impl Client {
    fn start(data: &Path, home: &Path, session: Option<&str>) -> Client {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentrouter"));
        cmd.arg("mcp-serve")
            .arg("workspace")
            .env("AGENTROUTER_DEVICE_HOME", data)
            .env("USERPROFILE", home)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(session) = session {
            cmd.env("AGENTROUTER_SESSION", session);
        }
        let mut child = cmd.spawn().expect("spawn the workspace MCP service");
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let mut client = Client {
            child,
            stdin,
            out,
            next: 1,
        };
        let init = client.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}));
        assert_eq!(
            init["result"]["serverInfo"]["name"],
            "agentrouter-workspace"
        );
        client
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
        let mut answer = String::new();
        self.out.read_line(&mut answer).unwrap();
        let value: Value = serde_json::from_str(&answer).expect("one JSON-RPC response line");
        assert_eq!(value["id"], id, "{value}");
        value
    }

    fn tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
        let answer = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let is_error = answer["result"]["isError"].as_bool().unwrap_or(false);
        let text = answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        (is_error, text)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ar-mcp-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    agentrouter_device::gate::display(&std::fs::canonicalize(dir).unwrap()).into()
}

#[test]
fn tools_are_listed_with_schemas() {
    let data = temp("list-data");
    let home = temp("list-home");
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let answer = client.request("tools/list", json!({}));
    let names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "exec",
        "workspace_create",
        "worktree_create",
        "workspace_list",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
}

#[test]
fn workspace_worktree_and_exec_run_inside_the_worktree() {
    let data = temp("run-data");
    let home = temp("run-home");
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "Mod Arena"}));
    assert!(!error, "{text}");
    assert!(text.contains("Workspaces"), "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod-arena"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = created["worktreeDir"].as_str().unwrap().to_string();
    // Idempotent: the same session gets the same worktree.
    let (error, text) = client.tool("worktree_create", json!({"project": "mod-arena"}));
    assert!(!error, "{text}");
    let again: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(again["worktreeDir"], created["worktreeDir"]);
    assert_eq!(again["reused"], true);
    // exec runs in the session worktree by default.
    let print_cwd = if cfg!(windows) {
        "(Get-Location).Path"
    } else {
        "pwd"
    };
    let (error, text) = client.tool("exec", json!({"command": print_cwd}));
    assert!(!error, "{text}");
    assert!(text.contains(&worktree), "cwd {worktree} not in {text}");
    // Exit codes are reported.
    let (error, text) = client.tool("exec", json!({"command": "exit 3"}));
    assert!(error, "{text}");
    assert!(text.contains("exit_code: 3"), "{text}");
    // Timeouts end the command.
    let (error, text) = client.tool(
        "exec",
        json!({"command": if cfg!(windows) { "Start-Sleep -Seconds 5" } else { "sleep 5" }, "timeout_ms": 500}),
    );
    assert!(error, "{text}");
    assert!(text.contains("timed_out: true"), "{text}");
}

#[test]
fn game_directories_and_junctions_are_refused() {
    let data = temp("gate-data");
    let home = temp("gate-home");
    let game = temp("gate-game");
    std::fs::write(
        data.join("config.json"),
        json!({"access": "folders", "folders": [game]}).to_string(),
    )
    .unwrap();
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, _) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error);
    // The linked game folder is never a cwd.
    let (error, text) = client.tool(
        "exec",
        json!({"command": "echo nope", "cwd": game.display().to_string()}),
    );
    assert!(error, "{text}");
    assert!(text.contains("WORKSPACE_CWD_INVALID"), "{text}");
    // A junction inside the workspace that resolves to the game folder is refused too.
    let (_, text) = client.tool("project_list", json!({}));
    let registry: Value = serde_json::from_str(&text).unwrap();
    let project = registry["projects"][0]["path"].as_str().unwrap();
    let escape = PathBuf::from(project).join("escape");
    #[cfg(windows)]
    let linked = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&escape)
        .arg(&game)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&game, &escape).is_ok();
    if linked {
        let (error, text) = client.tool(
            "exec",
            json!({"command": "echo nope", "cwd": escape.display().to_string()}),
        );
        assert!(error, "{text}");
    }
}

#[test]
fn catastrophic_commands_are_denied() {
    let data = temp("deny-data");
    let home = temp("deny-home");
    let mut client = Client::start(&data, &home, Some("ags_test"));
    client.tool("workspace_create", json!({"name": "mod"}));
    client.tool("worktree_create", json!({"project": "mod"}));
    let (error, text) = client.tool("exec", json!({"command": "format C: /y"}));
    assert!(error, "{text}");
    assert!(text.contains("DENIED") || text.contains("拒绝"), "{text}");
}

#[test]
fn worktree_create_needs_a_session() {
    let data = temp("nosession-data");
    let home = temp("nosession-home");
    let mut client = Client::start(&data, &home, None);
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(error, "{text}");
    assert!(text.contains("WORKSPACE_SESSION_INVALID"), "{text}");
}
