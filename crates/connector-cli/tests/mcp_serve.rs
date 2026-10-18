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
        Self::start_with(data, home, session, &[])
    }

    fn start_with(
        data: &Path,
        home: &Path,
        session: Option<&str>,
        envs: &[(&str, &str)],
    ) -> Client {
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
        for (key, value) in envs {
            cmd.env(key, value);
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

fn config(data: &Path, unattended: bool, folders: &[&Path]) {
    std::fs::write(
        data.join("config.json"),
        serde_json::to_string(&json!({
            "access": "folders", "unattended": unattended,
            "folders": folders.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_artifact(client: &mut Client, worktree: &Path) -> PathBuf {
    let command = if cfg!(windows) {
        "Set-Content -Path out.txt -Value new-mod"
    } else {
        "echo new-mod > out.txt"
    };
    let (error, text) = client.tool("exec", json!({"command": command}));
    assert!(!error, "{text}");
    worktree.join("out.txt")
}

#[test]
fn deploy_backs_up_lists_and_rolls_back() {
    let data = temp("dep-data");
    let home = temp("dep-home");
    let target = temp("dep-target");
    std::fs::create_dir_all(target.join("mods")).unwrap();
    std::fs::write(target.join("mods/out.txt"), b"old-mod").unwrap();
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    agentrouter_device::deploy::add_target(&data, "game", target.to_str().unwrap()).unwrap();
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    let artifact = write_artifact(&mut client, &worktree);
    // The registered target is visible to the Agent.
    let (error, text) = client.tool("deploy_targets", json!({}));
    assert!(!error, "{text}");
    assert!(
        text.contains("game") && text.contains("ar-mcp-dep-target"),
        "{text}"
    );
    // Deploy: backed up, recorded, target replaced.
    let (error, text) = client.tool(
        "deploy",
        json!({
            "project": "mod", "target": "game", "message": "first mod",
            "artifacts": [{"source": artifact.display().to_string(), "dest": "mods/out.txt"}],
        }),
    );
    assert!(!error, "{text}");
    let summary: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(summary["status"], "applied");
    let id = summary["id"].as_str().unwrap().to_string();
    assert!(
        std::fs::read_to_string(target.join("mods/out.txt"))
            .unwrap()
            .contains("new-mod")
    );
    let backup = data
        .join("deployments")
        .join(&id)
        .join("backup/mods/out.txt");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), "old-mod");
    // Listed, then rolled back.
    let (error, text) = client.tool("deploy_list", json!({}));
    assert!(!error, "{text}");
    assert!(text.contains(&id) && text.contains("applied"), "{text}");
    let (error, text) = client.tool("deploy_rollback", json!({"id": id}));
    assert!(!error, "{text}");
    assert_eq!(
        std::fs::read_to_string(target.join("mods/out.txt")).unwrap(),
        "old-mod"
    );
    let (error, text) = client.tool("deploy_list", json!({}));
    assert!(!error, "{text}");
    assert!(text.contains("rolled_back"), "{text}");
    // The connector-side audit would carry the target; the service-side log has the entry.
    let audit = std::fs::read_to_string(data.join("mcp-audit.log")).unwrap();
    assert!(
        audit.contains("\"tool\":\"deploy\"") && audit.contains("\"tool\":\"deploy_rollback\""),
        "{audit}"
    );
}

#[test]
fn deploy_refuses_unknown_targets_outside_artifacts_and_denied_consent() {
    let data = temp("dep-refuse-data");
    let home = temp("dep-refuse-home");
    let target = temp("dep-refuse-target");
    std::fs::write(target.join("old.bin"), b"old").unwrap();
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    agentrouter_device::deploy::add_target(&data, "game", target.to_str().unwrap()).unwrap();
    let mut client = Client::start(&data, &home, Some("ags_test"));
    client.tool("workspace_create", json!({"name": "mod"}));
    client.tool("worktree_create", json!({"project": "mod"}));
    let (error, text) = client.tool(
        "deploy",
        json!({"project": "mod", "target": "nope", "artifacts": [{"source": "out.txt", "dest": "out.txt"}]}),
    );
    assert!(error, "{text}");
    assert!(text.contains("DEPLOY_TARGET_UNKNOWN"), "{text}");
    // An artifact outside the workspace is refused.
    let outside = temp("dep-outside");
    std::fs::write(outside.join("x.bin"), b"x").unwrap();
    let (error, text) = client.tool(
        "deploy",
        json!({"project": "mod", "target": "game", "artifacts": [{"source": outside.join("x.bin").display().to_string(), "dest": "x.bin"}]}),
    );
    assert!(error, "{text}");
    assert!(
        text.contains("DEPLOY_ARTIFACT_INVALID") || text.contains("DENIED"),
        "{text}"
    );
    // Denied local approval.
    config(&data, false, &[]);
    let mut denied = Client::start_with(
        &data,
        &home,
        Some("ags_test"),
        &[("AGENTROUTER_CONSENT", "deny")],
    );
    denied.tool("workspace_create", json!({"name": "mod"}));
    let (error, text) = denied.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    let artifact = write_artifact(&mut denied, &worktree);
    let (error, text) = denied.tool(
        "deploy",
        json!({"project": "mod", "target": "game", "artifacts": [{"source": artifact.display().to_string(), "dest": "x.bin"}]}),
    );
    assert!(error, "{text}");
    assert!(
        text.contains("DENIED") || text.contains("没有批准"),
        "{text}"
    );
    assert_eq!(std::fs::read(target.join("old.bin")).unwrap(), b"old");
}

#[test]
fn confined_exec_writes_inside_but_not_outside_on_windows() {
    let data = temp("conf-data");
    let home = temp("conf-home");
    let outside = temp("conf-outside");
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    // Writing inside the worktree keeps working under the boundary.
    let inside_cmd = if cfg!(windows) {
        "Set-Content -Path inside.txt -Value ok"
    } else {
        "echo ok > inside.txt"
    };
    let (error, text) = client.tool("exec", json!({"command": inside_cmd}));
    assert!(!error, "{text}");
    assert!(worktree.join("inside.txt").is_file(), "{text}");
    // The command still runs at all (echo), so a refusal is about the path, not the shell.
    let (error, text) = client.tool("exec", json!({"command": "echo ok"}));
    assert!(!error, "{text}");
    assert!(text.contains("exit_code: 0"), "{text}");
    #[cfg(windows)]
    {
        assert!(agentrouter_device::confine::labelled(&worktree));
        let escape = outside.join("escape.txt");
        let cmd = format!(
            "$ErrorActionPreference='Stop'; Set-Content -Path '{}' -Value bad",
            escape.display()
        );
        let (error, text) = client.tool("exec", json!({"command": cmd}));
        assert!(
            !escape.exists(),
            "low-integrity exec wrote outside the workspace: {text}"
        );
        assert!(error, "the refused write should fail the command: {text}");
    }
    #[cfg(not(windows))]
    {
        // Unix: no portable OS write boundary yet (documented); the gate/deny/audit still apply.
        let _ = outside;
    }
}
