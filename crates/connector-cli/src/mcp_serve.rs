//! `agentrouter mcp-serve local`: the local-Agent entry, an MCP stdio service with a self-reported local identity
//! (`lag_`/`lses_`, #37/#38). Kept as it is pending the communication design (docs/HOSTD-MCP.md §4, owner Q2); the
//! cloud's `mcp-serve workspace` mode was deleted with the old device chain (#49): the cloud reaches a folder only
//! through `agentrouter link` and its sandboxed MCP servers.
//!
//! Tools: workspace/project/worktree management (the M1 primitives) and `exec` — a command run in the
//! person's shell inside one of those directories (the ssh-like half). Every path is checked with the
//! workspace gate, dangerous commands are denied or confirmed on this computer, and every exec is
//! audited here (the connector also records the `mcp_call` with its command line).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use agentrouter_device::config::{Access, Config};
use agentrouter_device::gate::{display, inside, resolve_folders};
use agentrouter_device::protocol::DeviceError;
use agentrouter_device::util::{data_dir, home_dir, iso_now};
use agentrouter_device::{
    audit, consent, deploy, exec, local_agent, lockfile, workspace, worktree,
};

const SERVER_NAME: &str = "agentrouter-workspace";
const PROTOCOL: &str = "2025-06-18";

/// Set when the process was started as `mcp-serve local`: the self-reported local Agent identity.
static LOCAL_IDENTITY: OnceLock<local_agent::Identity> = OnceLock::new();

fn local_identity() -> Option<&'static local_agent::Identity> {
    LOCAL_IDENTITY.get()
}

/// The caller shown in local approval dialogs/errors and audit entries.
fn caller_label() -> String {
    match local_identity() {
        Some(identity) => format!(
            "本地 Agent {}（会话 {}）",
            identity.agent_name, identity.session_id
        ),
        None => "云端 Agent".to_string(),
    }
}

/// Prefix an approval question with the local caller (the consent dialog then names who asks).
fn question_with_caller(question: &str) -> String {
    match local_identity() {
        Some(identity) => format!(
            "[本地 Agent {}（会话 {}）]\n\n{question}",
            identity.agent_name, identity.session_id
        ),
        None => question.to_string(),
    }
}

/// One MCP tool: name, description, JSON input schema.
fn tools() -> Value {
    json!([
        tool(
            "exec",
            "Run a command with the device shell (PowerShell 7 when installed, else Windows PowerShell 5.1) in a workspace project or the session's worktree; returns stdout, stderr and the exit code. cwd defaults to this session's worktree.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run."},
                    "cwd": {"type": "string", "description": "Absolute project or worktree directory; defaults to the session worktree."},
                    "timeout_ms": {"type": "integer", "minimum": 1000, "maximum": 600000, "description": "Kill the command after this; default 60000."}
                },
                "required": ["command"]
            }),
        ),
        tool(
            "workspace_create",
            "Create (idempotently) the workspace root on this computer and optionally a first project.",
            json!({"type": "object", "properties": {"name": {"type": "string", "description": "Optional project name to create."}}, "required": []}),
        ),
        tool(
            "workspace_list",
            "List the workspace root, projects and worktrees.",
            json!({"type": "object", "properties": {}, "required": []})
        ),
        tool(
            "project_add",
            "Create (idempotently) a project directory inside the workspace root.",
            json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
        ),
        tool(
            "project_list",
            "List the workspace projects.",
            json!({"type": "object", "properties": {}, "required": []})
        ),
        tool(
            "worktree_create",
            "Create (idempotently) this session's git worktree for a project and return its directory (the default exec cwd).",
            json!({"type": "object", "properties": {"project": {"type": "string"}}, "required": ["project"]}),
        ),
        tool(
            "worktree_list",
            "List the session worktrees (optionally one project).",
            json!({"type": "object", "properties": {"project": {"type": "string"}}, "required": []}),
        ),
        tool(
            "worktree_remove",
            "Remove a session's worktree (branch kept unless delete_branch is true); defaults to this session.",
            json!({"type": "object", "properties": {"project": {"type": "string"}, "session": {"type": "string"}, "delete_branch": {"type": "boolean"}}, "required": ["project"]}),
        ),
        tool(
            "worktree_prune",
            "Remove stale worktree directories of a project (older than older_hours, default 72).",
            json!({"type": "object", "properties": {"project": {"type": "string"}, "older_hours": {"type": "integer"}, "delete_branches": {"type": "boolean"}}, "required": ["project"]}),
        ),
        tool(
            "deploy_targets",
            "List the registered external deploy targets on this computer (name and directory).",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        tool(
            "deploy_target_add",
            "Register an external deploy target (full access only; in folders mode the owner registers it with the local CLI). Same validation as the CLI: an existing directory outside the workspace root and the device data folder.",
            json!({"type": "object", "properties": {"name": {"type": "string"}, "path": {"type": "string"}}, "required": ["name", "path"]}),
        ),
        tool(
            "deploy_target_list",
            "List the registered deploy targets (full access only).",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        tool(
            "deploy_target_remove",
            "Remove a registered deploy target by name (full access only).",
            json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
        ),
        tool(
            "deploy",
            "Deploy files from the project/this session's worktree onto a registered external target (for example a game install). Backs up overwritten files, asks the person on this computer, records the deployment and rolls back automatically on failure.",
            json!({
                "type": "object",
                "properties": {
                    "project": {"type": "string"},
                    "target": {"type": "string", "description": "A registered deploy target name (deploy_targets)."},
                    "artifacts": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {"type": "string"},
                                {"type": "object", "properties": {"source": {"type": "string"}, "dest": {"type": "string"}}, "required": ["source", "dest"]}
                            ]
                        },
                        "description": "Files to deploy: source inside the workspace, dest a relative path under the target."
                    },
                    "message": {"type": "string"}
                },
                "required": ["project", "target", "artifacts"]
            }),
        ),
        tool(
            "deploy_list",
            "List recent deployments (id, target, files, status).",
            json!({"type": "object", "properties": {"limit": {"type": "integer"}}, "required": []}),
        ),
        tool(
            "deploy_rollback",
            "Roll a deployment back: restore the backed-up target files (or remove the files it added). Asks the person on this computer first.",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        ),
    ])
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": schema})
}

fn data() -> PathBuf {
    std::env::var_os("AGENTROUTER_DEVICE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(data_dir)
}

fn session() -> String {
    // A local Agent's conversation is its own `lses_` isolation unit; the cloud path keeps using the
    // session the connector injected.
    if let Some(identity) = local_identity() {
        return identity.session_id.clone();
    }
    std::env::var("AGENTROUTER_SESSION").unwrap_or_default()
}

fn linked() -> Vec<PathBuf> {
    resolve_folders(&Config::load().folders)
}

fn text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn failure(error: &DeviceError) -> Value {
    let mut body = format!("{}: {}", error.code, error.message);
    if let Some(next) = &error.next {
        body.push('\n');
        body.push_str(next);
    }
    json!({"content": [{"type": "text", "text": body}], "isError": true})
}

/// Local approval, decided only from this machine's own config (written by `link`): an unattended
/// machine's operator pre-approved (`--unattended`), a headless machine answers no, otherwise the
/// person is asked here. Never from the environment: the cloud supplies a server's environment, so a
/// value there would let the cloud approve its own requests (#39, #43).
fn approved(question: &str) -> bool {
    let config = Config::load();
    if config.unattended {
        return true;
    }
    if config.headless {
        return false;
    }
    let cancel = AtomicBool::new(false);
    consent::question(consent::TITLE, question, &cancel)
}

/// The directory an exec runs in: explicit (a registered project/worktree), else the session's
/// worktree, else the only project.
fn exec_cwd(args: &Value) -> Result<PathBuf, DeviceError> {
    let data = data();
    let session = session();
    if let Some(raw) = args
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        let resolved = worktree::resolve_cwd(&data, &session, &json!({"cwd": raw}))?;
        let Some(cwd) = resolved else {
            return Err(DeviceError::new(
                "WORKSPACE_CWD_INVALID",
                format!("{raw} 不是工作区里登记的项目或 worktree 目录"),
            ));
        };
        return Ok(cwd.path);
    }
    if let Some(worktree) = worktree::for_session(&data, &session)? {
        return workspace::real(Path::new(&worktree.path))
            .map_err(|e| DeviceError::new("WORKSPACE_CWD_INVALID", e.message));
    }
    let registry = workspace::load(&data)?;
    if registry.projects.len() == 1 {
        return workspace::real(Path::new(&registry.projects[0].path))
            .map_err(|e| DeviceError::new("WORKSPACE_CWD_INVALID", e.message));
    }
    Err(DeviceError::new(
        "WORKSPACE_NO_CWD",
        "没有可运行命令的目录：这个会话还没有 worktree",
    )
    .next("先 worktree_create(project)；或只有一个项目时自动用项目目录。"))
}

fn append_audit(entry: Value) {
    let dir = data();
    let _ = std::fs::create_dir_all(&dir);
    let mut value = json!({"at": iso_now(), "session": session(), "entry": entry});
    if let Some(identity) = local_identity() {
        value["caller"] = identity.caller();
    }
    let line = format!("{}\n", serde_json::to_string(&value).unwrap_or_default());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mcp-audit.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Serialize execs of the same project across local sessions/processes (owner: one project at a time).
fn local_exec_lock(data: &Path, cwd: &Path) -> Result<Option<lockfile::FileLock>, DeviceError> {
    if local_identity().is_none() {
        return Ok(None);
    }
    let registry = workspace::load(data)?;
    for project in &registry.projects {
        let real = workspace::real(Path::new(&project.path))?;
        if inside(cwd, &real) || inside(cwd, Path::new(&project.path)) {
            let path = data
                .join("locks")
                .join(format!("project-{}.lock", local_agent::slug(&project.slug)));
            let timeout = std::env::var("AGENTROUTER_LOCK_TIMEOUT_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(30_000)
                .clamp(100, 600_000);
            return Ok(Some(lockfile::acquire(
                &path,
                Duration::from_millis(timeout),
            )?));
        }
    }
    Ok(None)
}

/// Registering/removing deploy targets from the Agent is a full-access capability (owner decision
/// 2026-10-18). In folders mode the owner keeps doing it with the local CLI.
fn require_full_access(what: &str) -> Result<(), DeviceError> {
    let config = Config::load();
    if config.access == Access::Full {
        return Ok(());
    }
    Err(DeviceError::new(
        "DEPLOY_TARGET_OWNER_ONLY",
        format!("{what}只能由所有者在电脑上登记：当前访问级别不是「完全访问」"),
    )
    .next("请所有者在这台电脑上运行 agentrouter workspace deploy target add <名字> <目录>；完全访问下才允许云端 AI 自己登记。"))
}

fn call_tool(tool: &str, args: &Value) -> Result<Value, DeviceError> {
    let data = data();
    let home = home_dir();
    let session = session();
    match tool {
        "exec" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            if command.trim().is_empty() {
                return Err(DeviceError::new("WORKSPACE_INVALID", "command 不能为空"));
            }
            let cwd = exec_cwd(args)?;
            let timeout = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(exec::TIMEOUT_DEFAULT_MS)
                .clamp(1000, exec::TIMEOUT_MAX_MS);
            let danger = exec::classify(command);
            let config = Config::load();
            match danger {
                exec::Danger::Deny(why) => {
                    append_audit(
                        json!({"tool": "exec", "cwd": display(&cwd), "command": command, "denied": why}),
                    );
                    return Err(DeviceError::denied(format!(
                        "{}：这条命令被拒绝：{why}",
                        caller_label()
                    ))
                    .next("换一种做法；确实需要请在电脑上自己操作。"));
                }
                exec::Danger::Ask(why) => {
                    if config.access != Access::Full {
                        let question = format!(
                            "想在这台电脑上运行一条可能危险的操作：{why}\n\n{command}\n\n位置：{}",
                            display(&cwd)
                        );
                        let ok = approved(&question_with_caller(&question));
                        if !ok {
                            append_audit(
                                json!({"tool": "exec", "cwd": display(&cwd), "command": command, "denied": why, "reason": "no consent"}),
                            );
                            return Err(DeviceError::denied(format!(
                                "{}没有确认这条命令，已取消",
                                caller_label()
                            ))
                            .next("不要原样重试；换个不需要确认的做法。"));
                        }
                        append_audit(
                            json!({"tool": "exec", "cwd": display(&cwd), "command": command, "approved": why}),
                        );
                    }
                }
                exec::Danger::Safe => {}
            }
            // Local sessions of the same project are serialized; the lock is held for the command.
            let _project_lock = local_exec_lock(&data, &cwd)?;
            // The OS write boundary under the gate/deny/consent (S2b): at `folders` the exec cwd is
            // labelled Low and the command runs at low integrity; at `full` the person asked for
            // unrestricted commands and the boundary is off.
            let outcome = if config.access == Access::Full {
                exec::run(command, &cwd, Duration::from_millis(timeout))
            } else {
                exec::run_confined(command, &cwd, Duration::from_millis(timeout), &data)
            }
            .map_err(|e| DeviceError::new("EXEC_FAILED", e))?;
            append_audit(json!({
                "tool": "exec", "cwd": display(&cwd), "command": command,
                "exitCode": outcome.exit_code, "timedOut": outcome.timed_out,
                "durationMs": outcome.duration_ms,
            }));
            let body = format!(
                "exit_code: {}\ntimed_out: {}\nduration_ms: {}\ncwd: {}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}",
                outcome
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "(killed)".to_string()),
                outcome.timed_out,
                outcome.duration_ms,
                display(&cwd),
                outcome.stdout,
                outcome.stderr,
            );
            Ok(json!({
                "content": [{"type": "text", "text": body}],
                "isError": outcome.timed_out || outcome.exit_code != Some(0),
            }))
        }
        "workspace_create" => {
            let linked = linked();
            let root = workspace::ensure_root(&data, &home, None, &linked)?;
            let mut projects = Vec::new();
            if let Some(name) = args
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.trim().is_empty())
            {
                let ensured = workspace::ensure_project(&data, &home, &linked, name, None)?;
                projects.push(ensured.project_dir.clone());
            } else {
                let registry = workspace::load(&data)?;
                projects.extend(registry.projects.iter().map(|p| p.path.clone()));
            }
            let value = json!({"root": display(&root), "projectDir": projects.first(), "projects": projects});
            append_audit(json!({"tool": "workspace_create", "root": display(&root)}));
            Ok(json!({"content": [{"type": "text", "text": text(&value)}]}))
        }
        "workspace_list" => {
            let registry = workspace::listing(&data, &home)?;
            let worktrees = worktree::list(&data, None)?;
            let value = json!({"root": registry.root, "projects": registry.projects, "worktrees": worktrees["worktrees"]});
            Ok(json!({"content": [{"type": "text", "text": text(&value)}]}))
        }
        "project_add" => {
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            let linked = linked();
            let ensured = workspace::ensure_project(&data, &home, &linked, name, None)?;
            append_audit(
                json!({"tool": "project_add", "project": ensured.slug, "path": ensured.project_dir}),
            );
            Ok(
                json!({"content": [{"type": "text", "text": text(&serde_json::to_value(&ensured).unwrap_or_default())}]}),
            )
        }
        "project_list" => {
            let registry = workspace::listing(&data, &home)?;
            Ok(
                json!({"content": [{"type": "text", "text": text(&json!({"root": registry.root, "projects": registry.projects}))}]}),
            )
        }
        "worktree_create" => {
            let project = args.get("project").and_then(Value::as_str).unwrap_or("");
            if session.trim().is_empty() {
                return Err(DeviceError::new(
                    "WORKSPACE_SESSION_INVALID",
                    "这个服务不知道是哪个会话在调用（没有 AGENTROUTER_SESSION）",
                )
                .next("从云端对话里调用；不要在本机终端里直接起这个服务。"));
            }
            let created = worktree::create(&data, project, &session)?;
            append_audit(
                json!({"tool": "worktree_create", "project": project, "session": session, "worktreeDir": created["worktreeDir"]}),
            );
            Ok(json!({"content": [{"type": "text", "text": text(&created)}]}))
        }
        "worktree_list" => {
            let project = args.get("project").and_then(Value::as_str);
            Ok(
                json!({"content": [{"type": "text", "text": text(&worktree::list(&data, project)?)}]}),
            )
        }
        "worktree_remove" => {
            let project = args.get("project").and_then(Value::as_str).unwrap_or("");
            let wanted = args
                .get("session")
                .and_then(Value::as_str)
                .unwrap_or(&session)
                .to_string();
            let delete_branch = args
                .get("delete_branch")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let removed = worktree::remove(&data, project, &wanted, !delete_branch)?;
            append_audit(json!({"tool": "worktree_remove", "project": project, "session": wanted}));
            Ok(json!({"content": [{"type": "text", "text": text(&removed)}]}))
        }
        "worktree_prune" => {
            let project = args.get("project").and_then(Value::as_str).unwrap_or("");
            let older = args.get("older_hours").and_then(Value::as_u64);
            let delete_branches = args
                .get("delete_branches")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let pruned = worktree::prune(&data, project, older, delete_branches)?;
            append_audit(json!({"tool": "worktree_prune", "project": project}));
            Ok(json!({"content": [{"type": "text", "text": text(&pruned)}]}))
        }
        "deploy_targets" => {
            let targets = deploy::list_targets(&data)?;
            let value = json!({"targets": targets});
            Ok(json!({"content": [{"type": "text", "text": text(&value)}]}))
        }
        "deploy_target_add" => {
            require_full_access("登记部署目标")?;
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
            if name.trim().is_empty() || path.trim().is_empty() {
                return Err(DeviceError::new(
                    "WORKSPACE_INVALID",
                    "deploy_target_add 需要 name 和 path",
                ));
            }
            let target = deploy::add_target(&data, name, path)?;
            append_audit(json!({
                "tool": "deploy_target_add", "name": target.name, "path": target.path,
            }));
            Ok(json!({"content": [{"type": "text", "text": text(&json!({"target": target}))}]}))
        }
        "deploy_target_list" => {
            require_full_access("列出部署目标")?;
            let targets = deploy::list_targets(&data)?;
            Ok(json!({"content": [{"type": "text", "text": text(&json!({"targets": targets}))}]}))
        }
        "deploy_target_remove" => {
            require_full_access("移除部署目标")?;
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            if name.trim().is_empty() {
                return Err(DeviceError::new(
                    "WORKSPACE_INVALID",
                    "deploy_target_remove 需要 name",
                ));
            }
            let removed = deploy::remove_target(&data, name)?;
            append_audit(json!({"tool": "deploy_target_remove", "name": name, "removed": removed}));
            Ok(
                json!({"content": [{"type": "text", "text": text(&json!({"name": name, "removed": removed}))}]}),
            )
        }
        "deploy" => {
            let project = args.get("project").and_then(Value::as_str).unwrap_or("");
            let target = args.get("target").and_then(Value::as_str).unwrap_or("");
            let message = args.get("message").and_then(Value::as_str).unwrap_or("");
            let artifacts = args.get("artifacts").cloned().unwrap_or(Value::Null);
            let planned = deploy::plan(&data, &session, project, &artifacts, target, message)?;
            if !approved(&question_with_caller(&deploy::approval_text(&planned))) {
                append_audit(
                    json!({"tool": "deploy", "target": planned.target.name, "denied": "no consent"}),
                );
                return Err(DeviceError::denied(format!(
                    "{}没有批准这次部署，已取消",
                    caller_label()
                ))
                .next("不要原样重试；把要部署的内容和原因告诉用户，等他们自己批准。"));
            }
            let summary = deploy::apply(&data, &planned)?;
            append_audit(json!({
                "tool": "deploy", "id": summary["id"], "target": summary["target"],
                "files": summary["files"], "project": summary["project"], "message": summary["message"],
            }));
            Ok(json!({"content": [{"type": "text", "text": text(&summary)}]}))
        }
        "deploy_list" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
            let value = json!({"deployments": deploy::list(&data, limit)});
            Ok(json!({"content": [{"type": "text", "text": text(&value)}]}))
        }
        "deploy_rollback" => {
            let id = args.get("id").and_then(Value::as_str).unwrap_or("");
            let question = deploy::rollback_text(&data, id)?;
            if !approved(&question_with_caller(&question)) {
                return Err(DeviceError::denied(format!(
                    "{}没有批准这次回滚，已取消",
                    caller_label()
                )));
            }
            let summary = deploy::rollback_apply(&data, id)?;
            append_audit(json!({"tool": "deploy_rollback", "id": id, "target": summary["target"]}));
            Ok(json!({"content": [{"type": "text", "text": text(&summary)}]}))
        }
        other => Err(DeviceError::new(
            "UNKNOWN_TOOL",
            format!("没有这个工具：{other}"),
        )),
    }
}

/// One JSON-RPC/2.0 line through stdin; a response line through stdout when the request has an id.
fn handle(line: &str) -> Option<Value> {
    let request: Value = serde_json::from_str(line).ok()?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
        }),
        "notifications/initialized" | "notifications/cancelled" => return None,
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = call_tool(name, &arguments);
            // A locally launched service also appends to the connector's hash-chained audit log, with
            // the caller (lag/lses); audit.rs locks the chain across processes.
            if let Some(identity) = local_identity() {
                let (outcome, error) = match &result {
                    Ok(_) => ("ok", None),
                    Err(error) => ("error", Some(error.code)),
                };
                audit::append(
                    &data().join("audit.log"),
                    json!({
                        "action": "local_mcp_call",
                        "caller": identity.caller(),
                        "tool": name,
                        "outcome": outcome,
                        "error": error,
                    }),
                );
            }
            match result {
                Ok(value) => value,
                Err(error) => failure(&error),
            }
        }
        other => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown method {other}")},
            }));
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve the workspace tools on stdio until stdin closes. Returns the process exit code.
///
/// `mcp-serve workspace` is the cloud-installed service (unchanged); `mcp-serve local` is the local
/// Agent entry: same tools, but the caller is a locally registered `lag_`/`lses_` identity read from
/// the launch environment, and every audit entry/dialog names it.
pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("local") || args.len() != 1 {
        eprintln!("用法：agentrouter mcp-serve local（本地 Agent 的 stdio MCP 服务）");
        return 2;
    }
    {
        let identity = local_agent::Identity::from_env();
        let data = data();
        if let Err(error) = local_agent::register(&data, &identity) {
            eprintln!("本地 Agent 登记失败：{} {}", error.code, error.message);
            return 2;
        }
        let _ = LOCAL_IDENTITY.set(identity);
        append_audit(json!({"tool": "local_agent_seen"}));
        audit::append(
            &data.join("audit.log"),
            json!({
                "action": "local_agent_seen",
                "caller": local_identity().map(|identity| identity.caller()),
            }),
        );
        // The local entry never adopts a cloud identity: only the local env values are used.
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle(&line) {
            if serde_json::to_writer(&mut stdout, &response).is_err() {
                break;
            }
            if stdout
                .write_all(b"\n")
                .and_then(|_| stdout.flush())
                .is_err()
            {
                break;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_have_names_and_schemas() {
        let list = tools();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in [
            "exec",
            "workspace_create",
            "project_add",
            "project_list",
            "worktree_create",
            "worktree_list",
            "worktree_remove",
            "worktree_prune",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        for item in list.as_array().unwrap() {
            assert_eq!(item["inputSchema"]["type"], "object", "{item}");
        }
    }

    #[test]
    fn initialize_and_unknown_method() {
        let init = handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], SERVER_NAME);
        let unknown = handle(r#"{"jsonrpc":"2.0","id":2,"method":"nope","params":{}}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }
}
