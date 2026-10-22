//! Local MCP servers behind the aggregator: their definitions (`link__server_add`), the confined process each one
//! runs as (the sandbox launcher), the official Rust SDK client towards it, and its supervision (restart after a
//! crash, at most 3 times in 2 minutes; stop with the link).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use sha2::Digest;
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
use agentrouter_sandbox::locks::{self, Lock};

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
/// `{{folder}}` (and `{{folders}}` inside a longer value) in an environment value: the first linked folder.
fn fill_folder(value: &str, home: &str) -> String {
    if value.contains("{{folder") {
        value
            .replace("{{folders}}", home)
            .replace("{{folder}}", home)
    } else {
        value.to_string()
    }
}

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
    /// The lock of everything below the pinned package (Cloud #2740): installed once from it, then the installed
    /// program runs instead of `npx`/`uvx` (an older cloud sends none: the launch command runs as before).
    pub lock: Option<Lock>,
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
    /// Credentials it reads from a file whose path is in an environment variable (a cookie jar): kept on this computer
    /// only ([`agentrouter_device::local_secrets`]), never sent to the cloud (Cloud CONNECTIONS §3.2 `local:`).
    pub credentials: Vec<CredentialFile>,
    /// Its tools that only `hostd` runs (log in, refresh a cookie): never listed to the cloud, refused if called.
    pub host_only: Vec<String>,
    /// How the person logs in on this computer (`link__login`).
    pub login: Option<LoginSpec>,
}

/// [`ServerDef::credentials`]: `{"name": "cookie", "fileEnv": <the variable the server reads the file path from>}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialFile {
    pub name: String,
    pub env: String,
}

/// [`ServerDef::login`]: the server's own login tools, run by `hostd` on this computer. `start` answers with a text
/// holding the address to open or scan (`url`, a pattern) and a key (`key`, a pattern whose first group is the key);
/// `check` is then called with `{<keyArg>: key}` and waits until the person has logged in, after which the server
/// has written the credential file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSpec {
    pub credential: String,
    pub start: String,
    pub url: String,
    pub key: Option<String>,
    pub check: Option<String>,
    pub key_arg: String,
}

const CREDENTIALS_MAX: usize = 8;

fn valid_credential(name: &str) -> bool {
    (1..=48).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty() && k.len() <= 128 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn pattern(text: &str, field: &str) -> Result<String, Failure> {
    if text.is_empty() || text.len() > 300 {
        return Err(Failure::new(
            "HOSTD_ARGS_INVALID",
            format!("login.{field} 要是 1–300 个字符的正则"),
        ));
    }
    regex::Regex::new(text)
        .map(|_| text.to_string())
        .map_err(|e| {
            Failure::new(
                "HOSTD_ARGS_INVALID",
                format!("login.{field} 不是有效的正则：{e}"),
            )
        })
}

fn parse_credentials(
    args: &Value,
    env: &[(String, String)],
) -> Result<Vec<CredentialFile>, Failure> {
    let list = args
        .get("credentials")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if list.len() > CREDENTIALS_MAX {
        return Err(Failure::new(
            "HOSTD_ARGS_INVALID",
            format!("credentials 最多 {CREDENTIALS_MAX} 个"),
        ));
    }
    let mut out: Vec<CredentialFile> = Vec::new();
    for c in list {
        let name = str_of(&c, "name").trim().to_string();
        let file_env = str_of(&c, "fileEnv").trim().to_string();
        if !valid_credential(&name) || out.iter().any(|o| o.name == name) {
            return Err(Failure::new(
                "HOSTD_ARGS_INVALID",
                format!("credentials.name {name:?} 要是不重复的小写字母、数字、_ 或 -"),
            ));
        }
        if !valid_env_name(&file_env)
            || reserved_env(&file_env)
            || env.iter().any(|(k, _)| k == &file_env)
        {
            return Err(Failure::new(
                "HOSTD_ARGS_INVALID",
                format!(
                    "credentials.{name}.fileEnv {file_env:?} 要是没在 env 里用过的环境变量名（不能是 AGENTROUTER_ 开头）"
                ),
            ));
        }
        out.push(CredentialFile {
            name,
            env: file_env,
        });
    }
    Ok(out)
}

fn parse_login(
    args: &Value,
    credentials: &[CredentialFile],
    host_only: &mut Vec<String>,
) -> Result<Option<LoginSpec>, Failure> {
    let Some(l) = args.get("login").filter(|v| v.is_object()) else {
        return Ok(None);
    };
    let credential = str_of(l, "credential").trim().to_string();
    if !credentials.iter().any(|c| c.name == credential) {
        return Err(Failure::new(
            "HOSTD_ARGS_INVALID",
            format!("login.credential {credential:?} 不在 credentials 里"),
        ));
    }
    let tool_name = |field: &str| -> Result<Option<String>, Failure> {
        match str_of(l, field).trim() {
            "" => Ok(None),
            t if t.len() <= 128 && !t.starts_with('_') => Ok(Some(t.to_string())),
            t => Err(Failure::new(
                "HOSTD_ARGS_INVALID",
                format!("login.{field} {t:?} 不是工具名"),
            )),
        }
    };
    let Some(start) = tool_name("start")? else {
        return Err(Failure::new(
            "HOSTD_ARGS_INVALID",
            "login.start 要写登录工具的名字",
        ));
    };
    let check = tool_name("check")?;
    let key = match str_of(l, "key") {
        "" => None,
        k => Some(pattern(k, "key")?),
    };
    if check.is_some() && key.is_none() {
        return Err(Failure::new(
            "HOSTD_ARGS_INVALID",
            "login.check 需要 login.key（从 start 的答复里取出的登录码）",
        ));
    }
    let key_arg = match str_of(l, "keyArg").trim() {
        "" => "key".to_string(),
        k if valid_env_name(k) => k.to_string(),
        k => {
            return Err(Failure::new(
                "HOSTD_ARGS_INVALID",
                format!("login.keyArg {k:?} 不是参数名"),
            ));
        }
    };
    // The login tools are hostd's: the cloud never lists or calls them.
    for t in std::iter::once(&start).chain(check.iter()) {
        if !host_only.contains(t) {
            host_only.push(t.clone());
        }
    }
    Ok(Some(LoginSpec {
        credential,
        start,
        url: pattern(str_of(l, "url"), "url")?,
        key,
        check,
        key_arg,
    }))
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
        let lock = Lock::parse(args)
            .map_err(|m| Failure::new(agentrouter_device::errors::HOSTD_ARGS_INVALID, m))?;
        if let Some(lock) = &lock {
            // A lock only ever installs the pinned package it came with.
            let Some(pin) = &pin else {
                return Err(Failure::new(
                    agentrouter_device::errors::HOSTD_ARGS_INVALID,
                    "lock 要和 pin 一起给",
                ));
            };
            let same = lock.package == pin.package && lock.version == pin.version;
            if let Err(why) = lock.pins(&pin.digest).and_then(|()| {
                same.then_some(()).ok_or_else(|| {
                    format!(
                        "lock 是 {} {}，pin 是 {} {}",
                        lock.package, lock.version, pin.package, pin.version
                    )
                })
            }) {
                return Err(Failure::new(
                    agentrouter_device::errors::HOSTD_ARGS_INVALID,
                    format!("lock 和 pin 对不上：{why}"),
                ));
            }
        }
        let title = match str_of(args, "title").trim() {
            "" => name.clone(),
            t => agentrouter_device::util::clip(t, 20),
        };
        let preset = Some(str_of(args, "preset").trim().to_string()).filter(|p| !p.is_empty());
        let credentials = parse_credentials(args, &env)?;
        let mut host_only = strings(args, "hostOnly", 32, 128)?;
        let login = parse_login(args, &credentials, &mut host_only)?;
        Ok(ServerDef {
            name,
            title,
            command,
            args: strings(args, "args", 64, 4000)?,
            env,
            secret_env: strings(args, "secretEnv", 64, 128)?,
            keep_out,
            pin,
            lock,
            network: strings(args, "network", 32, 253)?,
            preset,
            display: args
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hash: agentrouter_device::util::clip(str_of(args, "hash"), 80),
            credentials,
            host_only,
            login,
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
            // A lock that could not be installed (Cloud #2740), or a sandbox that cannot run it.
            State::Unavailable(u)
                if [locks::DIGEST_MISMATCH, locks::INSTALL_FAILED].contains(&u.code) =>
            {
                "install_failed"
            }
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
    /// Its credential files ([`ServerDef::credentials`]): what was last kept, the values to blank, a login under way.
    pub cred: Mutex<CredState>,
}

#[derive(Default)]
pub struct CredState {
    /// Credential name → the file the server reads.
    pub files: Vec<(String, PathBuf)>,
    /// Credential name → sha256 of the value this computer keeps.
    pub kept: std::collections::HashMap<String, Vec<u8>>,
    /// The values (and their parts) blanked in results, logs and the audit.
    pub values: Vec<String>,
    /// A login started with `link__login` and not finished: the credential and since when.
    pub waiting: Option<(String, Instant)>,
    /// The last login that did not finish, in words.
    pub last_error: Option<String>,
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
            cred: Mutex::new(CredState::default()),
        })
    }

    /// Every value to blank: the definition's secret environment and the credentials this computer keeps for it.
    pub fn secrets(&self) -> Vec<String> {
        let mut all = self.def.secrets();
        all.extend(self.cred.lock().unwrap().values.iter().cloned());
        all
    }

    /// The local secret name of one of its credentials (one account per server name on this computer).
    pub fn local_name(&self, credential: &str) -> String {
        format!("{}/{}", self.def.name, credential)
    }

    /// Per credential: `ok` (this computer keeps one), `missing`, or `waiting_login`; never a value.
    pub fn credentials_view(&self) -> Vec<Value> {
        let cred = self.cred.lock().unwrap();
        self.def
            .credentials
            .iter()
            .map(|c| {
                let kept = cred.kept.contains_key(&c.name);
                let state = match &cred.waiting {
                    Some((name, _)) if name == &c.name => "waiting_login",
                    _ if kept => "ok",
                    _ => "missing",
                };
                json!({"name": c.name, "state": state, "lastError": cred.last_error})
            })
            .collect()
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
        for s in self.secrets() {
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

    /// One of its tools only `hostd` runs ([`ServerDef::host_only`]).
    pub fn host_only(&self, tool: &str) -> bool {
        self.def.host_only.iter().any(|t| t == tool)
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
            // The catalog code of a server that cannot run (a sandbox or install failure), for the cloud's error.
            "code": match &state {
                State::Unavailable(u) => Some(u.code),
                _ => None,
            },
            "tools": self.tools.lock().unwrap().len(),
            "restarts": self.restarts.load(Ordering::SeqCst),
            "limitEvents": self.limit_events.lock().unwrap().iter().rev().take(5).map(|(_, l)| l.clone()).collect::<Vec<_>>(),
            "credentials": self.credentials_view(),
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
                // Not logged in yet: the login shows on this computer at once (HOSTD-MCP §13.7), so installing a
                // server that needs an account is enough for the person to see the QR; `link__login` starts it again.
                if let Some(login) = &server.def.login
                    && !server
                        .cred
                        .lock()
                        .unwrap()
                        .kept
                        .contains_key(&login.credential)
                {
                    let (agg, name) = (agg.clone(), server.def.name.clone());
                    tokio::spawn(async move {
                        if let Err(f) = agg.login(&json!({ "name": name })).await {
                            agg.feed_login_problem(&name, &f.message);
                        }
                    });
                }
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
                sync_credentials(&agg, &server);
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
    // A catalog lock (Cloud #2740): installed once outside the sandbox, no package code run; then its program runs.
    let lock = def.lock.as_ref().filter(|_| !def.is_custom());
    let (program, raw_args) = match lock {
        Some(lock) => {
            let sandbox = agg.sandbox.clone();
            let wanted = lock.clone();
            let installed = tokio::task::spawn_blocking(move || sandbox.install_lock(&wanted))
                .await
                .map_err(|e| StartError::Failed(format!("安装没做完：{e}")))?
                .map_err(StartError::Unavailable)?;
            if let Some(from) = &installed.from {
                let line = format!(
                    "installed {}@{} from its lock ({from}) in {:.1}s",
                    lock.package,
                    lock.version,
                    installed.took.as_secs_f64()
                );
                server.log(&format!("[agentrouter] {line}"));
                agg.server_event(server, "installed", Some(line));
            }
            let mut args = installed.args;
            args.extend(lock.args.iter().cloned());
            (installed.program, args)
        }
        None => {
            let program = resolve(&agg.sandbox, &def.command)
                .ok_or_else(|| StartError::Failed(format!("这台电脑上找不到 {}", def.command)))?;
            (program, def.args.clone())
        }
    };
    let mut args = Vec::with_capacity(raw_args.len() + folders.len());
    for a in &raw_args {
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
    if lock.is_none() {
        agg.packages
            .apply(def.pin.as_ref(), &mut args)
            .map_err(|e| StartError::Failed(format!("{}: {}", e.code, e.message)))?;
    }
    for name in &def.keep_out {
        agentrouter_device::mcp_pkg::keep_out(&home, name);
    }
    // A server that takes its data path only from an environment variable (the MCP memory server's MEMORY_FILE_PATH):
    // `{{folder}}` there is the first linked folder too, the one place the folder sandbox lets it write (Cloud #2739).
    let mut env: Vec<(String, String)> = def
        .env
        .iter()
        .map(|(k, v)| (k.clone(), fill_folder(v, &shown(&home))))
        .collect();
    if !env.iter().any(|(k, _)| k.eq_ignore_ascii_case("PATH")) {
        // The device's Node and uv first (`npx`'s `#!/usr/bin/env node` finds the device's Node, #116).
        let path = agg
            .sandbox
            .path_with_runtimes(std::env::var_os("PATH").as_deref());
        env.push(("PATH".into(), path.to_string_lossy().into_owned()));
    }
    prepare_credentials(agg, server, &mut env).map_err(StartError::Failed)?;
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
        key: &sandbox_key(def, &agg.cfg.link),
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

/// The sandbox key of a server on a link: one scratch folder and one policy file per key.
pub fn sandbox_key(def: &ServerDef, link: &str) -> String {
    format!("{}\u{1}{}", def.name, link)
}

/// The values to blank for a credential: the whole text and, for a JSON file or a cookie header, each value of at
/// least 8 characters inside it.
fn credential_values(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes).trim().to_string();
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.len() >= 8 => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Value::Object(m) => m.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        walk(&v, &mut out);
    } else {
        for part in text.split([';', '\n']) {
            if let Some((_, v)) = part.split_once('=') {
                let v = v.trim();
                if v.len() >= 8 {
                    out.push(v.to_string());
                }
            }
        }
    }
    if text.len() >= 8 {
        out.push(text);
    }
    out
}

/// Before a start: write each credential this computer keeps into the server's own scratch folder (owner-only) and
/// put the file's path — never the value — in its environment. A credential not kept yet gets the path only, so
/// the server's own login writes it there.
fn prepare_credentials(
    agg: &Inner,
    server: &Server,
    env: &mut Vec<(String, String)>,
) -> Result<(), String> {
    if server.def.credentials.is_empty() {
        return Ok(());
    }
    let dir = agg
        .sandbox
        .scratch(&sandbox_key(&server.def, &agg.cfg.link))
        .join("credentials");
    let mut cred = server.cred.lock().unwrap();
    cred.files.clear();
    for c in &server.def.credentials {
        let file = dir.join(&c.name);
        let name = server.local_name(&c.name);
        match agentrouter_device::local_secrets::get(&agg.cfg.data_dir, &name) {
            Ok(Some(value)) => {
                agentrouter_device::local_secrets::write_owner_only(&file, &value).map_err(
                    |e| {
                        format!(
                            "credential {} could not be handed to the server: {e}",
                            c.name
                        )
                    },
                )?;
                cred.kept
                    .insert(c.name.clone(), sha2::Sha256::digest(&value).to_vec());
                for v in credential_values(&value) {
                    if !cred.values.contains(&v) {
                        cred.values.push(v);
                    }
                }
            }
            Ok(None) => {
                cred.kept.remove(&c.name);
                let _ = std::fs::remove_file(&file);
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("credential folder {}: {e}", dir.display()))?;
            }
            Err(e) => {
                return Err(format!(
                    "credential {} could not be read on this computer: {e}",
                    c.name
                ));
            }
        }
        env.push((c.env.clone(), file.to_string_lossy().into_owned()));
        cred.files.push((c.name.clone(), file));
    }
    Ok(())
}

/// After a call, a login or an exit: a credential file the server wrote or rewrote (a login, a cookie refresh) is
/// kept on this computer; the cloud hears only its name. Returns the credentials that changed.
pub fn sync_credentials(agg: &Inner, server: &Server) -> Vec<String> {
    let files = server.cred.lock().unwrap().files.clone();
    let mut changed = Vec::new();
    for (name, file) in files {
        let Ok(value) = std::fs::read(&file) else {
            continue;
        };
        if value.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let hash = sha2::Sha256::digest(&value).to_vec();
        if server.cred.lock().unwrap().kept.get(&name) == Some(&hash) {
            continue;
        }
        match agentrouter_device::local_secrets::put(
            &agg.cfg.data_dir,
            &server.local_name(&name),
            &value,
        ) {
            Ok(()) => {
                let mut cred = server.cred.lock().unwrap();
                cred.kept.insert(name.clone(), hash);
                if cred.waiting.as_ref().is_some_and(|(w, _)| w == &name) {
                    cred.waiting = None;
                    cred.last_error = None;
                }
                for v in credential_values(&value) {
                    if !cred.values.contains(&v) {
                        cred.values.push(v);
                    }
                }
                drop(cred);
                server.log(&format!(
                    "[agentrouter] credential {name} kept on this computer"
                ));
                agg.server_event(server, "credential_saved", Some(name.clone()));
                changed.push(name);
            }
            Err(e) => server.log(&format!(
                "[agentrouter] credential {name} could not be kept: {e}"
            )),
        }
    }
    changed
}

/// Forget a credential on this computer (log out) and blank the file the server reads.
pub fn forget_credential(agg: &Inner, server: &Server, name: &str) -> std::io::Result<()> {
    agentrouter_device::local_secrets::remove(&agg.cfg.data_dir, &server.local_name(name))?;
    let mut cred = server.cred.lock().unwrap();
    cred.kept.remove(name);
    if let Some((_, file)) = cred.files.iter().find(|(n, _)| n == name) {
        let _ = std::fs::remove_file(file);
    }
    drop(cred);
    agg.server_event(server, "credential_forgotten", Some(name.to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_env_value_gets_the_first_linked_folder() {
        assert_eq!(
            fill_folder("{{folder}}/.memory/graph.jsonl", "/home/p/work"),
            "/home/p/work/.memory/graph.jsonl"
        );
        assert_eq!(fill_folder("plain", "/home/p/work"), "plain");
    }

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

    fn login_args() -> Value {
        json!({
            "credentials": [{"name": "cookie", "fileEnv": "SITE_COOKIE_FILE"}],
            "hostOnly": ["refresh"],
            "login": {"credential": "cookie", "start": "login", "url": "https://\\S+",
                      "key": "\"loginKey\": \"([^\"]+)\"", "check": "login_check", "keyArg": "loginKey"},
        })
    }

    #[test]
    fn credentials_and_login_parse_and_login_tools_become_host_only() {
        let d = def(login_args());
        assert_eq!(
            d.credentials,
            vec![CredentialFile {
                name: "cookie".into(),
                env: "SITE_COOKIE_FILE".into()
            }]
        );
        let login = d.login.clone().unwrap();
        assert_eq!(
            (
                login.start.as_str(),
                login.check.as_deref(),
                login.key_arg.as_str()
            ),
            ("login", Some("login_check"), "loginKey")
        );
        assert_eq!(
            d.host_only,
            vec!["refresh".to_string(), "login".into(), "login_check".into()]
        );
        let server = Server::new(d, None);
        assert!(
            server.host_only("login")
                && server.host_only("refresh")
                && !server.host_only("video_info")
        );
        assert_eq!(server.local_name("cookie"), "browser/cookie");
    }

    #[test]
    fn bad_credential_and_login_declarations_are_refused_by_field() {
        let refused = |extra: Value| {
            let mut args = json!({"name": "site", "command": "npx"});
            for (k, v) in extra.as_object().unwrap() {
                args[k] = v.clone();
            }
            ServerDef::parse(&args).unwrap_err().message
        };
        assert!(
            refused(json!({"credentials": [{"name": "c", "fileEnv": "AGENTROUTER_X"}]}))
                .contains("fileEnv")
        );
        assert!(
            refused(json!({"env": {"F": "x"}, "credentials": [{"name": "c", "fileEnv": "F"}]}))
                .contains("fileEnv")
        );
        assert!(
            refused(json!({"credentials": [{"name": "C!", "fileEnv": "F"}]}))
                .contains("credentials.name")
        );
        assert!(refused(json!({"credentials": [{"name": "c", "fileEnv": "F"}], "login": {"credential": "x", "start": "l", "url": "u"}})).contains("login.credential"));
        assert!(refused(json!({"credentials": [{"name": "c", "fileEnv": "F"}], "login": {"credential": "c", "start": "l", "url": "("}})).contains("login.url"));
        assert!(refused(json!({"credentials": [{"name": "c", "fileEnv": "F"}], "login": {"credential": "c", "start": "l", "url": "u", "check": "k"}})).contains("login.key"));
    }

    #[test]
    fn credential_values_cover_json_leaves_and_cookie_pairs() {
        let json_values = credential_values(
            br#"{"sessdata": "abcdefgh123", "uid": "42", "nested": {"jct": "0123456789"}}"#,
        );
        assert!(
            json_values.contains(&"abcdefgh123".to_string())
                && json_values.contains(&"0123456789".to_string())
        );
        assert!(!json_values.contains(&"42".to_string()));
        let header = credential_values(b"SESSDATA=abcdefgh123; uid=4");
        assert!(header.contains(&"abcdefgh123".to_string()) && !header.contains(&"4".to_string()));
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
