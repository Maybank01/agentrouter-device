//! Local MCP servers behind the aggregator: their definitions (`link__server_add`), the confined process each one
//! runs as (the sandbox launcher), the official Rust SDK client towards it, and its supervision (restart after a
//! crash, at most 3 times in 2 minutes; stop with the link).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

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
        return Err(Failure::new("FAILED", format!("{key} 最多 {max} 个")));
    }
    list.into_iter()
        .map(|v| match v.as_str() {
            Some(s) if s.len() <= each => Ok(s.to_string()),
            _ => Err(Failure::new("FAILED", format!("{key} 每个都要是字符串"))),
        })
        .collect()
}

impl ServerDef {
    pub fn parse(args: &Value) -> Result<ServerDef, Failure> {
        let name = str_of(args, "name").trim().to_string();
        if !valid_name(&name) {
            return Err(Failure::new(
                "FAILED",
                "name 要写 2–24 个小写字母、数字或 _，以字母开头（不能叫 link）",
            ));
        }
        let command = str_of(args, "command").trim().to_string();
        if command.is_empty() || command.len() > 1000 {
            return Err(Failure::new("FAILED", "command 不能为空"));
        }
        let mut env = Vec::new();
        if let Some(map) = args.get("env").and_then(Value::as_object) {
            if map.len() > 64 {
                return Err(Failure::new("FAILED", "env 最多 64 个"));
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
                    _ => return Err(Failure::new("FAILED", format!("env.{k} 要是字符串"))),
                }
            }
        }
        let keep_out = strings(args, "keepOut", 4, 64)?;
        if !keep_out
            .iter()
            .all(|k| agentrouter_device::mcp_pkg::valid_keep_out(k))
        {
            return Err(Failure::new("FAILED", "keepOut 要是以 . 开头的文件夹名"));
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
    Unavailable(String),
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
    pub stop: CancellationToken,
    pub changed: tokio::sync::Notify,
    pub restarts: AtomicU32,
    pub pid: Mutex<Option<u32>>,
    crashes: Mutex<VecDeque<Instant>>,
}

impl Server {
    pub fn new(def: ServerDef) -> Arc<Server> {
        Arc::new(Server {
            def,
            state: Mutex::new(State::Starting),
            peer: Mutex::new(None),
            tools: Mutex::new(Vec::new()),
            logs: Mutex::new(VecDeque::new()),
            stop: CancellationToken::new(),
            changed: tokio::sync::Notify::new(),
            restarts: AtomicU32::new(0),
            pid: Mutex::new(None),
            crashes: Mutex::new(VecDeque::new()),
        })
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
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= LOG_LINES {
            logs.pop_front();
        }
        logs.push_back(agentrouter_device::util::clip(&text, LOG_LINE_MAX));
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

    pub fn view(&self) -> Value {
        let state = self.state();
        let error = match &state {
            State::Crashed(e) | State::Unavailable(e) => Some(e.clone()),
            _ => None,
        };
        json!({
            "name": self.def.name,
            "title": self.def.title,
            "preset": self.def.preset,
            "hash": self.def.hash,
            "state": state.label(),
            "error": error,
            "tools": self.tools.lock().unwrap().len(),
            "restarts": self.restarts.load(Ordering::SeqCst),
        })
    }
}

/// The SDK client handler of one server: list changes and progress go back to the aggregator (its log lines are
/// its stderr).
pub struct Downstream {
    pub name: String,
    pub agg: Weak<Inner>,
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
}

/// Why one start did not work.
pub enum StartError {
    Unavailable(agentrouter_sandbox::Unavailable),
    Failed(String),
}

/// The program for a command: an absolute path as is, a bare name from PATH (with Windows extensions).
pub fn resolve(command: &str) -> Option<PathBuf> {
    let path = PathBuf::from(command);
    if path.is_absolute() {
        return path.is_file().then_some(path);
    }
    if command.contains(['/', '\\']) {
        return None;
    }
    agentrouter_sandbox::find_program(command, None)
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
                    Ok(tools) => *server.tools.lock().unwrap() = tools,
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
                kill_tree(pid);
                if stopped {
                    break;
                }
                server.log("[agentrouter] exited");
                agg.server_event(&server, "exited", None);
            }
            Err(StartError::Unavailable(u)) => {
                server.set_state(State::Unavailable(u.to_string()));
                agg.server_event(&server, "sandbox_unavailable", Some(u.to_string()));
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
    let program = resolve(&def.command)
        .ok_or_else(|| StartError::Failed(format!("这台电脑上找不到 {}", def.command)))?;
    for name in &def.keep_out {
        agentrouter_device::mcp_pkg::keep_out(&home, name);
    }
    let mut env = def.env.clone();
    if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        && let Some(path) = std::env::var_os("PATH")
    {
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
    };
    let (policy, std_cmd) = if agg.cfg.full {
        // 完全访问 (§6.4): the person's own rights and environment, no sandbox.
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&args).current_dir(&home);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        (json!({"full": true}), cmd)
    } else {
        let policy = agg
            .sandbox
            .policy(&launch)
            .map_err(StartError::Unavailable)?;
        let cmd = agg
            .sandbox
            .command(&launch)
            .map_err(StartError::Unavailable)?;
        (policy, cmd)
    };
    agg.audit_server_start(def, &program, &args, &policy);
    let mut cmd = tokio::process::Command::from(std_cmd);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);
    let (transport, stderr) = rmcp::transport::TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| StartError::Failed(format!("没能启动：{e}")))?;
    let pid = transport.id();
    server.log(&format!(
        "[agentrouter] started {} (pid {})",
        def.shown(),
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
    let handler = Downstream {
        name: def.name.clone(),
        agg: Arc::downgrade(agg),
    };
    let running = tokio::select! {
        r = tokio::time::timeout(HANDSHAKE, handler.serve(transport)) => match r {
            Ok(Ok(running)) => running,
            Ok(Err(e)) => {
                kill_tree(pid);
                return Err(StartError::Failed(format!("MCP 握手失败：{e}")));
            }
            Err(_) => {
                kill_tree(pid);
                return Err(StartError::Failed("MCP 握手超时".into()));
            }
        },
        _ = server.stop.cancelled() => {
            kill_tree(pid);
            return Err(StartError::Failed("已停止".into()));
        }
    };
    Ok((running, pid))
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
}
