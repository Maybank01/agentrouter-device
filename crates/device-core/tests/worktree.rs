//! M1 of the workspace model (workspace spec §2–4): git baseline, worktrees and the per-session cwd,
//! through the signed device actions. Git is required for these cases (the device feature needs it);
//! the fake MCP server case is skipped where python is absent, like `mcp_host.rs`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use agentrouter_device::checkpoint::{self, Change};
use agentrouter_device::config::Access;
use agentrouter_device::workspace;
use agentrouter_device::worktree;
use common::{ControlPlane, call, device_workspaces, temp_dir};
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
        r = {"tools": [{"name": "probe", "description": "cwd and env", "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        r = {"content": [{"type": "text", "text": os.getcwd() + "|" + os.environ.get("AR_EXPECT", "")}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": i, "result": r}), flush=True)
"#;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn git_commit(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git commit");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn git_ready() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn python() -> Option<PathBuf> {
    ["python", "python3"]
        .iter()
        .filter_map(|p| {
            Command::new(p)
                .args(["-c", "print(1)"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| PathBuf::from(p))
        })
        .next()
}

fn ensure_project(d: &agentrouter_device::device::Device, cp: &ControlPlane, name: &str) -> Value {
    call(d, cp, "workspace_ensure", json!({"project": name})).unwrap()
}

fn worktree_create(
    d: &agentrouter_device::device::Device,
    cp: &ControlPlane,
    project: &str,
    session: &str,
) -> Value {
    call(
        d,
        cp,
        "workspace_worktree_create",
        json!({"project": project, "session": session}),
    )
    .unwrap()
}

#[test]
fn a_project_without_git_gets_a_baseline_and_a_worktree() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let cp = ControlPlane::new();
    let data = temp_dir("m1-baseline");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    let project = PathBuf::from(
        ensure_project(&d, &cp, "Mod Arena")["projectDir"]
            .as_str()
            .unwrap(),
    );
    // No repository yet: the baseline creates one and one commit.
    assert!(!project.join(".git").exists());
    let created = worktree_create(&d, &cp, "mod-arena", "s1");
    assert_eq!(created["reused"], false);
    assert!(!created["baseCommit"].as_str().unwrap().is_empty());
    assert!(project.join(".git").exists());
    assert_eq!(git_ok(&project, &["rev-list", "--count", "HEAD"]), "1");
    let gitignore = std::fs::read_to_string(project.join(".gitignore")).unwrap();
    assert!(gitignore.contains("node_modules/"), "{gitignore}");
    assert!(gitignore.contains("*.pem"), "{gitignore}");
    let worktree_dir = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    assert_eq!(
        worktree_dir,
        project.join(".worktrees").join("s1"),
        "worktree under .worktrees/<session>"
    );
    assert!(worktree_dir.is_dir());
    assert_eq!(created["branch"], "ar/s1");
    // The worktrees folder never shows up in the repository's status.
    let status = git_ok(&project, &["status", "--porcelain"]);
    assert!(!status.contains(".worktrees"), "{status}");
    // Idempotent: the same session returns the same worktree without a second one.
    let again = worktree_create(&d, &cp, "mod-arena", "s1");
    assert_eq!(again["reused"], true);
    assert_eq!(again["worktreeDir"], created["worktreeDir"]);
    let list = call(&d, &cp, "workspace_worktree_list", json!({})).unwrap();
    assert_eq!(list["worktrees"].as_array().unwrap().len(), 1);
    assert_eq!(list["worktrees"][0]["exists"], true);
}

#[test]
fn an_existing_repository_is_used_read_only_as_the_base() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let cp = ControlPlane::new();
    let data = temp_dir("m1-existing");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    let project = PathBuf::from(
        ensure_project(&d, &cp, "Mod Arena")["projectDir"]
            .as_str()
            .unwrap(),
    );
    git_ok(&project, &["init"]);
    std::fs::write(project.join("main.txt"), "user content\n").unwrap();
    git_ok(&project, &["add", "-A"]);
    git_commit(&project, &["commit", "-m", "user baseline"]);
    let base = git_ok(&project, &["rev-parse", "HEAD"]);
    let before = git_ok(&project, &["config", "--local", "--list"]);
    let created = worktree_create(&d, &cp, "mod-arena", "s2");
    assert_eq!(created["baseCommit"], base);
    assert_eq!(
        git_ok(&project, &["rev-parse", "HEAD"]),
        base,
        "no new commit"
    );
    assert_eq!(git_ok(&project, &["rev-list", "--count", "HEAD"]), "1");
    let after = git_ok(&project, &["config", "--local", "--list"]);
    assert_eq!(before, after, "the person's repository config is untouched");
    assert!(git_ok(&project, &["remote", "-v"]).is_empty());
}

#[test]
fn two_sessions_get_independent_worktrees() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let cp = ControlPlane::new();
    let data = temp_dir("m1-two");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    ensure_project(&d, &cp, "Mod Arena");
    let one = worktree_create(&d, &cp, "mod-arena", "s1");
    let two = worktree_create(&d, &cp, "mod-arena", "s2");
    assert_ne!(one["worktreeDir"], two["worktreeDir"]);
    let one_dir = PathBuf::from(one["worktreeDir"].as_str().unwrap());
    let two_dir = PathBuf::from(two["worktreeDir"].as_str().unwrap());
    std::fs::write(one_dir.join("only-s1.txt"), "s1\n").unwrap();
    git_ok(&one_dir, &["add", "-A"]);
    git_commit(&one_dir, &["commit", "-m", "s1 only"]);
    assert!(one_dir.join("only-s1.txt").is_file());
    assert!(!two_dir.join("only-s1.txt").exists());
    assert_eq!(git_ok(&two_dir, &["status", "--porcelain"]), "");
    let list = call(
        &d,
        &cp,
        "workspace_worktree_list",
        json!({"project": "mod-arena"}),
    )
    .unwrap();
    assert_eq!(list["worktrees"].as_array().unwrap().len(), 2);
}

#[test]
fn remove_keeps_the_branch_and_prune_drops_stale_directories() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let cp = ControlPlane::new();
    let data = temp_dir("m1-remove");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    let project = PathBuf::from(
        ensure_project(&d, &cp, "Mod Arena")["projectDir"]
            .as_str()
            .unwrap(),
    );
    let created = worktree_create(&d, &cp, "mod-arena", "s1");
    let dir = PathBuf::from(created["worktreeDir"].as_str().unwrap());
    let removed = call(
        &d,
        &cp,
        "workspace_worktree_remove",
        json!({"project": "mod-arena", "session": "s1"}),
    )
    .unwrap();
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["branchKept"], true);
    assert!(!dir.exists());
    assert_eq!(
        git_ok(&project, &["rev-parse", "--verify", "ar/s1"]),
        created["baseCommit"].as_str().unwrap()
    );
    // Removing again is a no-op (idempotent).
    let again = call(
        &d,
        &cp,
        "workspace_worktree_remove",
        json!({"project": "mod-arena", "session": "s1"}),
    )
    .unwrap();
    assert_eq!(again["removed"], false);
    // A worktree directory that vanished is pruned, with its ledger entry.
    worktree_create(&d, &cp, "mod-arena", "s2");
    let s2 = PathBuf::from(
        call(&d, &cp, "workspace_worktree_list", json!({})).unwrap()["worktrees"][0]["path"]
            .as_str()
            .unwrap(),
    );
    std::fs::remove_dir_all(&s2).unwrap();
    let pruned = call(
        &d,
        &cp,
        "workspace_worktree_prune",
        json!({"project": "mod-arena", "olderThanHours": 0}),
    )
    .unwrap();
    assert_eq!(pruned["removed"].as_array().unwrap().len(), 1);
    let list = call(&d, &cp, "workspace_worktree_list", json!({})).unwrap();
    assert!(list["worktrees"].as_array().unwrap().is_empty());
}

#[test]
fn consecutive_worktree_creates_are_safe() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let data = temp_dir("m1-concurrent");
    let home = temp_dir("m1-concurrent-home");
    let enforced = workspace::ensure_project(&data, &home, &[], "Mod Arena", None).unwrap();
    assert!(PathBuf::from(&enforced.project_dir).is_dir());
    // Four sessions are created from four threads: the project lock and git's own locks keep the
    // base repository consistent.
    let results: Vec<Result<Value, String>> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for session in ["s1", "s2", "s3", "s4"] {
            let data = data.clone();
            handles.push(scope.spawn(move || {
                worktree::create(&data, "mod-arena", session).map_err(|e| e.code.to_string())
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in &results {
        assert!(result.is_ok(), "{result:?}");
    }
    let list = worktree::list(&data, Some("mod-arena")).unwrap();
    assert_eq!(list["worktrees"].as_array().unwrap().len(), 4);
}

#[test]
fn mcp_start_runs_in_the_session_worktree() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let Some(python) = python() else {
        eprintln!("no python; skipping the fake MCP server case");
        return;
    };
    let cp = ControlPlane::new();
    let data = temp_dir("m1-cwd");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    d.set_mcp(true);
    ensure_project(&d, &cp, "Mod Arena");
    let created = worktree_create(&d, &cp, "mod-arena", "ags_test");
    let worktree_dir = created["worktreeDir"].as_str().unwrap().to_string();
    let start = call(
        &d,
        &cp,
        "mcp_start",
        json!({
            "name": "probe",
            "command": python.display().to_string(),
            "args": ["-c", SERVER],
            "worktree": true,
            "env": {"AR_EXPECT": "{{worktree}}"},
            "waitMs": 60_000,
        }),
    )
    .unwrap_err();
    // #42: a server never starts outside the OS sandbox (the worktree path itself goes with #49).
    assert_eq!(start.0, "SANDBOX_UNAVAILABLE", "{start:?}");
    let _ = worktree_dir;
}

#[test]
fn checkpoints_and_undo_work_inside_a_worktree() {
    if !git_ready() {
        eprintln!("git is not installed; skipping");
        return;
    }
    let data = temp_dir("m1-cp-data");
    let home = temp_dir("m1-cp-home");
    workspace::ensure_project(&data, &home, &[], "Mod", None).unwrap();
    let one = worktree::create(&data, "mod", "s1").unwrap();
    let two = worktree::create(&data, "mod", "s2").unwrap();
    let one_dir = PathBuf::from(one["worktreeDir"].as_str().unwrap());
    let two_dir = PathBuf::from(two["worktreeDir"].as_str().unwrap());
    let checkpoints = checkpoint::Checkpoints::new(&data);
    let id = checkpoints
        .before("s1", Some("turn-1"), &one_dir, Change::Command)
        .expect("a checkpoint for the worktree");
    std::fs::write(one_dir.join("draft.txt"), "only s1\n").unwrap();
    std::fs::write(two_dir.join("other.txt"), "s2 keeps this\n").unwrap();
    let plan = checkpoint::plan(&data, &id).unwrap();
    assert!(!plan.changes.is_empty(), "the worktree change is seen");
    checkpoint::undo(&data, &plan).unwrap();
    assert!(
        !one_dir.join("draft.txt").exists(),
        "s1 is back to the checkpoint"
    );
    assert!(
        two_dir.join("other.txt").exists(),
        "the other worktree is untouched"
    );
}
