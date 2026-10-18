//! The device's side of each request: protocol checks, then the local gate (access level, the MCP switch),
//! then the action, with every step in the audit log and on the console stream.
//!
//! The device is the link (owner 2026-10-14: 「电脑上的mcp就是接口，我们的链接就是把这些mcp能够链接上云」「就用现成的
//! mcp就好了，就是默认安装的mcp」): it relays signed requests to the local MCP servers the cloud installs here
//! (mcp.rs). It has no file, command or screen tools of its own any more (LINKED-DEVICES.md §23.8,
//! DEVICE-PROTOCOL.md §18.4); what stays ours is the link, the 「正在操作」 bar, the lock screen / UAC rule,
//! the per-turn checkpoint before a writing call (`agentrouter undo`) and the audit log.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{Value, json};

use crate::audit::Audit;
use crate::checkpoint::{Change, Checkpoints};
use crate::config::Access;
use crate::gate::{Scope, display, resolve_folders};
use crate::keystore::Identity;
use crate::mcp::McpHost;
use crate::protocol::{DeviceError, ReplayCache, check_request};
use crate::screen::Screen;
use crate::util::{clip, now_ms};

/// A conversation counts as "using" the device this long after its last request.
const IN_USE_MS: i64 = 20_000;

/// The actions this device understands besides the MCP ones (reported in `hello`).
pub const ACTIONS: &[&str] = &[
    "info",
    "workspace_ensure",
    "workspace_list",
    "workspace_baseline",
    "workspace_worktree_create",
    "workspace_worktree_list",
    "workspace_worktree_remove",
    "workspace_worktree_prune",
];

type Console = Arc<Mutex<Option<(Sender<String>, i64)>>>;
/// A line for the person at the terminal (what the cloud just did here).
pub type Echo = Arc<dyn Fn(&str) + Send + Sync>;

pub struct Device {
    scope: RwLock<Scope>,
    identity: Mutex<Option<Identity>>,
    replay: Mutex<ReplayCache>,
    audit: Arc<Mutex<Audit>>,
    console: Console,
    last_use: Mutex<Option<(String, i64)>>,
    checkpoints: Arc<Checkpoints>,
    foreground: AtomicBool,
    echo: Arc<Mutex<Option<Echo>>>,
    home: PathBuf,
    data_dir: PathBuf,
    /// The workspace switch (`agentrouter workspace init`): the cloud may create the workspace root and
    /// projects here (`workspace_ensure`). Off by default (workspace spec §6.1).
    workspaces: RwLock<bool>,
    /// The screen switch (`agentrouter screen on`): Windows UI servers join the default bundle; the bar and the
    /// secure-desktop check (screen.rs).
    pub screen: Screen,
    mcp: McpHost,
}

pub struct Options {
    pub data_dir: PathBuf,
    pub access: Access,
    pub folders: Vec<String>,
    pub home: PathBuf,
    pub workspaces: bool,
}

fn emit_to(console: &Console, kind: &str, session: &str, text: &str) {
    if let Some((tx, stream)) = console.lock().unwrap().as_ref() {
        let event = json!({"kind": kind, "session": session, "text": clip(text, 8000)});
        let _ = tx.send(json!({"id": stream, "event": event}).to_string());
    }
}

/// The turn a change belongs to (DEVICE-PROTOCOL.md §5.7).
fn turn_of(args: &Value) -> Option<String> {
    args.get("turn")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Put the checkpoint id into a result.
fn with_checkpoint(mut value: Value, checkpoint: Option<String>) -> Value {
    if let (Some(id), Some(map)) = (checkpoint, value.as_object_mut()) {
        map.insert("checkpoint".into(), json!(id));
    }
    value
}

/// The shell the person's computer runs commands in (shown to the AI; the terminal server uses it).
fn shell_name() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

impl Device {
    pub fn new(options: Options) -> Device {
        let deny = std::fs::canonicalize(&options.data_dir)
            .ok()
            .into_iter()
            .collect();
        Device {
            scope: RwLock::new(Scope {
                access: options.access,
                folders: resolve_folders(&options.folders),
                deny,
            }),
            identity: Mutex::new(None),
            replay: Mutex::new(ReplayCache::default()),
            audit: Arc::new(Mutex::new(Audit::open(&options.data_dir.join("audit.log")))),
            console: Arc::new(Mutex::new(None)),
            last_use: Mutex::new(None),
            checkpoints: Arc::new(Checkpoints::new(&options.data_dir)),
            foreground: AtomicBool::new(false),
            echo: Arc::new(Mutex::new(None)),
            home: options.home,
            mcp: McpHost::new(&options.data_dir),
            data_dir: options.data_dir,
            workspaces: RwLock::new(options.workspaces),
            screen: Screen::default(),
        }
    }

    pub fn set_identity(&self, identity: Option<Identity>) {
        *self.identity.lock().unwrap() = identity;
    }

    pub fn device_id(&self) -> Option<String> {
        self.identity
            .lock()
            .unwrap()
            .as_ref()
            .map(|i| i.device.clone())
    }

    pub fn set_access(&self, access: Access, folders: &[String]) {
        let kept = {
            let mut scope = self.scope.write().unwrap();
            scope.access = access;
            scope.folders = resolve_folders(folders);
            scope.folders.clone()
        };
        // Integrity labels an older version put on a folder no longer linked come off (confine.rs).
        let _ = crate::confine::release(&self.data_dir, &kept);
    }

    pub fn access(&self) -> Access {
        self.scope.read().unwrap().access
    }

    /// A person is at the terminal (the command line in the foreground).
    pub fn set_foreground(&self, on: bool) {
        self.foreground.store(on, Ordering::SeqCst);
    }

    /// The screen switch (`agentrouter screen on`).
    pub fn set_screen(&self, on: bool) {
        self.screen.set(on);
    }

    /// Local MCP servers (`agentrouter mcp on`): run the servers the cloud installs here.
    pub fn set_mcp(&self, on: bool) {
        self.mcp.set(on);
    }

    /// The workspace switch (`agentrouter workspace init`).
    pub fn set_workspaces(&self, on: bool) {
        *self.workspaces.write().unwrap() = on;
    }

    pub fn workspaces_enabled(&self) -> bool {
        *self.workspaces.read().unwrap()
    }

    /// The actions this device serves: `info` and the MCP host.
    pub fn actions(&self) -> Vec<&'static str> {
        let mut list = ACTIONS.to_vec();
        list.extend_from_slice(crate::mcp::ACTIONS);
        list
    }

    pub fn set_echo(&self, echo: Option<Echo>) {
        *self.echo.lock().unwrap() = echo;
    }

    pub fn set_console(&self, stream: Option<(Sender<String>, i64)>) {
        *self.console.lock().unwrap() = stream;
    }

    fn say(&self, text: &str) {
        let echo = self.echo.lock().unwrap().clone();
        if let Some(echo) = echo {
            echo(text);
        }
    }

    pub fn audit_path(&self) -> PathBuf {
        self.audit.lock().unwrap().path().to_path_buf()
    }

    pub fn record(&self, entry: Value) {
        self.audit.lock().unwrap().record(entry);
    }

    /// The conversation using the device right now (a recent request), if any.
    pub fn in_use_by(&self) -> Option<String> {
        let last = self.last_use.lock().unwrap();
        last.as_ref()
            .filter(|(_, at)| now_ms() - at < IN_USE_MS)
            .map(|(s, _)| s.clone())
    }

    /// What the device tells the web and the AI about itself (the `hello` info).
    pub fn info(&self) -> Value {
        let scope = self.scope.read().unwrap();
        json!({
            "os": crate::util::os_name(),
            "arch": crate::util::arch_name(),
            "shell": shell_name(),
            "home": display(&self.home),
            "access": scope.access.as_str(),
            "folders": scope.folders.iter().map(|f| display(f)).collect::<Vec<_>>(),
            "app": crate::util::app_kind(),
            "version": env!("CARGO_PKG_VERSION"),
            "actions": self.actions(),
            "screen": self.screen.enabled(),
            "mcp": self.mcp.enabled(),
            "workspaces": self.workspaces_enabled(),
            "mcpServers": self.mcp.summary(),
            "checkpoints": true,
            "foreground": self.foreground.load(Ordering::SeqCst),
        })
    }

    /// Stop everything the AI started here (tray 断开, revocation, quit, Ctrl+C).
    pub fn stop_all(&self, why: &str) {
        self.screen.forget();
        self.mcp.stop_all();
        self.record(json!({"event": "stop_all", "reason": why}));
    }

    /// One signed request from the gateway.
    pub fn serve(
        &self,
        request: &Value,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let session_hint = request
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let action_hint = request
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let checked = {
            let identity = self.identity.lock().unwrap();
            let Some(identity) = identity.as_ref() else {
                return Err(DeviceError::new("FAILED", "this device is not linked"));
            };
            let mut replay = self.replay.lock().unwrap();
            check_request(
                request,
                args,
                &identity.control,
                &identity.device,
                now_ms(),
                &mut replay,
            )
        };
        let verified = match checked {
            Ok(v) => v,
            Err(e) => {
                self.record(json!({"session": session_hint, "action": action_hint, "outcome": e.code, "detail": e.message}));
                return Err(e);
            }
        };
        *self.last_use.lock().unwrap() = Some((verified.session.clone(), now_ms()));
        let session = verified.session.as_str();
        let result = self.act(&verified.action, session, args, cancel);
        let summary = summary(&verified.action, args);
        match &result {
            Ok(_) => {
                self.record(json!({"session": session, "action": verified.action, "text": summary, "outcome": "ok"}));
                if verified.action != "info" {
                    self.say(&format!(
                        "{} {} — 完成",
                        verified.action,
                        clip(&summary, 200)
                    ));
                }
            }
            Err(e) => {
                if e.code == "DENIED" {
                    emit_to(
                        &self.console,
                        "denied",
                        session,
                        &format!("{}: {}", verified.action, e.message),
                    );
                }
                self.record(json!({"session": session, "action": verified.action, "text": summary, "outcome": e.code, "detail": e.message}));
                self.say(&format!(
                    "{} {} — {}",
                    verified.action,
                    clip(&summary, 200),
                    e.code
                ));
            }
        }
        result
    }

    fn act(
        &self,
        action: &str,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let scope = self.scope.read().unwrap().clone();
        match action {
            "info" => Ok(json!({"device": self.info(), "jobs": [], "approvals": []})),
            "workspace_ensure" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                if project.trim().is_empty() {
                    return Err(DeviceError::new(
                        "WORKSPACE_INVALID",
                        "要写项目名：workspace_ensure 带 project（如 mod-arena）",
                    ));
                }
                let root = args.get("root").and_then(Value::as_str);
                let ensured = crate::workspace::ensure_project(
                    &self.data_dir,
                    &self.home,
                    &scope.folders,
                    project,
                    root,
                )?;
                serde_json::to_value(ensured)
                    .map_err(|e| DeviceError::new("WORKSPACE_FAILED", format!("结果写不出来：{e}")))
            }
            "workspace_list" => {
                self.require_workspaces()?;
                let registry = crate::workspace::listing(&self.data_dir, &self.home)?;
                let mut value = serde_json::to_value(registry).map_err(|e| {
                    DeviceError::new("WORKSPACE_FAILED", format!("结果写不出来：{e}"))
                })?;
                value["enabled"] = json!(true);
                Ok(value)
            }
            "workspace_baseline" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                crate::worktree::baseline(&self.data_dir, project)
            }
            "workspace_worktree_create" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                let wanted = args
                    .get("session")
                    .or_else(|| args.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or(session);
                crate::worktree::create(&self.data_dir, project, wanted)
            }
            "workspace_worktree_list" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str);
                crate::worktree::list(&self.data_dir, project)
            }
            "workspace_worktree_remove" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                let wanted = args
                    .get("session")
                    .or_else(|| args.get("sessionId"))
                    .and_then(Value::as_str)
                    .unwrap_or(session);
                let keep_branch = args
                    .get("keepBranch")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                crate::worktree::remove(&self.data_dir, project, wanted, keep_branch)
            }
            "workspace_worktree_prune" => {
                self.require_workspaces()?;
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                let older = args.get("olderThanHours").and_then(Value::as_u64);
                let delete_branches = args
                    .get("deleteBranches")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                crate::worktree::prune(&self.data_dir, project, older, delete_branches)
            }
            "mcp_start" => {
                let wants_workspace = crate::worktree::wants_workspace(args);
                if wants_workspace && !self.workspaces_enabled() {
                    return Err(DeviceError::new(
                        "WORKSPACE_DISABLED",
                        "这台电脑没有打开工作区，不能按项目/worktree 指定 cwd",
                    )
                    .next("请用户在电脑上运行 agentrouter workspace init。"));
                }
                let (cwd, pairs) = if self.workspaces_enabled() {
                    let cwd = crate::worktree::resolve_cwd(&self.data_dir, session, args)?;
                    let pairs =
                        crate::worktree::placeholder_pairs(&self.data_dir, session, cwd.as_ref())?;
                    (cwd, pairs)
                } else {
                    (None, Vec::new())
                };
                let mut resolved = args.clone();
                crate::worktree::substitute(&mut resolved, &pairs)?;
                self.mcp.start(
                    &scope,
                    &resolved,
                    cancel,
                    cwd.as_ref().map(|c| c.path.as_path()),
                )
            }
            "mcp_stop" => self.mcp.stop(args),
            "mcp_list" => self.mcp.list(),
            "mcp_call" => {
                // A call that may change files takes this turn's checkpoint first: the session's
                // worktree when it has one, then the linked folders (`agentrouter undo` rolls back).
                let checkpoint = if args.get("writes").and_then(Value::as_bool) == Some(true) {
                    let turn = turn_of(args);
                    let worktree = crate::worktree::for_session(&self.data_dir, session)
                        .ok()
                        .flatten()
                        .and_then(|w| {
                            self.checkpoints.before(
                                session,
                                turn.as_deref(),
                                Path::new(&w.path),
                                Change::Command,
                            )
                        });
                    worktree.or_else(|| {
                        scope
                            .folders
                            .iter()
                            .filter_map(|f| {
                                self.checkpoints.before(
                                    session,
                                    turn.as_deref(),
                                    f,
                                    Change::Command,
                                )
                            })
                            .next()
                    })
                } else {
                    None
                };
                self.mcp
                    .call(&scope, args, cancel)
                    .map(|v| with_checkpoint(v, checkpoint))
            }
            "mcp_logs" => self.mcp.logs(args),
            "mcp_package" => self.mcp.package(&scope, args),
            // The device's own file, command and screen actions are gone (DEVICE-PROTOCOL.md §18.4).
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown action")
                .next("This computer is reached through its local MCP servers (device_mcp_list).")),
        }
    }

    fn require_workspaces(&self) -> Result<(), DeviceError> {
        if self.workspaces_enabled() {
            Ok(())
        } else {
            Err(
                DeviceError::new("WORKSPACE_DISABLED", "这台电脑没有打开工作区")
                    .next("请用户在这台电脑上运行 agentrouter workspace init（工作区默认关闭）。"),
            )
        }
    }
}

/// A short description of a request for the audit log (never file contents or secrets).
fn summary(action: &str, args: &Value) -> String {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let text = match action {
        // Never the environment (it carries the person's secrets).
        "mcp_start" => format!(
            "{} = {} {}",
            s("name"),
            s("command"),
            args.get("args")
                .and_then(Value::as_array)
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "))
                .unwrap_or_default()
        ),
        "mcp_call" => format!("{}.{}", s("name"), s("tool")),
        "mcp_stop" | "mcp_logs" => s("name"),
        // The workspace actions: project name and (on the first call) the root; never a file list.
        "workspace_ensure" => format!("{} {}", s("project"), s("root")),
        "workspace_list" => String::new(),
        "workspace_baseline" => s("project"),
        "workspace_worktree_create" => format!(
            "{} {}",
            s("project"),
            args.get("session")
                .or_else(|| args.get("sessionId"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
        "workspace_worktree_list" => s("project"),
        "workspace_worktree_remove" => format!("{} {}", s("project"), s("session")),
        "workspace_worktree_prune" => {
            format!("{} olderThanHours={}", s("project"), s("olderThanHours"))
        }
        // The file and its digest, never the bytes.
        "mcp_package" => args
            .get("pin")
            .map(|p| {
                format!(
                    "{} {}",
                    p.get("file").and_then(Value::as_str).unwrap_or(""),
                    p.get("digest").and_then(Value::as_str).unwrap_or("")
                )
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    clip(&text, 4000)
}
