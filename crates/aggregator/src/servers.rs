//! Local MCP servers behind the aggregator: their definitions (`link__server_add`), the confined process each one
//! runs as (the sandbox launcher), the official Rust SDK client towards it, and its supervision (restart after a
//! crash, at most 3 times in 2 minutes; stop with the link).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[allow(deprecated)]
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::{
    ClientCapabilities, ClientConfig, Implementation, ProgressNotificationParam, Tool,
};
use rmcp::service::{NotificationContext, Peer, RoleClient};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio_util::sync::CancellationToken;

use agentrouter_device::mcp_pkg::{PACKAGE_ARG, Pin};

use crate::{Failure, Inner};

const NAME_MAX: usize = 24;
const LOG_LINES: usize = 200;
const LOG_LINE_MAX: usize = 500;
/// The first start of a package (uvx / npx download it and a runtime) may take minutes.
const HANDSHAKE: Duration = Duration::from_secs(300);
const RESTARTS_MAX: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(120);
/// How long a stopped server gets to end by itself (input closed) before its process group is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// The most servers one link runs at once.
pub const MAX_SERVERS: usize = 12;
/// Limit events kept per server.
const LIMIT_EVENTS: usize = 20;
/// A server's log file is moved to `<name>.log.1` when it reaches this size at a start (one old file kept).
const LOG_FILE_MAX: u64 = 2 * 1024 * 1024;

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    (2..=NAME_MAX).contains(&name.len())
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name != "link"
}

/// Environment names only the device sets (`AGENTROUTER_*`, any case): a definition from the cloud may not carry them.
pub fn reserved_env(name: &str) -> bool {
    name.len() >= 12 && name[..12].eq_ignore_ascii_case("AGENTROUTER_")
}

/// What `link__server_add` asks to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDef {
    pub name: String,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Environment variables whose values are secrets: blanked in logs, results and the audit.
    pub secret_env: Vec<String>,
    /// Folders the server keeps its data in inside the linked folder (serena's `.serena`): kept out of git.
    pub keep_out: Vec<String>,
    /// A catalog package: runs only as this checked file (`{{package}}`).
    pub pin: Option<Pin>,
    /// Domains its sandbox may reach beside the package mirrors (the catalog's list).
    pub network: Vec<String>,
    /// The catalog preset it comes from.
    pub preset: Option<String>,
    /// It asks to see and drive the screen (device #97): granted only by the person on this computer
    /// ([`crate::grants`]), it then runs with the person's own rights, outside the folder sandbox — any display access
    /// drives every other window, so a sandbox around it would be nominal (docs/HOSTD-MCP.md §6.7).
    pub display: bool,
    /// The cloud's hash of the definition (a different one replaces the running server).
    pub hash: String,
}

fn str_of<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

fn strings(args: &Value, key: &str, max: usize, each: usize) -> Result<Vec<String>, Failure> {
    let list = args
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if list.len() > max {
        return Err(Failure::new(
            agentrouter_device::errors::HOSTD_ARGS_INVALID,
            format!("{key} 最多 {max} 个，给了 {} 个", list.len()),
        ));
    }
    list.into_iter()
        .map(|v| match v.as_str() {
            Some(s) if s.len() <= each => Ok(s.to_string()),
            _ => Err(Failure::new(
                agentrouter_device::errors::HOSTD_ARGS_INVALID,
                format!("{key} 每个都要是不超过 {each} 字节的字符串，收到 {v}"),
            )),
        })
        .collect()
}

impl ServerDef {
    pub fn parse(args: &Value) -> Result<ServerDef, Failure> {
        let name = str_of(args, "name").trim().to_string();
        if !valid_name(&name) {
            return Err(Failure::new(
                agentrouter_device::errors::HOSTD_ARGS_INVALID,
                format!(
                    "name 要写 2–24 个小写字母、数字或 _，以字母开头（不能叫 link），收到 {name:?}"
                ),
            ));
        }
        let command = str_of(args, "command").trim().to_string();
        if command.is_empty() || command.len() > 1000 {
            return Err(Failure::new(
                agentrouter_device::errors::HOSTD_ARGS_INVALID,
                format!(
                    "command 不能为空，最长 1000 字节（收到 {} 字节）",
                    command.len()
                ),
            ));
        }
        let mut env = Vec::new();
        if let Some(map) = args.get("env").and_then(Value::as_object) {
            if map.len() > 64 {
                return Err(Failure::new(
                    agentrouter_device::errors::HOSTD_ARGS_INVALID,
                    format!("env 最多 64 个，给了 {} 个", map.len()),
                ));
            }
            for (k, v) in map {
                let ok_key = !k.is_empty()
                    && k.len() <= 128
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if ok_key && reserved_env(k) {
                    return Err(Failure::new(
                        "ENV_RESERVED",
                        format!("env.{k} 是这台电脑自己的设置，云端不能指定"),
                    ));
                }
                match v.as_str() {
                    Some(s) if ok_key && s.len() <= 8000 => env.push((k.clone(), s.to_string())),
                    _ => {
                        return Err(Failure::new(
                            agentrouter_device::errors::HOSTD_ARGS_INVALID,
                            format!(
                                "env.{k} 的名字只用字母、数字或 _（最长 128），值要是不超过 8000 字节的字符串"
                            ),
                        ));
                    }
                }
            }
        }
        let keep_out = strings(args, "keepOut", 4, 64)?;
        if !keep_out
            .iter()
            .all(|k| agentrouter_device::mcp_pkg::valid_keep_out(k))
        {
            return Err(Failure::new(
                agentrouter_device::errors::HOSTD_ARGS_INVALID,
                format!("keepOut 要是以 . 开头的文件夹名，收到 {keep_out:?}"),
            ));
        }
        let pin = Pin::parse(args).map_err(Failure::from)?;
        let title = match str_of(args, "title").trim() {
            "" => name.clone(),
            t => agentrouter_device::util::clip(t, 20),
        };
        let preset = Some(str_of(args, "preset").trim().to_string()).filter(|p| !p.is_empty());
        Ok(ServerDef {
            name,
            title,
            command,
            args: strings(args, "args", 64, 4000)?,
            env,
            secret_env: strings(args, "secretEnv", 64, 128)?,
            keep_out,
            pin,
            network: strings(args, "network", 32, 253)?,
            preset,
            display: args
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hash: agentrouter_device::util::clip(str_of(args, "hash"), 80),
        })
    }

    /// A catalog preset runs exactly its pinned, digest-checked package through npx/uvx. Anything else is a
    /// custom server: the person answers y/N on this computer first (HOSTD-MCP §5).
    pub fn is_custom(&self) -> bool {
        !(self.preset.is_some()
            && self.pin.is_some()
            && matches!(self.command.as_str(), "npx" | "uvx")
            && self.args.iter().any(|a| a == PACKAGE_ARG))
    }

    /// The person says yes on this computer before it runs (or granted this exact definition before,
    /// [`crate::grants`]): a custom command, or any tool that asks for the display.
    pub fn needs_yes(&self) -> bool {
        self.is_custom() || self.display
    }

    /// It runs outside the folder sandbox, with the person's rights: a display grant (the link's 完全访问 runs every
    /// server so anyway).
    pub fn unconfined(&self) -> bool {
        self.display
    }

    /// Its sandbox may reach the open web: a pinned catalog preset whose catalog list holds
    /// [`agentrouter_sandbox::OPEN_WEB`] (owner 2026-10-20, the browser presets). For a custom server the entry is
    /// dropped and it keeps the package mirrors plus its named domains.
    pub fn open_web(&self) -> bool {
        !self.is_custom()
            && self
                .network
                .iter()
                .any(|d| d == agentrouter_sandbox::OPEN_WEB)
    }

    /// The command line as the person sees it (no environment values).
    pub fn shown(&self) -> String {
        let mut parts = vec![self.command.clone()];
        parts.extend(self.args.iter().cloned());
        let mut text = parts.join(" ");
        if !self.env.is_empty() {
            let names: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
            text.push_str(&format!("（环境变量：{}）", names.join("、")));
        }
        text
    }

    /// The secret values (to blank wherever they would show).
    pub fn secrets(&self) -> Vec<String> {
        self.env
            .iter()
            .filter(|(k, v)| v.len() >= 4 && self.secret_env.iter().any(|s| s == k))
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Blank secret values inside a JSON value.
pub fn redact_value(value: &mut Value, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    match value {
        Value::String(s) => {
            for x in secrets {
                if s.contains(x.as_str()) {
                    *s = s.replace(x.as_str(), "[secret]");
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| redact_value(v, secrets)),
        Value::Object(m) => m.values_mut().for_each(|v| redact_value(v, secrets)),
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Restarting,
    Crashed(String),
    Stopped,
    /// The sandbox cannot run it (never started unconfined).
    Unavailable(agentrouter_sandbox::Unavailable),
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Ready => "ready",
            State::Restarting => "restarting",
            State::Crashed(_) => "crashed",
            State::Stopped => "stopped",
            State::Unavailable(_) => "sandbox_unavailable",
        }
    }
}

pub struct Server {
    pub def: ServerDef,
    pub state: Mutex<State>,
    pub peer: Mutex<Option<Peer<RoleClient>>>,
    pub tools: Mutex<Vec<Tool>>,
    pub logs: Mutex<VecDeque<String>>,
    /// The same lines, appended to `<device data>/logs/server-<name>.log` ([`log_path`]): a server that never gets
    /// ready (no tools, `starting` for the whole task) is diagnosable after the link ended (Cloud #2703).
    file: Mutex<Option<std::fs::File>>,
    pub stop: CancellationToken,
    pub changed: tokio::sync::Notify,
    pub restarts: AtomicU32,
    pub pid: Mutex<Option<u32>>,
    crashes: Mutex<VecDeque<Instant>>,
    /// The environment's resource limits on this server's tree while it runs (device #127); dropping it on Windows
    /// ends the tree.
    pub enforced: Mutex<Option<agentrouter_device::limits::Enforced>>,
    /// When a limit was reached, newest last (at most [`LIMIT_EVENTS`]).
    pub limit_events: Mutex<VecDeque<(Instant, String)>>,
}

/// Where one server's log lines go on disk, under the device data folder (hidden from every sandbox).
pub fn log_path(data_dir: &std::path::Path, name: &str) -> PathBuf {
    data_dir.join("logs").join(format!("server-{name}.log"))
}

/// The server's log file, opened for appending (after moving a full one aside). A failure only means no file.
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_FILE_MAX) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = std::fs::rename(path, old);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

impl Server {
    /// `log_file`: also append its log lines there ([`log_path`]); `None` keeps them in memory only.
    pub fn new(def: ServerDef, log_file: Option<&std::path::Path>) -> Arc<Server> {
        Arc::new(Server {
            def,
            state: Mutex::new(State::Starting),
            peer: Mutex::new(None),
            tools: Mutex::new(Vec::new()),
            logs: Mutex::new(VecDeque::new()),
            file: Mutex::new(log_file.and_then(open_log)),
            stop: CancellationToken::new(),
            changed: tokio::sync::Notify::new(),
            restarts: AtomicU32::new(0),
            pid: Mutex::new(None),
            crashes: Mutex::new(VecDeque::new()),
            enforced: Mutex::new(None),
            limit_events: Mutex::new(VecDeque::new()),
        })
    }

    /// The limit lines reached since `at` (a call's start), oldest first.
    pub fn limits_since(&self, at: Instant) -> Vec<String> {
        self.limit_events
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| *t >= at)
            .map(|(_, l)| l.clone())
            .collect()
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn set_state(&self, state: State) {
        *self.state.lock().unwrap() = state;
        self.changed.notify_waiters();
    }

    pub fn log(&self, line: &str) {
        let mut text = line.to_string();
        for s in self.def.secrets() {
            text = text.replace(s.as_str(), "[secret]");
        }
        let text = agentrouter_device::util::clip(&text, LOG_LINE_MAX);
        if let Some(file) = self.file.lock().unwrap().as_mut() {
            use std::io::Write;
            let _ = writeln!(file, "{} {text}", agentrouter_device::util::iso_now());
        }
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_LINES {
            logs.pop_front();
        }
        logs.push_back(text);
    }

    /// Wait until it is ready or has given up, at most `wait`.
    pub async fn wait_settled(&self, wait: Duration) -> State {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.changed.notified();
            let state = self.state();
            if !matches!(state, State::Starting | State::Restarting) {
                return state;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.state();
            }
        }
    }

    /// What the server listed, without hostd's own `_…` tools (the host server's `_port_bridge`, #136): the one
    /// place for the first list and every `list_changed` refresh.
    pub fn set_tools(&self, tools: Vec<Tool>) {
        *self.tools.lock().unwrap() = tools
            .into_iter()
            .filter(|t| !t.name.starts_with('_'))
            .collect();
    }

    pub fn view(&self) -> Value {
        let state = self.state();
        let error = match &state {
            State::Crashed(e) => Some(e.clone()),
            State::Unavailable(u) => Some(u.to_string()),
            _ => None,
        };
        json!({
            "name": self.def.name,
            "title": self.def.title,
            "preset": self.def.preset,
            "hash": self.def.hash,
            // A display grant: it runs with the person's own rights (「以你的权限运行」), outside the folder sandbox.
            "display": self.def.display,
            "state": state.label(),
            "error": error,
            "tools": self.tools.lock().unwrap().len(),
            "restarts": self.restarts.load(Ordering::SeqCst),
            "limitEvents": self.limit_events.lock().unwrap().iter().rev().take(5).map(|(_, l)| l.clone()).collect::<Vec<_>>(),
        })
    }
}

/// The SDK client handler of one server: list changes and progress go back to the aggregator; its log is its stderr
/// plus the MCP log messages it sends (many servers log only that way, DesktopCommander among them).
pub struct Downstream {
    pub name: String,
    pub agg: Weak<Inner>,
    pub server: Weak<Server>,
}

impl ClientHandler for Downstream {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("agentrouter-link", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        if let Some(agg) = self.agg.upgrade() {
            agg.refresh_tools(&self.name).await;
        }
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        if let Some(agg) = self.agg.upgrade() {
            agg.forward_progress(&self.name, params).await;
        }
    }

    // MCP logging is deprecated (SEP-2577) but servers in use still send it; it is their only log for some.
    #[allow(deprecated)]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        if let Some(server) = self.server.upgrade() {
            let data = match &params.data {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let level = serde_json::to_value(params.level)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            server.log(&format!("[mcp {level}] {data}"));
        }
    }
}

/// `_meta` key on each tool of a display-granted server (device #147), for the platform's own views.
pub const META_DISPLAY: &str = "agentrouter.top/display";

/// Each tool of a display-granted server says so first (device #147). Its own description rarely does (a general
/// tool such as DesktopCommander), and the Agent then looked for the screen in the folder-sandboxed shell, which has
/// none, and never used the tool the person granted it.
pub fn mark_display(tool: &mut Tool, full: bool) {
    let note = if full {
        "[Screen access granted by the person: this tool sees and drives their screen, with their own rights.]"
    } else {
        "[Screen access granted by the person: this tool runs with their own rights and their screen, outside the \
         folder sandbox. Sandboxed commands on this computer have no screen; use this tool for anything that needs it.]"
    };
    tool.description = Some(match tool.description.take() {
        Some(d) if !d.trim().is_empty() => format!("{note} {d}").into(),
        _ => note.into(),
    });
    tool.meta
        .get_or_insert_with(Default::default)
        .0
        .insert(META_DISPLAY.into(), Value::Bool(true));
}

/// Why one start did not work.
pub enum StartError {
    Unavailable(agentrouter_sandbox::Unavailable),
    Failed(String),
}

/// The program for a command: an absolute path as is, a bare name from the device's runtimes (`npx`, `uvx`, `node`,
/// #116) or PATH (with Windows extensions).
pub fn resolve(sandbox: &agentrouter_sandbox::Runtime, command: &str) -> Option<PathBuf> {
    let path = PathBuf::from(command);
    if path.is_absolute() {
        return path.is_file().then_some(path);
    }
    if command.contains(['/', '\\']) {
        return None;
    }
    sandbox.find(command)
}

/// Run one server until it is stopped, restarting it after crashes (at most 3 times in 2 minutes).
pub async fn supervise(agg: Arc<Inner>, server: Arc<Server>) {
    loop {
        if server.stop.is_cancelled() {
            break;
        }
        let started = start_once(&agg, &server).await;
        match started {
            Ok((running, pid)) => {
                *server.pid.lock().unwrap() = pid;
                *server.peer.lock().unwrap() = Some(running.peer().clone());
                match running.peer().list_all_tools().await {
                    Ok(tools) => {
                        server.log(&format!("[agentrouter] ready: {} tools", tools.len()));
                        server.set_tools(tools);
                    }
                    Err(e) => server.log(&format!("[agentrouter] tools/list failed: {e}")),
                }
                server.set_state(State::Ready);
                agg.server_event(&server, "ready", None);
                agg.notify_tools_changed().await;
                let ct = running.cancellation_token();
                let mut waiting = Box::pin(running.waiting());
                let stopped = tokio::select! {
                    _ = &mut waiting => false,
                    _ = server.stop.cancelled() => {
                        // Close its input and give it a moment to end what it started (the host server ends its
                        // background jobs on that) before the process group is killed.
                        ct.cancel();
                        let _ = tokio::time::timeout(STOP_GRACE, &mut waiting).await;
                        true
                    }
                };
                *server.peer.lock().unwrap() = None;
                end(&server, pid);
                if stopped {
                    break;
                }
                server.log("[agentrouter] exited");
                agg.server_event(&server, "exited", None);
            }
            Err(StartError::Unavailable(u)) => {
                agg.server_event(&server, "sandbox_unavailable", Some(u.to_string()));
                server.set_state(State::Unavailable(u));
                return;
            }
            Err(StartError::Failed(e)) => {
                server.log(&format!("[agentrouter] start failed: {e}"));
                agg.server_event(&server, "start_failed", Some(e.clone()));
            }
        }
        // A crash: restart with backoff, unless it keeps crashing.
        let again = {
            let mut crashes = server.crashes.lock().unwrap();
            let now = Instant::now();
            crashes.push_back(now);
            while crashes
                .front()
                .is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW)
            {
                crashes.pop_front();
            }
            crashes.len() <= RESTARTS_MAX
        };
        let had_tools = !server.tools.lock().unwrap().is_empty();
        if !again {
            server.set_state(State::Crashed(format!(
                "2 分钟里退出了超过 {RESTARTS_MAX} 次"
            )));
            server.tools.lock().unwrap().clear();
            if had_tools {
                agg.notify_tools_changed().await;
            }
            return;
        }
        server.set_state(State::Restarting);
        let delay = Duration::from_millis(1000 << server.crashes.lock().unwrap().len().min(4));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = server.stop.cancelled() => break,
        }
        server.restarts.fetch_add(1, Ordering::SeqCst);
    }
    server.tools.lock().unwrap().clear();
    server.set_state(State::Stopped);
    agg.server_event(&server, "stopped", None);
    agg.notify_tools_changed().await;
}

type Running = rmcp::service::RunningService<RoleClient, Downstream>;

async fn start_once(
    agg: &Arc<Inner>,
    server: &Arc<Server>,
) -> Result<(Running, Option<u32>), StartError> {
    let def = &server.def;
    let folders = agg.folders_now();
    let home = agg.home_folder();
    let shown = |p: &std::path::Path| p.to_string_lossy().into_owned();
    // `{{folders}}` alone is every linked folder, one argument each (the filesystem server's allowed directories);
    // inside a longer argument, and `{{folder}}`, it is the first.
    let mut args = Vec::with_capacity(def.args.len() + folders.len());
    for a in &def.args {
        if a == "{{folders}}" {
            args.extend(folders.iter().map(|f| shown(f)));
        } else if a.contains("{{folder") {
            args.push(
                a.replace("{{folders}}", &shown(&home))
                    .replace("{{folder}}", &shown(&home)),
            );
        } else {
            args.push(a.clone());
        }
    }
    agg.packages
        .apply(def.pin.as_ref(), &mut args)
        .map_err(|e| StartError::Failed(format!("{}: {}", e.code, e.message)))?;
    let program = resolve(&agg.sandbox, &def.command)
        .ok_or_else(|| StartError::Failed(format!("这台电脑上找不到 {}", def.command)))?;
    for name in &def.keep_out {
        agentrouter_device::mcp_pkg::keep_out(&home, name);
    }
    let mut env = def.env.clone();
    if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case("PATH")) {
        // The device's Node and uv first (`npx`'s `#!/usr/bin/env node` finds the device's Node, #116).
        let path = agg
            .sandbox
            .path_with_runtimes(std::env::var_os("PATH").as_deref());
        env.push(("PATH".into(), path.to_string_lossy().into_owned()));
    }
    env.push(("PYTHONIOENCODING".into(), "utf-8".into()));
    env.push(("PYTHONUTF8".into(), "1".into()));
    #[cfg(windows)]
    {
        env.retain(|(k, _)| !k.eq_ignore_ascii_case("PSModulePath"));
        env.push((
            "PSModulePath".into(),
            agentrouter_device::util::child_ps_module_path(),
        ));
    }
    let readable = vec![agg.sandbox.packages_dir()];
    // The environment's limits, from outside the sandbox (device #127): on Linux the command runs in its own systemd
    // scope; on Windows a Job object holds the tree (in folder mode one made before the start, which the sandbox entry
    // joins from inside, #129); on macOS the sampler takes it right after the spawn.
    let limits = agg.limits();
    // 完全访问, or a display grant (device #97): the person's own rights, no folder sandbox.
    let unconfined = agg.cfg.full || def.unconfined();
    #[cfg(windows)]
    let prepared = if unconfined {
        None
    } else {
        named_job(agg, server, limits)?
    };
    #[cfg(not(windows))]
    let prepared: Option<(agentrouter_device::limits::Enforced, String)> = None;
    let launch = agentrouter_sandbox::Launch {
        key: &format!("{}\u{1}{}", def.name, agg.cfg.link),
        program: &program,
        args: &args,
        env: &env,
        cwd: &home,
        folders: &folders,
        network: &def.network,
        readable: &readable,
        read_only: agg.cfg.read_only,
        open_web: def.open_web(),
        job: prepared
            .as_ref()
            .zip(agg.cfg.device_program.as_deref())
            .map(|((_, name), program)| agentrouter_sandbox::JobJoin { name, program }),
    };
    let (policy, std_cmd, pipe) = if unconfined {
        // 完全访问 (§6.4) or a display grant (§6.7): the person's own rights and environment, no sandbox. Still the
        // environment's limits, the argument audit and the link's lifetime.
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args).current_dir(&home);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let policy = if agg.cfg.full {
            json!({"full": true})
        } else {
            json!({"display": true, "unconfined": true})
        };
        (policy, cmd, None)
    } else {
        let policy = agg
            .sandbox
            .policy(&launch)
            .map_err(StartError::Unavailable)?;
        let confined = agg
            .sandbox
            .stdio_command(&launch)
            .map_err(StartError::Unavailable)?;
        (policy, confined.command, confined.pipe)
    };
    agg.audit_server_start(def, &program, &args, &policy);
    let unit = unit_name(&def.name);
    let std_cmd = agentrouter_device::limits::command(std_cmd, &unit, &limits);
    let prepared = prepared.map(|(enforced, _)| enforced);
    let limit_it = move |pid: Option<u32>| enforce(agg, server, pid, &unit, limits, prepared);
    let mut cmd = tokio::process::Command::from(std_cmd);
    #[cfg(target_os = "linux")]
    cmd.process_group(0);
    // macOS: a session of its own (also its own process group, as on Linux): a dev server whose starting shell exited
    // keeps the session, which is how a preview stream knows it is the link's (#139, `ports::in_trees`).
    #[cfg(target_os = "macos")]
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.kill_on_drop(true);
    let handler = Downstream {
        name: def.name.clone(),
        agg: Arc::downgrade(agg),
        server: Arc::downgrade(server),
    };
    #[cfg(windows)]
    if let Some(pipe) = pipe {
        return piped::start(server, cmd, pipe, handler, limit_it).await;
    }
    #[cfg(not(windows))]
    let _ = pipe;
    let (transport, stderr) = rmcp::transport::TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| StartError::Failed(format!("没能启动：{e}")))?;
    let pid = transport.id();
    started(server, pid, stderr);
    limit_it(pid)?;
    handshake(server, handler, transport, pid).await
}

/// A systemd unit name for one start of a server (unique on this computer while it runs).
fn unit_name(server: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "agentrouter-{server}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Windows folder mode (device #129): the named Job object the confined entry joins before it starts the server, its
/// limits already set. Without limits a failure is only logged (the server starts; later limits need a restart); with
/// limits the start fails, so it never runs unlimited.
#[cfg(windows)]
fn named_job(
    agg: &Arc<Inner>,
    server: &Arc<Server>,
    limits: agentrouter_device::limits::Limits,
) -> Result<Option<(agentrouter_device::limits::Enforced, String)>, StartError> {
    let made = match &agg.cfg.device_program {
        None => Err("没有 agentrouter 程序来加入作业对象".to_string()),
        Some(_) => agg
            .sandbox
            .windows_sid()
            .map_err(|u| u.to_string())
            .and_then(|sid| {
                agentrouter_device::limits::Enforced::named(&sid, limits, on_limit(agg, server))
            }),
    };
    match made {
        Ok(named) => Ok(Some(named)),
        Err(e) if limits.is_empty() => {
            server.log(&format!(
                "[agentrouter] 资源上限的管控没装上（现在不限，之后设的上限要重启才生效）：{e}"
            ));
            Ok(None)
        }
        Err(e) => Err(StartError::Failed(format!(
            "资源上限（{}）没能生效，所以没有启动：{e}",
            limits.describe()
        ))),
    }
}

/// Where a reached limit goes: the server's log, its recent limit events, and the console/audit event.
fn on_limit(agg: &Arc<Inner>, server: &Arc<Server>) -> agentrouter_device::limits::OnLimit {
    let weak_agg = Arc::downgrade(agg);
    let weak_server = Arc::downgrade(server);
    Arc::new(move |line: String| {
        let (Some(agg), Some(server)) = (weak_agg.upgrade(), weak_server.upgrade()) else {
            return;
        };
        server.log(&format!("[agentrouter] 资源上限：{line}"));
        {
            let mut events = server.limit_events.lock().unwrap();
            if events.len() >= LIMIT_EVENTS {
                events.pop_front();
            }
            events.push_back((Instant::now(), line.clone()));
        }
        agg.server_event(&server, "limit", Some(line));
    })
}

/// Put the just-spawned server under the environment's limits (`prepared`: Windows folder mode's named job, which the
/// server's entry joined itself). Without limits a failure is only logged (nothing to enforce yet; later limits then
/// need a restart); with limits the start fails, so it never runs unlimited.
fn enforce(
    agg: &Arc<Inner>,
    server: &Arc<Server>,
    pid: Option<u32>,
    unit: &str,
    limits: agentrouter_device::limits::Limits,
    prepared: Option<agentrouter_device::limits::Enforced>,
) -> Result<(), StartError> {
    if let Some(enforced) = prepared {
        *server.enforced.lock().unwrap() = Some(enforced);
        return Ok(());
    }
    let Some(pid) = pid else {
        return Ok(());
    };
    match agentrouter_device::limits::Enforced::start(pid, unit, limits, on_limit(agg, server)) {
        Ok(enforced) => {
            *server.enforced.lock().unwrap() = Some(enforced);
            Ok(())
        }
        Err(e) if limits.is_empty() => {
            server.log(&format!(
                "[agentrouter] 资源上限的管控没装上（现在不限，之后设的上限要重启才生效）：{e}"
            ));
            Ok(())
        }
        Err(e) => {
            kill_tree(Some(pid));
            Err(StartError::Failed(format!(
                "资源上限（{}）没能生效，所以没有启动：{e}",
                limits.describe()
            )))
        }
    }
}

/// End a server's tree and its limits (on Windows dropping the Job object ends what is in it).
fn end(server: &Server, pid: Option<u32>) {
    kill_tree(pid);
    server.enforced.lock().unwrap().take();
}

/// The start line and the server's stderr into its log.
fn started(server: &Arc<Server>, pid: Option<u32>, stderr: Option<tokio::process::ChildStderr>) {
    server.log(&format!(
        "[agentrouter] started {} (pid {})",
        server.def.shown(),
        pid.map(|p| p.to_string()).unwrap_or_default()
    ));
    if let Some(stderr) = stderr {
        let s = server.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                s.log(&line);
            }
        });
    }
}

/// MCP `initialize` over the server's transport, within [`HANDSHAKE`] and until the server is stopped.
async fn handshake<T, E, A>(
    server: &Arc<Server>,
    handler: Downstream,
    transport: T,
    pid: Option<u32>,
) -> Result<(Running, Option<u32>), StartError>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let running = tokio::select! {
        r = tokio::time::timeout(HANDSHAKE, handler.serve(transport)) => match r {
            Ok(Ok(running)) => running,
            Ok(Err(e)) => {
                end(server, pid);
                return Err(StartError::Failed(format!("MCP 握手失败：{e}")));
            }
            Err(_) => {
                end(server, pid);
                return Err(StartError::Failed("MCP 握手超时".into()));
            }
        },
        _ = server.stop.cancelled() => {
            end(server, pid);
            return Err(StartError::Failed("已停止".into()));
        }
    };
    server.log("[agentrouter] MCP handshake done; listing tools");
    Ok((running, pid))
}

/// Windows folder mode (#112): the confined server's stdin and stdout travel over the sandbox crate's named pipe,
/// because srt-win gives a sandboxed child an empty stdin. The sandbox entry inside connects to it and relays; the
/// child's own stdout carries nothing and its stderr is the server's log, as elsewhere.
#[cfg(windows)]
mod piped {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::NamedPipeServer;
    use tokio::process::Child;

    use super::{Downstream, HANDSHAKE, Running, Server, StartError, handshake, started};

    /// The pipe as the MCP transport; it owns the confined process, which ends (kill on drop) with the transport.
    struct PipeTransport {
        pipe: NamedPipeServer,
        _child: Child,
    }

    impl AsyncRead for PipeTransport {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().pipe).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for PipeTransport {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.get_mut().pipe).poll_write(cx, buf)
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().pipe).poll_flush(cx)
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.get_mut().pipe).poll_shutdown(cx)
        }
    }

    pub(super) async fn start(
        server: &Arc<Server>,
        mut cmd: tokio::process::Command,
        pipe: agentrouter_sandbox::StdioPipe,
        handler: Downstream,
        enforce: impl FnOnce(Option<u32>) -> Result<(), StartError>,
    ) -> Result<(Running, Option<u32>), StartError> {
        use std::os::windows::io::IntoRawHandle;
        // SAFETY: an overlapped named pipe server end that nothing else owns (agentrouter_sandbox::StdioPipe).
        let pipe = unsafe { NamedPipeServer::from_raw_handle(pipe.handle.into_raw_handle()) }
            .map_err(|e| StartError::Failed(format!("本机工具的输入输出通道打不开：{e}")))?;
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| StartError::Failed(format!("没能启动：{e}")))?;
        let pid = child.id();
        started(server, pid, child.stderr.take());
        enforce(pid)?;
        tokio::select! {
            r = tokio::time::timeout(HANDSHAKE, pipe.connect()) => match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(StartError::Failed(format!("本机工具没连上输入输出通道：{e}"))),
                Err(_) => return Err(StartError::Failed("本机工具没连上输入输出通道（超时）".into())),
            },
            status = child.wait() => {
                let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
                return Err(StartError::Failed(format!("本机工具在连上输入输出通道之前退出了（{status}）")));
            }
            _ = server.stop.cancelled() => return Err(StartError::Failed("已停止".into())),
        }
        handshake(
            server,
            handler,
            PipeTransport {
                pipe,
                _child: child,
            },
            pid,
        )
        .await
    }
}

/// End the server's whole process tree (it runs in its own process group on Unix; on Windows srt runs it as the
/// sandbox account inside srt's own job, which ends with srt).
fn kill_tree(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid
        && pid > 1
    {
        // SAFETY: killpg only sends a signal to the process group this server was started in.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(extra: Value) -> ServerDef {
        let mut args = json!({
            "name": "browser", "preset": "browser", "command": "npx",
            "args": ["-y", PACKAGE_ARG],
            "pin": {"file": "playwright-mcp-0.0.41.tgz", "digest": format!("sha256:{}", "0".repeat(64)),
                    "package": "@playwright/mcp", "version": "0.0.41"},
            "network": ["*"],
        });
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        ServerDef::parse(&args).unwrap()
    }

    #[test]
    fn only_a_pinned_catalog_preset_gets_the_open_web() {
        assert!(def(json!({})).open_web());
        // A preset whose list does not ask for it.
        assert!(!def(json!({"network": ["playwright.dev"]})).open_web());
        // Custom servers: no preset, no pin, or not the pinned package.
        assert!(!def(json!({"preset": ""})).open_web());
        assert!(!def(json!({"pin": null})).open_web());
        assert!(!def(json!({"args": ["-y", "@playwright/mcp@latest"]})).open_web());
        assert!(!def(json!({"command": "node"})).open_web());
    }

    #[test]
    fn a_display_granted_tool_says_so_first() {
        let tool = |description: Option<&str>| -> Tool {
            serde_json::from_value(json!({"name": "start_process", "description": description, "inputSchema": {"type": "object"}}))
                .unwrap()
        };
        let mut t = tool(Some("Start a terminal process."));
        mark_display(&mut t, false);
        let d = t.description.as_deref().unwrap();
        assert!(d.starts_with("[Screen access granted by the person"), "{d}");
        assert!(
            d.contains("outside the folder sandbox") && d.ends_with("] Start a terminal process."),
            "{d}"
        );
        assert_eq!(t.meta.as_ref().unwrap().0[META_DISPLAY], true);
        let mut t = tool(None);
        mark_display(&mut t, true);
        let d = t.description.as_deref().unwrap();
        assert!(
            d.starts_with('[') && d.ends_with(']') && !d.contains("folder sandbox"),
            "{d}"
        );
    }
}
