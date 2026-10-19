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
            "access": "folders", "unattended": unattended, "headless": true,
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
        // A cloud-style "allow" in the environment approves nothing (#43); headless config says no.
        &[("AGENTROUTER_CONSENT", "allow")],
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

#[cfg(windows)]
#[test]
fn exec_keeps_windows_powershell_5_cmdlets_with_a_pwsh7_module_path() {
    let data = temp("ps51-data");
    let home = temp("ps51-home");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.json"),
        json!({"access": "folders", "unattended": true}).to_string(),
    )
    .unwrap();
    // What a connector started from `pwsh` 7 inherits: PowerShell 7's own $PSHOME\Modules shadows
    // Microsoft.PowerShell.Utility for a Windows PowerShell 5.1 child.
    let polluted = r"C:\Users\maybe\Documents\PowerShell\Modules;C:\Program Files\PowerShell\Modules;c:\program files\powershell\7\Modules;C:\Program Files\WindowsPowerShell\Modules;C:\WINDOWS\system32\WindowsPowerShell\v1.0\Modules";
    let mut client = Client::start_with(
        &data,
        &home,
        Some("ags_test"),
        &[("PSModulePath", polluted)],
    );
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool(
        "exec",
        json!({"command": "powershell.exe -NoProfile -Command \"if (Get-Command Get-FileHash -ErrorAction SilentlyContinue) { 'FOUND' } else { 'MISSING' }\""}),
    );
    assert!(!error, "{text}");
    assert!(text.contains("FOUND"), "{text}");
    let (error, text) = client.tool(
        "exec",
        json!({"command": "powershell.exe -NoProfile -Command \"Get-FileHash C:\\Windows\\win.ini -Algorithm SHA256 | Select-Object -ExpandProperty Hash\""}),
    );
    assert!(!error, "{text}");
    assert!(
        text.to_ascii_uppercase()
            .contains("6B3D6E268DCB76E175A7DB3D9E031349AB2C32654C7E57581A851E64DD6214AB"),
        "{text}"
    );
}

#[cfg(windows)]
#[test]
fn exec_prefers_powershell_7_when_installed() {
    if agentrouter_device::shell::find_pwsh().is_none() {
        eprintln!("skipping: PowerShell 7 (pwsh.exe) is not installed on this runner");
        return;
    }
    let data = temp("pwsh7-data");
    let home = temp("pwsh7-home");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.json"),
        json!({"access": "folders", "unattended": true}).to_string(),
    )
    .unwrap();
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool(
        "exec",
        json!({"command": "$PSVersionTable.PSVersion.Major"}),
    );
    assert!(!error, "{text}");
    assert!(
        text.lines().any(|line| line.trim() == "7"),
        "expected PowerShell 7 major version in exec output: {text}"
    );
}

impl Client {
    /// The local Agent entry: `agentrouter mcp-serve local` with a self-reported identity.
    fn start_local(
        data: &Path,
        home: &Path,
        agent: Option<&str>,
        local_session: Option<&str>,
        envs: &[(&str, &str)],
    ) -> Client {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentrouter"));
        cmd.arg("mcp-serve")
            .arg("local")
            .env("AGENTROUTER_DEVICE_HOME", data)
            .env("USERPROFILE", home)
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(agent) = agent {
            cmd.env("AGENTROUTER_LOCAL_AGENT", agent);
        }
        if let Some(session) = local_session {
            cmd.env("AGENTROUTER_LOCAL_SESSION", session);
        }
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn the local MCP service");
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
}

#[test]
fn local_entry_registers_identity_and_exposes_the_workspace_tools() {
    let data = temp("local-data");
    let home = temp("local-home");
    std::fs::create_dir_all(&data).unwrap();
    config_access(&data, "full", true);
    let mut client = Client::start_local(&data, &home, Some("dsh"), Some("conv-1"), &[]);
    let answer = client.request("tools/list", json!({}));
    let names: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "exec",
        "workspace_create",
        "worktree_create",
        "deploy",
        "deploy_targets",
        "deploy_target_add",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = created["worktreeDir"].as_str().unwrap().to_string();
    assert!(worktree.contains("lses_conv-1"), "{worktree}");
    let (error, text) = client.tool("exec", json!({"command": "echo ok"}));
    assert!(!error, "{text}");
    // Full-access tools (deploy target management) are reachable through the local entry too.
    let target = temp("local-target");
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "game", "path": target.display().to_string()}),
    );
    assert!(!error, "{text}");
    let (error, text) = client.tool("deploy_target_list", json!({}));
    assert!(!error, "{text}");
    assert!(text.contains("game"), "{text}");
    let (error, text) = client.tool("deploy_target_remove", json!({"name": "game"}));
    assert!(!error, "{text}");
    // The registry lists the first-seen pair.
    let registry = std::fs::read_to_string(data.join("local-agents.json")).unwrap();
    assert!(
        registry.contains("lag_dsh") && registry.contains("lses_conv-1"),
        "{registry}"
    );
    // Both audit surfaces carry the caller; the connector chain stays valid.
    let mcp_audit = std::fs::read_to_string(data.join("mcp-audit.log")).unwrap();
    assert!(
        mcp_audit.contains("\"caller\"") && mcp_audit.contains("lag_dsh"),
        "{mcp_audit}"
    );
    assert!(mcp_audit.contains("lses_conv-1"), "{mcp_audit}");
    let audit = std::fs::read_to_string(data.join("audit.log")).unwrap();
    assert!(
        audit.contains("local_mcp_call") && audit.contains("lag_dsh"),
        "{audit}"
    );
    assert_eq!(
        agentrouter_device::audit::verify(&data.join("audit.log")),
        Ok(7)
    );
    // `agentrouter agent list` shows it.
    let output = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .args(["agent", "list"])
        .env("AGENTROUTER_DEVICE_HOME", &data)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lag_dsh") && stdout.contains("lses_conv-1"),
        "{stdout}"
    );
}

#[test]
fn two_local_sessions_are_isolated_and_the_project_lock_serialises_exec() {
    let data = temp("local-two-data");
    let home = temp("local-two-home");
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    let mut first = Client::start_local(&data, &home, Some("dsh"), Some("session-a"), &[]);
    let mut second = Client::start_local(&data, &home, Some("dsh"), Some("session-b"), &[]);
    let (error, text) = first.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = first.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let a: Value = serde_json::from_str(&text).unwrap();
    let (error, text) = second.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let b: Value = serde_json::from_str(&text).unwrap();
    assert_ne!(a["worktreeDir"], b["worktreeDir"]);
    assert!(
        a["worktreeDir"]
            .as_str()
            .unwrap()
            .contains("lses_session-a")
    );
    assert!(
        b["worktreeDir"]
            .as_str()
            .unwrap()
            .contains("lses_session-b")
    );
    let print_cwd = if cfg!(windows) {
        "(Get-Location).Path"
    } else {
        "pwd"
    };
    let (_, text) = first.tool("exec", json!({"command": print_cwd}));
    assert!(text.contains("lses_session-a"), "{text}");
    let (_, text) = second.tool("exec", json!({"command": print_cwd}));
    assert!(text.contains("lses_session-b"), "{text}");
    // Hold the project lock from the test process: the next exec must report LOCAL_SESSION_BUSY.
    let lock_path = data.join("locks").join("project-mod.lock");
    let held = agentrouter_device::lockfile::acquire(&lock_path, std::time::Duration::from_secs(1))
        .unwrap();
    let mut short_wait = Client::start_local(
        &data,
        &home,
        Some("dsh"),
        Some("session-a"),
        &[("AGENTROUTER_LOCK_TIMEOUT_MS", "100")],
    );
    let (error, text) = short_wait.tool("exec", json!({"command": "echo busy"}));
    assert!(error, "{text}");
    assert!(text.contains("LOCAL_SESSION_BUSY"), "{text}");
    drop(held);
    let (error, text) = short_wait.tool("exec", json!({"command": "echo free"}));
    assert!(!error, "{text}");
}

#[test]
fn local_mode_without_environment_still_works() {
    let data = temp("local-none-data");
    let home = temp("local-none-home");
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    let mut client = Client::start_local(&data, &home, None, None, &[]);
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    assert!(
        created["worktreeDir"]
            .as_str()
            .unwrap()
            .contains("lses_default"),
        "{created}"
    );
    let mcp_audit = std::fs::read_to_string(data.join("mcp-audit.log")).unwrap();
    assert!(
        mcp_audit.contains("lag_local") && mcp_audit.contains("lses_default"),
        "{mcp_audit}"
    );
}

fn config_access(data: &Path, access: &str, unattended: bool) {
    std::fs::write(
        data.join("config.json"),
        json!({"access": access, "unattended": unattended, "headless": true}).to_string(),
    )
    .unwrap();
}

#[test]
fn full_access_agent_can_register_targets_but_deploy_still_needs_approval() {
    let data = temp("dt-full-data");
    let home = temp("dt-full-home");
    let target = temp("dt-full-target");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    config_access(&data, "full", false);
    // A denied local approval: registering a target never approves a deployment.
    let mut client = Client::start_with(
        &data,
        &home,
        Some("ags_test"),
        // A cloud-style "allow" in the environment approves nothing (#43); headless config says no.
        &[("AGENTROUTER_CONSENT", "allow")],
    );
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("worktree_create", json!({"project": "mod"}));
    assert!(!error, "{text}");
    let created: Value = serde_json::from_str(&text).unwrap();
    let worktree = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    std::fs::create_dir_all(worktree.join("out")).unwrap();
    std::fs::write(worktree.join("out/mod.dll"), b"new-mod").unwrap();
    let artifact = worktree.join("out/mod.dll");
    // Full access: the Agent registers/lists/removes the target itself.
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "game", "path": target.display().to_string()}),
    );
    assert!(!error, "{text}");
    assert!(
        text.contains("game") && text.contains("dt-full-target"),
        "{text}"
    );
    let (error, text) = client.tool("deploy_target_list", json!({}));
    assert!(!error, "{text}");
    assert!(text.contains("game"), "{text}");
    // Registration is not a deployment: the L2 approval still decides.
    let (error, text) = client.tool(
        "deploy",
        json!({"project": "mod", "target": "game",
            "artifacts": [{"source": artifact.display().to_string(), "dest": "mods/mod.dll"}]}),
    );
    assert!(error, "{text}");
    assert!(
        text.contains("DENIED") || text.contains("没有批准"),
        "{text}"
    );
    assert!(!target.join("mods/mod.dll").exists());
    let (error, text) = client.tool("deploy_target_remove", json!({"name": "game"}));
    assert!(!error, "{text}");
    let (error, text) = client.tool("deploy_target_list", json!({}));
    assert!(!error, "{text}");
    assert!(!text.contains("game"), "{text}");
}

#[test]
fn folders_access_cannot_register_deploy_targets() {
    let data = temp("dt-folders-data");
    let home = temp("dt-folders-home");
    let target = temp("dt-folders-target");
    std::fs::create_dir_all(&data).unwrap();
    config(&data, true, &[]);
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "game", "path": target.display().to_string()}),
    );
    assert!(error, "{text}");
    assert!(text.contains("DEPLOY_TARGET_OWNER_ONLY"), "{text}");
    let (error, text) = client.tool("deploy_target_list", json!({}));
    assert!(error, "{text}");
    assert!(text.contains("DEPLOY_TARGET_OWNER_ONLY"), "{text}");
    let (error, text) = client.tool("deploy_target_remove", json!({"name": "game"}));
    assert!(error, "{text}");
    assert!(text.contains("DEPLOY_TARGET_OWNER_ONLY"), "{text}");
}

#[test]
fn full_access_target_registration_validates_paths() {
    let data = temp("dt-valid-data");
    let home = temp("dt-valid-home");
    std::fs::create_dir_all(&data).unwrap();
    config_access(&data, "full", true);
    let mut client = Client::start(&data, &home, Some("ags_test"));
    let (error, text) = client.tool("workspace_create", json!({"name": "mod"}));
    assert!(!error, "{text}");
    // A directory that does not exist.
    let missing = data.join("does-not-exist");
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "missing", "path": missing.display().to_string()}),
    );
    assert!(error, "{text}");
    // The workspace root itself is never a deploy target.
    let root = home.join("Workspaces");
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "root", "path": root.display().to_string()}),
    );
    assert!(error, "{text}");
    assert!(text.contains("DENIED") || text.contains("重叠"), "{text}");
    // The device data folder is never a deploy target either.
    let (error, text) = client.tool(
        "deploy_target_add",
        json!({"name": "data", "path": data.display().to_string()}),
    );
    assert!(error, "{text}");
}
