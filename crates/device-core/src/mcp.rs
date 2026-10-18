//! Local MCP host (DEVICE-PROTOCOL.md §18, LINKED-DEVICES.md §23; owner 2026-10-13: 「我们只做远程联通这块，
//! 更多能力交给 mcp 去做」). The device is the secure link; capabilities come from MCP servers that run here.
//!
//! - A server is a stdio MCP server the cloud asks this device to run (`mcp_start`: command, arguments, environment).
//!   The cloud holds the definition (a 工具 作品) and the person's secrets; the device keeps nothing on disk but logs
//!   it can show, and starts nothing on its own: after a restart the cloud starts a server again on its next call.
//! - The host supervises each one: one process tree per server (a job object on Windows), restart after a crash
//!   (at most 3 times in 2 minutes, then `crashed`), stderr kept as a short log with the secret values blanked.
//! - Calls are forwarded as they are (`tools/call`), image content included; a call has a deadline and is
//!   cancelled on the server (`notifications/cancelled`) when the cloud cancels it.
//! - The platform rules stay here, for every server: the person turns the host on once (`agentrouter mcp on`); only at
//!   「只限这些文件夹」 or 「完全访问」; nothing while the lock screen, UAC or a secure desktop is up; the
//!   「正在操作」 bar names the server and tool; `stop_all` (tray 断开, Ctrl+C, revocation) stops every server.
//! - Catalog packages run only as the file the cloud's catalog pinned (checked here, `mcp_pkg`); a broken npx cache
//!   entry is cleared once; a server's data folder in the project is kept out of git (`keepOut`).

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::Access;
use crate::gate::Scope;
use crate::mcp_pkg::{Packages, Pin};
use crate::protocol::DeviceError;
use crate::util::{clip, now_ms};

/// The signed actions of the host (listed in `info.actions`).
pub const ACTIONS: &[&str] = &[
    "mcp_start",
    "mcp_stop",
    "mcp_list",
    "mcp_call",
    "mcp_logs",
    "mcp_package",
];

const PROTOCOL_VERSION: &str = "2025-06-18";
const MAX_SERVERS: usize = 8;
const LOG_LINES: usize = 200;
const LOG_LINE_MAX: usize = 500;
/// The first start of a package (uvx / npx download it and a runtime) may take minutes.
const HANDSHAKE_MS: u64 = 300_000;
const WAIT_DEFAULT_MS: u64 = 40_000;
const CALL_DEFAULT_MS: u64 = 45_000;
const CALL_MAX_MS: u64 = 55_000;
/// A frame is at most 8 MiB; a result stays under it with room for the envelope.
const RESULT_MAX: usize = 7 << 20;
const RESTARTS_MAX: usize = 3;
const RESTART_WINDOW_MS: i64 = 120_000;
const NAME_MAX: usize = 24;

pub const NEXT_MCP_OFF: &str = "这台电脑没有打开本机工具（MCP）。请用户在这台电脑上运行 agentrouter mcp on（或 agentrouter link --mcp）后再试。";

fn error(code: &'static str, message: impl Into<String>) -> DeviceError {
    DeviceError::new(code, message)
}

/// One server process per (tool name, cwd): the same tool in two session worktrees is two processes.
fn server_key(name: &str, cwd: &Path) -> String {
    format!("{name}\u{1}{}", crate::gate::display(cwd))
}

/// Is this the same directory as the plain path the cloud named (Windows is case-insensitive)?
fn same_cwd(server_cwd: &Path, raw: &str) -> bool {
    let shown = PathBuf::from(crate::gate::display(server_cwd));
    let raw = Path::new(raw);
    crate::gate::inside(raw, &shown) && crate::gate::inside(&shown, raw)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    (2..=NAME_MAX).contains(&name.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// What the cloud asked to run.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Spec {
    name: String,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// Environment variables whose values are secrets: blanked in logs and results.
    secret_env: Vec<String>,
    /// The cloud's hash of the definition (a different one replaces the running server).
    hash: String,
    /// A catalog package: run only as this checked file (`{{package}}`).
    pin: Option<Pin>,
    /// Folders the server keeps its data in, inside a linked folder (serena's `.serena`): kept out of git.
    keep_out: Vec<String>,
}

impl Spec {
    fn parse(args: &Value) -> Result<Spec, DeviceError> {
        let name = str_arg(args, "name").to_string();
        if !valid_name(&name) {
            return Err(error("FAILED", "name 要写 2–24 个小写字母、数字或 _"));
        }
        let command = str_arg(args, "command").trim().to_string();
        if command.is_empty() || command.len() > 1000 {
            return Err(error("FAILED", "command 不能为空"));
        }
        let list = args
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if list.len() > 64 {
            return Err(error("FAILED", "args 最多 64 个"));
        }
        let mut argv = Vec::new();
        for a in list {
            match a.as_str() {
                Some(s) if s.len() <= 4000 => argv.push(s.to_string()),
                _ => return Err(error("FAILED", "args 每个都要是字符串")),
            }
        }
        let mut env = Vec::new();
        if let Some(map) = args.get("env").and_then(Value::as_object) {
            if map.len() > 64 {
                return Err(error("FAILED", "env 最多 64 个"));
            }
            for (k, v) in map {
                let ok_key = !k.is_empty()
                    && k.len() <= 128
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                match v.as_str() {
                    Some(s) if ok_key && s.len() <= 8000 => env.push((k.clone(), s.to_string())),
                    _ => return Err(error("FAILED", format!("env.{k} 要是字符串"))),
                }
            }
        }
        let secret_env = args
            .get("secretEnv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut keep_out = Vec::new();
        for k in args
            .get("keepOut")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            match k.as_str() {
                Some(name) if crate::mcp_pkg::valid_keep_out(name) && keep_out.len() < 4 => {
                    keep_out.push(name.to_string())
                }
                _ => return Err(error("FAILED", "keepOut 要是以 . 开头的文件夹名")),
            }
        }
        Ok(Spec {
            name,
            command,
            args: argv,
            env,
            secret_env,
            hash: clip(str_arg(args, "hash"), 80),
            pin: Pin::parse(args)?,
            keep_out,
        })
    }

    /// The linked folders go in where the definition says (`{{folders}}`: each one as an argument, `{{folder}}`: the
    /// first): the device fills them in, so a server gets exactly the folders the person linked here.
    fn scoped(mut self, folders: &[PathBuf]) -> Result<Spec, DeviceError> {
        let wants = self.args.iter().any(|a| a.contains("{{folder"));
        if wants && folders.is_empty() {
            return Err(error("MCP_NO_FOLDER", "这台设备还没有链接文件夹")
                .next(crate::protocol::NEXT_NO_FOLDER));
        }
        let shown: Vec<String> = folders.iter().map(|f| crate::gate::display(f)).collect();
        let mut args = Vec::new();
        for a in self.args {
            if a == "{{folders}}" {
                args.extend(shown.iter().cloned());
            } else {
                args.push(a.replace(
                    "{{folder}}",
                    shown.first().map(String::as_str).unwrap_or(""),
                ));
            }
        }
        self.args = args;
        Ok(self)
    }

    fn secrets(&self) -> Vec<String> {
        self.env
            .iter()
            .filter(|(k, v)| self.secret_env.contains(k) && v.len() >= 4)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// The command line as the log and the audit show it (never the environment).
    fn shown(&self) -> String {
        clip(&format!("{} {}", self.command, self.args.join(" ")), 300)
    }
}

fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "[secret]");
        }
    }
    out
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    match value {
        Value::String(s) => {
            if secrets.iter().any(|x| s.contains(x.as_str())) {
                *s = redact(s, secrets);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| redact_value(v, secrets)),
        Value::Object(m) => m.values_mut().for_each(|v| redact_value(v, secrets)),
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Starting,
    Ready,
    Restarting,
    Crashed,
    Stopped,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Ready => "ready",
            State::Restarting => "restarting",
            State::Crashed => "crashed",
            State::Stopped => "stopped",
        }
    }
}

struct Status {
    state: State,
    tools: Vec<Value>,
    server: Value,
    pid: Option<u32>,
    started_at: i64,
    error: Option<String>,
    crashes: VecDeque<i64>,
    restarts: u32,
    generation: u64,
    /// A broken npx cache entry was cleared once for this server.
    healed: bool,
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
    #[cfg(windows)]
    job: Option<win::Job>,
    generation: u64,
}

struct Server {
    spec: Spec,
    dir: PathBuf,
    /// Where it runs: the first linked folder (servers that work on "the current project"), else its own folder.
    cwd: PathBuf,
    proc: Mutex<Option<Proc>>,
    pending: Mutex<HashMap<i64, Sender<Value>>>,
    status: Mutex<Status>,
    changed: Condvar,
    logs: Mutex<VecDeque<String>>,
    next_id: AtomicI64,
    stopping: AtomicBool,
}

impl Server {
    fn log(&self, line: &str) {
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_LINES {
            logs.pop_front();
        }
        logs.push_back(clip(&redact(line, &self.spec.secrets()), LOG_LINE_MAX));
    }

    fn set_state(&self, state: State, error: Option<String>) {
        let mut status = self.status.lock().unwrap();
        status.state = state;
        if error.is_some() {
            status.error = error;
        }
        drop(status);
        self.changed.notify_all();
    }

    fn view(&self) -> Value {
        let status = self.status.lock().unwrap();
        json!({
            "name": self.spec.name,
            "cwd": crate::gate::display(&self.cwd),
            "state": status.state.as_str(),
            "hash": self.spec.hash,
            "command": self.spec.shown(),
            "pid": status.pid,
            "startedAt": status.started_at,
            "restarts": status.restarts,
            "server": status.server,
            "tools": status.tools,
            "error": status.error,
        })
    }

    fn send(&self, message: &Value) -> Result<(), String> {
        let mut proc = self.proc.lock().unwrap();
        let Some(p) = proc.as_mut() else {
            return Err("server is not running".into());
        };
        let mut line = serde_json::to_string(message).map_err(|e| e.to_string())?;
        line.push('\n');
        p.stdin
            .write_all(line.as_bytes())
            .and_then(|_| p.stdin.flush())
            .map_err(|e| e.to_string())
    }

    /// One JSON-RPC request; the answer (or None on deadline / cancel / exit).
    fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx): (Sender<Value>, Receiver<Value>) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) =
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
        {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        loop {
            let now = Instant::now();
            if now >= deadline || cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                self.pending.lock().unwrap().remove(&id);
                let _ = self.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": id, "reason": if now >= deadline { "timeout" } else { "cancelled" }}}));
                return Err(if now >= deadline {
                    "timeout".into()
                } else {
                    "cancelled".into()
                });
            }
            match rx.recv_timeout((deadline - now).min(Duration::from_millis(200))) {
                Ok(message) => {
                    if let Some(e) = message.get("error") {
                        return Err(clip(
                            e.get("message").and_then(Value::as_str).unwrap_or("error"),
                            500,
                        ));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err("the server exited".into()),
            }
        }
    }

    /// Wait until the server is ready (or failed) or the deadline passes.
    fn wait_ready(&self, deadline: Instant, cancel: Option<&AtomicBool>) -> State {
        let mut status = self.status.lock().unwrap();
        loop {
            if !matches!(status.state, State::Starting | State::Restarting) {
                return status.state;
            }
            let now = Instant::now();
            if now >= deadline || cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
                return status.state;
            }
            status = self
                .changed
                .wait_timeout(status, (deadline - now).min(Duration::from_millis(200)))
                .unwrap()
                .0;
        }
    }

    fn kill(&self) {
        if let Some(mut p) = self.proc.lock().unwrap().take() {
            #[cfg(windows)]
            if let Some(job) = p.job.take() {
                job.terminate();
            }
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
        for (_, tx) in self.pending.lock().unwrap().drain() {
            drop(tx);
        }
    }
}

/// Find a command the way a shell would (PATH, and on Windows PATHEXT), plus the usual per-user tool folders
/// (uv's `~/.local/bin`, npm's global folder): the connector may have started before the runtime was installed.
pub fn resolve_command(command: &str) -> Option<PathBuf> {
    let path = Path::new(command);
    let exts: Vec<String> = if cfg!(windows) {
        let mut e = vec![String::new()];
        e.extend(
            [".exe", ".cmd", ".bat", ".com"]
                .iter()
                .map(|s| s.to_string()),
        );
        e
    } else {
        vec![String::new()]
    };
    let found = |p: &Path| -> Option<PathBuf> {
        for ext in &exts {
            let candidate = if ext.is_empty() {
                // Windows runs only files with an extension: `npx` beside `npx.cmd` is a shell script (os error 193).
                if cfg!(windows) && p.extension().is_none() {
                    continue;
                }
                p.to_path_buf()
            } else {
                PathBuf::from(format!("{}{ext}", p.display()))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    };
    if path.is_absolute() || command.contains('/') || command.contains('\\') {
        return found(path);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let home = crate::util::home_dir();
    dirs.push(home.join(".local").join("bin"));
    dirs.push(home.join(".cargo").join("bin"));
    if let Some(appdata) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(appdata).join("npm"));
    }
    if cfg!(windows) {
        dirs.push(PathBuf::from(r"C:\Program Files\nodejs"));
    }
    dirs.iter().find_map(|d| found(&d.join(command)))
}

fn missing_runtime(command: &str) -> DeviceError {
    let base = Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command)
        .to_ascii_lowercase();
    let next = match base.as_str() {
        "uvx" | "uv" => {
            "这台电脑没有装 uv（运行 Python 写的 MCP 服务要用）。请用户按 https://docs.astral.sh/uv/getting-started/installation/ 装上 uv，装好后再试，不用重启 AgentRouter。"
        }
        "npx" | "node" | "npm" => {
            "这台电脑没有装 Node.js（运行 JavaScript 写的 MCP 服务要用）。请用户从 https://nodejs.org 装上 Node.js LTS 后再试。"
        }
        _ => "这台电脑上找不到这个程序。请确认路径，或请用户先装上它。",
    };
    error("MCP_RUNTIME_MISSING", format!("找不到 {command}")).next(next)
}

pub struct McpHost {
    enabled: AtomicBool,
    dir: PathBuf,
    packages: Packages,
    servers: Arc<Mutex<HashMap<String, Arc<Server>>>>,
}

impl McpHost {
    pub fn new(data_dir: &Path) -> McpHost {
        McpHost {
            enabled: AtomicBool::new(false),
            dir: data_dir.join("mcp"),
            // Beside the servers' folders, not in them (a server may be called anything).
            packages: Packages::new(data_dir.join("mcp-packages")),
            servers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `agentrouter mcp on|off`: off stops every server.
    pub fn set(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        if !on {
            self.stop_all();
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// What `info` reports: whether the host is on and which servers run (no tools: `mcp_list` has them).
    pub fn summary(&self) -> Value {
        let servers = self.servers.lock().unwrap();
        let mut list: Vec<Value> = servers
            .values()
            .map(|s| {
                let st = s.status.lock().unwrap();
                json!({"name": s.spec.name, "state": st.state.as_str(), "hash": s.spec.hash, "tools": st.tools.len()})
            })
            .collect();
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Value::Array(list)
    }

    pub fn stop_all(&self) {
        let drained: Vec<Arc<Server>> = self
            .servers
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for s in drained {
            s.stopping.store(true, Ordering::SeqCst);
            s.kill();
            s.set_state(State::Stopped, None);
        }
    }

    fn check(&self, scope: &Scope) -> Result<(), DeviceError> {
        if !self.enabled() {
            return Err(error("MCP_OFF", "这台电脑没有打开本机工具（MCP）").next(NEXT_MCP_OFF));
        }
        match scope.access {
            Access::Readonly | Access::Confirm => Err(DeviceError::denied(
                "这台设备的访问级别不能运行本机工具（要「只限这些文件夹」或「完全访问」）",
            )
            .next("请用户在这台电脑上运行 agentrouter access folders 后再试。")),
            Access::Folders | Access::Full => Ok(()),
        }
    }

    /// The server a call/log/stop names: by name, and by cwd when the same name runs for more than one
    /// session worktree (M1 per-session cwd). Ambiguous without a cwd is an explicit error.
    fn pick(&self, args: &Value) -> Result<Option<Arc<Server>>, DeviceError> {
        let name = str_arg(args, "name");
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let servers = self.servers.lock().unwrap();
        let mut found: Vec<Arc<Server>> = servers
            .values()
            .filter(|s| s.spec.name == name)
            .filter(|s| cwd.is_none_or(|c| same_cwd(&s.cwd, c)))
            .cloned()
            .collect();
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(error(
                "MCP_AMBIGUOUS",
                format!("本机工具 {name} 在多个工作目录里运行"),
            )
            .next("用 mcp_list 看 cwd，并在调用里带上 cwd 指定一个。")),
        }
    }

    /// `mcp_start`: run a server (or keep the one running with the same definition) and wait a while for it.
    /// `cwd` is the workspace directory the server runs in (a project or a session worktree, resolved and
    /// validated by the caller); without it the first linked folder stays the cwd.
    pub fn start(
        &self,
        scope: &Scope,
        args: &Value,
        cancel: &AtomicBool,
        cwd: Option<&Path>,
    ) -> Result<Value, DeviceError> {
        self.check(scope)?;
        let mut spec = Spec::parse(args)?.scoped(&scope.folders)?;
        // A catalog package runs only as the checked file (MCP_PACKAGE_NEEDED: the cloud sends it first).
        self.packages.apply(spec.pin.as_ref(), &mut spec.args)?;
        let wait = args
            .get("waitMs")
            .and_then(Value::as_u64)
            .unwrap_or(WAIT_DEFAULT_MS)
            .min(CALL_MAX_MS);
        // Same tool name in another directory is another process: the key carries the cwd.
        let server_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| scope.folders.first().cloned())
            .unwrap_or_else(|| self.dir.join(&spec.name));
        let key = server_key(&spec.name, &server_cwd);
        let server = {
            let mut servers = self.servers.lock().unwrap();
            match servers.get(&key) {
                Some(s)
                    if s.spec == spec
                        && !matches!(
                            s.status.lock().unwrap().state,
                            State::Crashed | State::Stopped
                        ) =>
                {
                    s.clone()
                }
                existing => {
                    if existing.is_none() && servers.len() >= MAX_SERVERS {
                        return Err(error(
                            "BUSY",
                            format!("一台电脑最多同时运行 {MAX_SERVERS} 个本机工具"),
                        )
                        .next("先用 mcp_stop 停掉不用的。"));
                    }
                    if let Some(old) = servers.remove(&key) {
                        old.stopping.store(true, Ordering::SeqCst);
                        old.kill();
                    }
                    let program = resolve_command(&spec.command)
                        .ok_or_else(|| missing_runtime(&spec.command))?;
                    let dir = self.dir.join(&spec.name);
                    let _ = std::fs::create_dir_all(&dir);
                    // Its data folder in the linked folders it works on stays out of the person's git.
                    for folder in scope.folders.iter().filter(|f| {
                        let shown = crate::gate::display(f);
                        spec.args.iter().any(|a| a.contains(&shown))
                    }) {
                        for name in &spec.keep_out {
                            crate::mcp_pkg::keep_out(folder, name);
                        }
                    }
                    let server = Arc::new(Server {
                        spec,
                        dir,
                        cwd: server_cwd,
                        proc: Mutex::new(None),
                        pending: Mutex::new(HashMap::new()),
                        status: Mutex::new(Status {
                            state: State::Starting,
                            tools: Vec::new(),
                            server: Value::Null,
                            pid: None,
                            started_at: now_ms(),
                            error: None,
                            crashes: VecDeque::new(),
                            restarts: 0,
                            generation: 0,
                            healed: false,
                        }),
                        changed: Condvar::new(),
                        logs: Mutex::new(VecDeque::new()),
                        next_id: AtomicI64::new(1),
                        stopping: AtomicBool::new(false),
                    });
                    if let Err(e) = launch(&server, &program) {
                        return Err(error("MCP_START_FAILED", format!("没能启动：{e}")));
                    }
                    servers.insert(key, server.clone());
                    server
                }
            }
        };
        server.wait_ready(Instant::now() + Duration::from_millis(wait), Some(cancel));
        Ok(server.view())
    }

    /// `mcp_package`: a pinned catalog package the cloud sends; kept only when it is exactly the pinned file.
    pub fn package(&self, scope: &Scope, args: &Value) -> Result<Value, DeviceError> {
        self.check(scope)?;
        self.packages.store(args)
    }

    pub fn stop(&self, args: &Value) -> Result<Value, DeviceError> {
        let name = str_arg(args, "name").to_string();
        let Some(server) = self.pick(args)? else {
            return Ok(json!({"name": name, "stopped": false}));
        };
        server.stopping.store(true, Ordering::SeqCst);
        server.kill();
        server.set_state(State::Stopped, None);
        self.servers
            .lock()
            .unwrap()
            .retain(|_, candidate| !Arc::ptr_eq(candidate, &server));
        Ok(json!({"name": name, "stopped": true}))
    }

    pub fn list(&self) -> Result<Value, DeviceError> {
        let servers = self.servers.lock().unwrap();
        let mut list: Vec<Value> = servers.values().map(|s| s.view()).collect();
        list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"enabled": self.enabled(), "servers": list}))
    }

    pub fn logs(&self, args: &Value) -> Result<Value, DeviceError> {
        let name = str_arg(args, "name");
        let lines = args
            .get("lines")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(LOG_LINES as u64) as usize;
        let Some(server) = self.pick(args)? else {
            return Err(error(
                "MCP_NOT_RUNNING",
                format!("本机工具 {name} 没有在运行"),
            ));
        };
        let logs = server.logs.lock().unwrap();
        let tail: Vec<&String> = logs.iter().skip(logs.len().saturating_sub(lines)).collect();
        Ok(
            json!({"name": name, "cwd": crate::gate::display(&server.cwd), "lines": tail, "dir": crate::gate::display(&server.dir)}),
        )
    }

    /// `mcp_call`: one tool call, forwarded as it is.
    pub fn call(
        &self,
        scope: &Scope,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        self.check(scope)?;
        let name = str_arg(args, "name");
        let tool = str_arg(args, "tool");
        if tool.is_empty() || tool.len() > 128 {
            return Err(error("FAILED", "tool 不能为空"));
        }
        let Some(server) = self.pick(args)? else {
            return Err(error(
                "MCP_NOT_RUNNING",
                format!("本机工具 {name} 没有在运行"),
            ));
        };
        if !str_arg(args, "hash").is_empty() && str_arg(args, "hash") != server.spec.hash {
            return Err(error(
                "MCP_NOT_RUNNING",
                format!("本机工具 {name} 运行的是另一个版本"),
            ));
        }
        let timeout = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(CALL_DEFAULT_MS)
            .clamp(1_000, CALL_MAX_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout);
        match server.wait_ready(deadline, Some(cancel)) {
            State::Ready => {}
            State::Starting | State::Restarting => {
                return Err(
                    error("MCP_STARTING", format!("本机工具 {name} 还在启动")).next(
                        "第一次启动要下载，可能要一两分钟：过一会儿再调；用 mcp_list 看它的状态。",
                    ),
                );
            }
            _ => {
                let why = server
                    .status
                    .lock()
                    .unwrap()
                    .error
                    .clone()
                    .unwrap_or_default();
                return Err(error(
                    "MCP_NOT_RUNNING",
                    format!("本机工具 {name} 没有在运行 {why}"),
                ));
            }
        }
        // Never on the lock screen, UAC or a secure desktop (a server cannot tell; the host can).
        if crate::screen::secure_desktop_active() {
            return Err(error(
                "SECURE_DESKTOP",
                "电脑现在在锁屏、UAC 或安全桌面上，AI 不能在那里操作",
            )
            .next("告诉用户这一步需要他自己在电脑上点。"));
        }
        crate::screen::note_banner(&format!("本机工具 {name} · {}", clip(tool, 60)));
        let arguments = args
            .get("arguments")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let result = server.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            deadline,
            Some(cancel),
        );
        match result {
            Ok(mut value) => {
                redact_value(&mut value, &server.spec.secrets());
                let size = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
                if size > RESULT_MAX {
                    return Err(error("TOO_LARGE", format!("结果太大（{size} 字节）"))
                        .next("换一个返回更少内容的调用（例如截图缩小、只取一部分）。"));
                }
                Ok(value)
            }
            Err(e) if e == "timeout" => Err(error(
                "MCP_TIMEOUT",
                format!("{name}.{tool} 在 {} 秒内没有答复，已取消", timeout / 1000),
            )
            .next("换一个更快的做法，或把事情拆小；不要原样重试。")),
            Err(e) if e == "cancelled" => Err(error("CANCELLED", "已取消")),
            Err(e) => Err(error("MCP_CALL_FAILED", e)),
        }
    }
}

/// Start the process, its readers and the handshake (in the background).
fn launch(server: &Arc<Server>, program: &Path) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(&server.spec.args)
        .current_dir(&server.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &server.spec.env {
        cmd.env(k, v);
    }
    // Python servers print UTF-8; tell them so (Windows consoles default to the ANSI code page).
    cmd.env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let pid = child.id();
    let generation = {
        let mut st = server.status.lock().unwrap();
        st.generation += 1;
        st.pid = Some(pid);
        st.started_at = now_ms();
        st.generation
    };
    #[cfg(windows)]
    let job = win::Job::for_child(&child);
    *server.proc.lock().unwrap() = Some(Proc {
        child,
        stdin,
        #[cfg(windows)]
        job,
        generation,
    });
    server.log(&format!(
        "[agentrouter] started {} (pid {pid})",
        server.spec.shown()
    ));

    let s = server.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            s.log(&line);
        }
    });
    let s = server.clone();
    std::thread::spawn(move || read_stdout(s, stdout, generation));
    let s = server.clone();
    std::thread::spawn(move || handshake(s, generation));
    Ok(())
}

fn read_stdout(server: Arc<Server>, stdout: std::process::ChildStdout, generation: u64) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
            server.log(&format!("[stdout] {trimmed}"));
            continue;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            // An answer to one of ours.
            (None, Some(id)) => {
                if let Some(tx) = id
                    .as_i64()
                    .and_then(|i| server.pending.lock().unwrap().remove(&i))
                {
                    let _ = tx.send(message);
                }
            }
            // The server asks us something: ping is answered, the rest is not offered (no roots, sampling, elicitation).
            (Some(m), Some(id)) => {
                let reply = if m == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported by AgentRouter"}})
                };
                let _ = server.send(&reply);
            }
            (Some("notifications/tools/list_changed"), None) => {
                let s = server.clone();
                std::thread::spawn(move || {
                    if let Ok(tools) = list_tools(&s) {
                        s.status.lock().unwrap().tools = tools;
                    }
                });
            }
            _ => {}
        }
    }
    exited(server, generation);
}

fn list_tools(server: &Server) -> Result<Vec<Value>, String> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let page = server.request(
            "tools/list",
            params,
            Instant::now() + Duration::from_secs(60),
            None,
        )?;
        if let Some(list) = page.get("tools").and_then(Value::as_array) {
            tools.extend(list.iter().take(200 - tools.len().min(200)).cloned());
        }
        cursor = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(String::from);
        if cursor.is_none() || tools.len() >= 200 {
            break;
        }
    }
    Ok(tools)
}

fn handshake(server: Arc<Server>, generation: u64) {
    let deadline = Instant::now() + Duration::from_millis(HANDSHAKE_MS);
    let init = server.request(
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "agentrouter-device", "version": env!("CARGO_PKG_VERSION")},
        }),
        deadline,
        None,
    );
    let current = || server.status.lock().unwrap().generation == generation;
    let init = match init {
        Ok(v) => v,
        Err(e) => {
            if current() && !server.stopping.load(Ordering::SeqCst) {
                server.log(&format!("[agentrouter] initialize failed: {e}"));
                server.set_state(State::Crashed, Some(format!("initialize: {e}")));
                server.kill();
            }
            return;
        }
    };
    let _ = server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    match list_tools(&server) {
        Ok(tools) if current() => {
            let mut st = server.status.lock().unwrap();
            st.tools = tools;
            st.server = init.get("serverInfo").cloned().unwrap_or(Value::Null);
            st.error = None;
            drop(st);
            server.set_state(State::Ready, None);
            server.log("[agentrouter] ready");
        }
        Ok(_) => {}
        Err(e) => {
            if current() {
                server.set_state(State::Crashed, Some(format!("tools/list: {e}")));
                server.kill();
            }
        }
    }
}

/// The process ended: restart it unless it was stopped or keeps crashing.
fn exited(server: Arc<Server>, generation: u64) {
    if server.stopping.load(Ordering::SeqCst)
        || server.status.lock().unwrap().generation != generation
    {
        return;
    }
    // Its output closed: whatever is left of the tree goes too (a server that closed stdout is of no use).
    let taken = {
        let mut proc = server.proc.lock().unwrap();
        if proc.as_ref().is_some_and(|p| p.generation == generation) {
            proc.take()
        } else {
            None
        }
    };
    let code = taken.and_then(|mut p| {
        #[cfg(windows)]
        if let Some(job) = p.job.take() {
            job.terminate();
        }
        let _ = p.child.kill();
        p.child.wait().ok().and_then(|s| s.code())
    });
    for (_, tx) in server.pending.lock().unwrap().drain() {
        drop(tx);
    }
    server.log(&format!("[agentrouter] exited (code {code:?})"));
    if heal_npx(&server) {
        if server.stopping.load(Ordering::SeqCst) {
            return;
        }
        server.status.lock().unwrap().restarts += 1;
        server.set_state(
            State::Restarting,
            Some("cleared a broken npx cache entry".into()),
        );
        match resolve_command(&server.spec.command) {
            Some(program) => {
                if let Err(e) = launch(&server, &program) {
                    server.set_state(State::Crashed, Some(e));
                }
            }
            None => server.set_state(State::Crashed, Some("command not found".into())),
        }
        return;
    }
    let again = {
        let mut st = server.status.lock().unwrap();
        let now = now_ms();
        st.crashes.push_back(now);
        while st
            .crashes
            .front()
            .is_some_and(|t| now - t > RESTART_WINDOW_MS)
        {
            st.crashes.pop_front();
        }
        st.crashes.len() <= RESTARTS_MAX
    };
    if !again {
        server.set_state(
            State::Crashed,
            Some(format!(
                "exited {code:?} more than {RESTARTS_MAX} times in 2 minutes"
            )),
        );
        return;
    }
    server.set_state(State::Restarting, Some(format!("exited {code:?}")));
    let delay = 1000u64 << server.status.lock().unwrap().crashes.len().min(4);
    std::thread::sleep(Duration::from_millis(delay));
    if server.stopping.load(Ordering::SeqCst) {
        return;
    }
    server.status.lock().unwrap().restarts += 1;
    match resolve_command(&server.spec.command) {
        Some(program) => {
            if let Err(e) = launch(&server, &program) {
                server.set_state(State::Crashed, Some(e));
            }
        }
        None => server.set_state(State::Crashed, Some("command not found".into())),
    }
}

/// An npx server that died before it was ready, with a log that says its files are missing: clear only its broken
/// `_npx/<hash>` entry (an install cut midway) and say so; once per server.
fn heal_npx(server: &Server) -> bool {
    let is_npx = Path::new(&server.spec.command)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("npx"));
    {
        let st = server.status.lock().unwrap();
        if !is_npx || st.healed || st.state == State::Ready {
            return false;
        }
    }
    // This start's output only.
    let log: Vec<String> = {
        let logs = server.logs.lock().unwrap();
        let from = logs
            .iter()
            .rposition(|l| l.starts_with("[agentrouter] started"))
            .unwrap_or(0);
        logs.iter().skip(from).cloned().collect()
    };
    let specs: Vec<String> = server
        .spec
        .args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect();
    let package = match &server.spec.pin {
        Some(pin) => pin.package.clone(),
        None => package_of(&server.spec.args),
    };
    let Some(entry) = crate::mcp_pkg::broken_npx_entry(&log, &server.spec.env, &specs, &package)
    else {
        return false;
    };
    server.status.lock().unwrap().healed = true;
    match std::fs::remove_dir_all(&entry) {
        Ok(()) => {
            server.log(&format!(
                "[agentrouter] cleared the broken npx cache entry {} (an install was cut midway); starting once more",
                crate::gate::display(&entry)
            ));
            true
        }
        Err(e) => {
            server.log(&format!(
                "[agentrouter] could not clear the broken npx cache entry {}: {e}",
                crate::gate::display(&entry)
            ));
            false
        }
    }
}

/// The npm package an npx command line runs (`-p <spec>`, else the first package argument), without its version.
fn package_of(args: &[String]) -> String {
    let spec = args
        .iter()
        .position(|a| a == "-p" || a == "--package")
        .and_then(|i| args.get(i + 1))
        .or_else(|| args.iter().find(|a| !a.starts_with('-')))
        .cloned()
        .unwrap_or_default();
    match spec.rfind('@') {
        Some(at) if at > 0 => spec[..at].to_string(),
        _ => spec,
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// The server's whole process tree (uvx → python, npx → node): closed or terminated, it all ends.
    pub struct Job(HANDLE);
    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn for_child(child: &std::process::Child) -> Option<Job> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Job(job))
            }
        }

        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_short_snake_case() {
        assert!(valid_name("windows"));
        assert!(valid_name("win_mcp2"));
        assert!(!valid_name("Windows"));
        assert!(!valid_name("w"));
        assert!(!valid_name("a-b"));
    }

    #[test]
    fn secrets_are_blanked_in_logs_and_results() {
        let spec = Spec::parse(&json!({"name": "demo", "command": "x", "env": {"API_KEY": "s3cr3t-value", "MODE": "fast"}, "secretEnv": ["API_KEY"]})).unwrap();
        assert_eq!(spec.secrets(), vec!["s3cr3t-value".to_string()]);
        let mut v = json!({"content": [{"type": "text", "text": "key=s3cr3t-value mode=fast"}]});
        redact_value(&mut v, &spec.secrets());
        assert_eq!(v["content"][0]["text"], "key=[secret] mode=fast");
        assert!(!spec.shown().contains("s3cr3t"));
    }

    #[test]
    fn linked_folders_are_filled_in_by_the_device() {
        let spec = Spec::parse(
            &json!({"name": "files", "command": "npx", "args": ["-y", "srv@1.0.0", "{{folders}}"]}),
        )
        .unwrap();
        let folders = vec![PathBuf::from("/a/repo"), PathBuf::from("/b/notes")];
        let scoped = spec.clone().scoped(&folders).unwrap();
        assert_eq!(scoped.args[2..].len(), 2);
        assert!(scoped.args[2].ends_with("repo") && scoped.args[3].ends_with("notes"));
        let one = Spec::parse(
            &json!({"name": "git", "command": "uvx", "args": ["--repository", "{{folder}}"]}),
        )
        .unwrap();
        assert!(one.scoped(&folders).unwrap().args[1].ends_with("repo"));
        assert_eq!(spec.scoped(&[]).unwrap_err().code, "MCP_NO_FOLDER");
    }

    #[test]
    fn the_host_is_off_until_the_person_turns_it_on() {
        // Nothing is started here: every case is refused before a process or a folder is made.
        let host = McpHost::new(Path::new("unused"));
        let scope = Scope {
            access: Access::Full,
            folders: vec![],
            deny: vec![],
        };
        let cancel = AtomicBool::new(false);
        let e = host
            .start(
                &scope,
                &json!({"name": "demo", "command": "x"}),
                &cancel,
                None,
            )
            .unwrap_err();
        assert_eq!(e.code, "MCP_OFF");
        host.set(true);
        let ro = Scope {
            access: Access::Readonly,
            folders: vec![],
            deny: vec![],
        };
        assert_eq!(
            host.start(&ro, &json!({"name": "demo", "command": "x"}), &cancel, None)
                .unwrap_err()
                .code,
            "DENIED"
        );
        let e = host
            .start(
                &scope,
                &json!({"name": "demo", "command": "surely-not-a-command-xyz"}),
                &cancel,
                None,
            )
            .unwrap_err();
        assert_eq!(e.code, "MCP_RUNTIME_MISSING");
    }
}
