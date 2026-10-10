//! The device's side of each request: protocol checks, then the local gate (access level, folders,
//! confirmation), then the action, with every step in the audit log and on the console stream.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::allowlist;
use crate::approvals::{Approvals, Outcome, Pending, Run, Settle};
use crate::audit::Audit;
use crate::checkpoint::{Change, Checkpoints};
use crate::config::Access;
use crate::consent::{Ask, Confirm, Decision};
use crate::gate::{Scope, display, inside, resolve_folders};
use crate::jobs::{EventSink, Jobs, MAX_JOBS, Shell};
use crate::keystore::Identity;
use crate::protocol::{DeviceError, ReplayCache, canonical_json, check_request};
use crate::scope_guard;
use crate::util::{b64, b64_decode, clip, hex, now_ms};

mod coding;

const READ_DEFAULT: u64 = 262_144;
const READ_MAX: u64 = 1 << 20;
const WRITE_MAX: usize = 8 << 20;
/// Files up to this size get a hash in read and write results.
const HASH_MAX: u64 = 64 << 20;
/// A conversation counts as "using" the device this long after its last request.
const IN_USE_MS: i64 = 20_000;
/// How long a request waits for the local decision by default, and at most (`approvalWait`).
const APPROVAL_WAIT_DEFAULT: f64 = 10.0;
const APPROVAL_WAIT_MAX: f64 = 40.0;

/// The actions this device understands (reported in `hello`).
pub const ACTIONS: &[&str] = &[
    "info",
    "exec",
    "job",
    "read_file",
    "write_file",
    "edit_file",
    "apply_patch",
    "search",
    "read_files",
    "list_dir",
];

type Console = Arc<Mutex<Option<(Sender<String>, i64)>>>;
/// A line for the person at the terminal (what the cloud just did here).
pub type Echo = Arc<dyn Fn(&str) + Send + Sync>;

/// What 「本对话同类允许」 allowed in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Allowance {
    Kind(String),
    Exact(String),
    Writes,
}

type Allowances = Arc<Mutex<HashMap<String, Vec<Allowance>>>>;

pub struct Device {
    scope: RwLock<Scope>,
    identity: Mutex<Option<Identity>>,
    replay: Mutex<ReplayCache>,
    pub jobs: Arc<Jobs>,
    audit: Arc<Mutex<Audit>>,
    confirm: RwLock<Confirm>,
    console: Console,
    last_use: Mutex<Option<(String, i64)>>,
    /// Conversations the person let run commands freely (full access), until restart or a level change.
    trusted: Arc<Mutex<HashSet<String>>>,
    allowances: Allowances,
    approvals: Approvals,
    checkpoints: Arc<Checkpoints>,
    readonly_commands: AtomicBool,
    foreground: AtomicBool,
    disconnect: Arc<AtomicBool>,
    echo: Arc<Mutex<Option<Echo>>>,
    home: PathBuf,
    shell: Shell,
}

pub struct Options {
    pub data_dir: PathBuf,
    pub access: Access,
    pub folders: Vec<String>,
    pub confirm: Confirm,
    pub home: PathBuf,
    pub shell: Shell,
}

fn emit_to(console: &Console, kind: &str, session: &str, job: Option<&str>, text: &str) {
    if let Some((tx, stream)) = console.lock().unwrap().as_ref() {
        let mut event = json!({"kind": kind, "session": session, "text": clip(text, 8000)});
        if let Some(job) = job {
            event["job"] = json!(job);
        }
        let _ = tx.send(json!({"id": stream, "event": event}).to_string());
    }
}

fn say_to(echo: &Mutex<Option<Echo>>, text: &str) {
    let echo = echo.lock().unwrap().clone();
    if let Some(echo) = echo {
        echo(text);
    }
}

/// SHA-256 (hex) of a whole file, for conflict checks; `None` when missing or too large.
pub fn file_sha256(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > HASH_MAX {
        return None;
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(hex(&hasher.finalize()))
}

/// The base hash a write expects (`""`: the file must not exist yet).
fn base_hash(args: &Value) -> Option<String> {
    args.get("baseHash")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
}

fn conflict(message: &str) -> DeviceError {
    DeviceError::new("CONFLICT", message).next("重新读一遍这个文件，按现在的内容再改。")
}

/// The file must still be what the writer last read.
fn check_base(path: &Path, base: &str) -> Result<(), DeviceError> {
    let exists = path.exists();
    if base.is_empty() {
        return if exists {
            Err(conflict("这个文件已经存在"))
        } else {
            Ok(())
        };
    }
    match file_sha256(path) {
        Some(current) if current == base => Ok(()),
        Some(_) => Err(conflict("文件在你上次读之后被改过了")),
        None if !exists => Err(conflict("文件已经不在了")),
        None => Err(conflict("文件太大，无法比对哈希")),
    }
}

/// Writes through the file tools: never at the read-only level, and never into `.git` below full access
/// (a repository's config and hooks can make later commands run any program).
fn check_writable(scope: &Scope, real: &Path) -> Result<(), DeviceError> {
    if scope.access == Access::Readonly {
        return Err(DeviceError::denied("这台设备设成了只读，不能写文件")
            .next(crate::protocol::NEXT_READONLY));
    }
    if scope.access != Access::Full
        && real
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        return Err(DeviceError::denied("文件工具不能改 .git 文件夹里的东西").next("用 git 命令。"));
    }
    Ok(())
}

/// How long this request waits for the local decision.
fn approval_wait(args: &Value) -> Duration {
    let secs = args
        .get("approvalWait")
        .and_then(Value::as_f64)
        .unwrap_or(APPROVAL_WAIT_DEFAULT)
        .clamp(0.0, APPROVAL_WAIT_MAX);
    // A moment even at 0, so an answer that is already decided (unattended) is not reported as pending.
    Duration::from_secs_f64(secs.max(0.3))
}

/// Identical requests (same conversation, action and arguments, apart from how long to wait).
fn approval_key(session: &str, action: &str, args: &Value) -> String {
    let mut args = args.clone();
    if let Some(map) = args.as_object_mut() {
        map.remove("timeout");
        map.remove("approvalWait");
    }
    format!("{session}\n{action}\n{}", canonical_json(&args))
}

/// The linked folder a path is in (checkpoints are per folder).
fn folder_of(scope: &Scope, path: &Path) -> Option<PathBuf> {
    scope.folders.iter().find(|f| inside(path, f)).cloned()
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

impl Device {
    pub fn new(options: Options) -> Device {
        let console: Console = Arc::new(Mutex::new(None));
        let sink_console = console.clone();
        let sink: EventSink = Arc::new(move |kind, session, job, text| {
            emit_to(&sink_console, kind, session, job, text)
        });
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
            jobs: Arc::new(Jobs::new(&options.data_dir.join("jobs"), sink)),
            audit: Arc::new(Mutex::new(Audit::open(&options.data_dir.join("audit.log")))),
            confirm: RwLock::new(options.confirm),
            console,
            last_use: Mutex::new(None),
            trusted: Arc::new(Mutex::new(HashSet::new())),
            allowances: Arc::new(Mutex::new(HashMap::new())),
            approvals: Approvals::default(),
            checkpoints: Arc::new(Checkpoints::new(&options.data_dir)),
            readonly_commands: AtomicBool::new(false),
            foreground: AtomicBool::new(false),
            disconnect: Arc::new(AtomicBool::new(false)),
            echo: Arc::new(Mutex::new(None)),
            home: options.home,
            shell: options.shell,
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
        let mut scope = self.scope.write().unwrap();
        scope.access = access;
        scope.folders = resolve_folders(folders);
        self.trusted.lock().unwrap().clear();
        self.allowances.lock().unwrap().clear();
    }

    pub fn access(&self) -> Access {
        self.scope.read().unwrap().access
    }

    /// The coding default: read-only commands run without asking at the 「只限这些文件夹」 level.
    pub fn set_readonly_commands(&self, on: bool) {
        self.readonly_commands.store(on, Ordering::SeqCst);
    }

    /// A person is at the terminal (the command line in the foreground).
    pub fn set_foreground(&self, on: bool) {
        self.foreground.store(on, Ordering::SeqCst);
    }

    pub fn set_confirm(&self, confirm: Confirm) {
        *self.confirm.write().unwrap() = confirm;
    }

    pub fn set_echo(&self, echo: Option<Echo>) {
        *self.echo.lock().unwrap() = echo;
    }

    /// The person chose 「断开」 in a local question (once; the caller disconnects).
    pub fn take_disconnect_request(&self) -> bool {
        self.disconnect.swap(false, Ordering::SeqCst)
    }

    pub fn set_console(&self, stream: Option<(Sender<String>, i64)>) {
        *self.console.lock().unwrap() = stream;
    }

    fn emit(&self, kind: &str, session: &str, job: Option<&str>, text: &str) {
        emit_to(&self.console, kind, session, job, text);
    }

    fn say(&self, text: &str) {
        say_to(&self.echo, text);
    }

    pub fn audit_path(&self) -> PathBuf {
        self.audit.lock().unwrap().path().to_path_buf()
    }

    pub fn record(&self, entry: Value) {
        self.audit.lock().unwrap().record(entry);
    }

    /// The conversation using the device right now (running job or a recent request), if any.
    pub fn in_use_by(&self) -> Option<String> {
        if let Some(session) = self.jobs.running_sessions().into_iter().next() {
            return Some(session);
        }
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
            "shell": self.shell.name(),
            "home": display(&self.home),
            "access": scope.access.as_str(),
            "folders": scope.folders.iter().map(|f| display(f)).collect::<Vec<_>>(),
            "app": crate::util::app_kind(),
            "version": env!("CARGO_PKG_VERSION"),
            "maxJobs": MAX_JOBS,
            "actions": ACTIONS,
            "readonlyCommands": self.readonly_commands.load(Ordering::SeqCst) && scope.access == Access::Confirm,
            "scopeGuard": scope.access == Access::Folders,
            "checkpoints": true,
            "foreground": self.foreground.load(Ordering::SeqCst),
        })
    }

    /// Stop everything the AI started here (tray 断开, revocation, quit, Ctrl+C).
    pub fn stop_all(&self, why: &str) {
        self.approvals.withdraw_all();
        self.jobs.kill_all();
        self.trusted.lock().unwrap().clear();
        self.allowances.lock().unwrap().clear();
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
            Ok(value) => {
                self.record(json!({
                    "session": session, "action": verified.action, "text": summary, "outcome": "ok",
                    "job": value.get("job"), "status": value.get("status"),
                }));
                let status = value.get("status").and_then(Value::as_str).unwrap_or("");
                let how = match status {
                    "awaiting_approval" => "等你确认",
                    "running" => "在后台运行",
                    "withdrawn" => "已撤回",
                    _ => "完成",
                };
                if verified.action != "info" {
                    self.say(&format!(
                        "{} {} — {how}",
                        verified.action,
                        clip(&summary, 200)
                    ));
                }
            }
            Err(e) => {
                if e.code == "DENIED" {
                    self.emit(
                        "denied",
                        session,
                        None,
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
            "info" => Ok(json!({
                "device": self.info(),
                "jobs": self.jobs.list(session),
                "approvals": self.approvals.open_for(session),
            })),
            "exec" => self.exec(&scope, session, args, cancel),
            "job" => self.job(&scope, session, args, cancel),
            "read_file" => self.read_file(&scope, session, args),
            "write_file" => self.write_file(&scope, session, args, cancel),
            "edit_file" => self.edit_file(&scope, session, args, cancel),
            "apply_patch" => self.apply_patch(&scope, session, args, cancel),
            "search" => self.search(&scope, session, args),
            "read_files" => self.read_files(&scope, session, args),
            "list_dir" => self.list_dir(&scope, session, args),
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown action")),
        }
    }

    fn allowed(&self, session: &str, test: impl Fn(&Allowance) -> bool) -> bool {
        self.allowances
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|list| list.iter().any(test))
    }

    fn writes_allowed(&self, scope: &Scope, session: &str) -> bool {
        scope.access != Access::Confirm || self.allowed(session, |a| *a == Allowance::Writes)
    }

    /// What happens once a question is settled: 同类允许, trust, disconnect, audit, console.
    fn settle(&self, allowance: Option<Allowance>) -> Settle {
        let (allowances, trusted, disconnect, jobs, audit, console, echo) = (
            self.allowances.clone(),
            self.trusted.clone(),
            self.disconnect.clone(),
            self.jobs.clone(),
            self.audit.clone(),
            self.console.clone(),
            self.echo.clone(),
        );
        Box::new(move |p: &Pending, decision: Decision| {
            if decision == Decision::Session
                && let Some(a) = allowance
            {
                allowances
                    .lock()
                    .unwrap()
                    .entry(p.session.clone())
                    .or_default()
                    .push(a);
            }
            if p.action == "exec_full" && decision.allows() {
                trusted.lock().unwrap().insert(p.session.clone());
            }
            if decision == Decision::Disconnect {
                jobs.kill_all();
                disconnect.store(true, Ordering::SeqCst);
            }
            let outcome = match p.outcome() {
                Outcome::Done(Ok(_)) => "ok".to_string(),
                Outcome::Done(Err(e)) => e.code.to_string(),
                Outcome::Denied { disconnect: true } => "disconnected".into(),
                Outcome::Denied { .. } => "denied".into(),
                Outcome::Unavailable => "unavailable".into(),
                Outcome::Expired => "expired".into(),
                Outcome::Withdrawn => "withdrawn".into(),
                Outcome::Waiting | Outcome::Running => "open".into(),
            };
            let decided = match decision {
                Decision::Once => "once",
                Decision::Session => "session",
                Decision::Deny => "deny",
                Decision::Disconnect => "disconnect",
                Decision::Unavailable => "unavailable",
            };
            audit.lock().unwrap().record(json!({
                "event": "approval", "approval": p.id, "session": p.session, "action": p.action,
                "text": clip(&p.text, 4000), "decision": decided, "outcome": outcome,
            }));
            emit_to(
                &console,
                "approval",
                &p.session,
                Some(&p.id),
                &format!("{outcome}: {}", clip(&p.text, 300)),
            );
            if outcome != "ok" {
                say_to(&echo, &format!("确认 {}：{outcome}", p.id));
            }
        })
    }

    /// Ask on this device (or join the same open question), waiting up to the request's `approvalWait`.
    #[allow(clippy::too_many_arguments)]
    fn gated(
        &self,
        session: &str,
        action: &str,
        args: &Value,
        ask: Ask,
        allowance: Option<Allowance>,
        run: Run,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let key = approval_key(session, action, args);
        let confirm = self.confirm.read().unwrap().clone();
        let settle = self.settle(allowance);
        let p = self.approvals.request(&confirm, ask, key, run, settle);
        p.wait(approval_wait(args), cancel);
        if matches!(p.outcome(), Outcome::Waiting) {
            self.emit(
                "approval",
                session,
                Some(&p.id),
                &format!("waiting: {}", clip(&p.text, 300)),
            );
        }
        p.answer()
    }

    fn exec(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        if command.trim().is_empty() {
            return Err(DeviceError::new("FAILED", "command required"));
        }
        let timeout = args
            .get("timeout")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 600.0);
        if scope.access == Access::Readonly {
            return Err(DeviceError::denied("这台设备设成了只读，不能运行命令")
                .next(crate::protocol::NEXT_READONLY));
        }
        let cwd = match args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            Some(raw) => {
                let path = scope.check(raw)?;
                if !path.is_dir() {
                    return Err(DeviceError::new("NOT_FOUND", "没有这个文件夹"));
                }
                path
            }
            None => scope.default_cwd(&self.home).ok_or_else(|| {
                DeviceError::denied("这台设备没有允许的文件夹")
                    .next(crate::protocol::NEXT_NO_FOLDER)
            })?,
        };
        let started = Instant::now();
        let run: Run = {
            let (jobs, shell, checkpoints) =
                (self.jobs.clone(), self.shell, self.checkpoints.clone());
            let (command, cwd, session) = (command.to_string(), cwd.clone(), session.to_string());
            let (folder, turn) = (folder_of(scope, &cwd), turn_of(args));
            Box::new(move || {
                let checkpoint = folder.and_then(|f| {
                    checkpoints.before(&session, turn.as_deref(), &f, Change::Command)
                });
                jobs.start(&command, &cwd, &session, shell)
                    .map(|job| with_checkpoint(job.view(0), checkpoint))
            })
        };
        let shown_cwd = Some(display(&cwd));
        let value = if scope.access == Access::Full {
            if self.trusted.lock().unwrap().contains(session) {
                run()?
            } else {
                // Full access: no shell starts before the person says yes here, once per conversation.
                let ask = Ask {
                    session: session.to_string(),
                    action: "exec_full",
                    text: command.to_string(),
                    cwd: shown_cwd,
                    kind: Some("这个对话之后的所有命令".into()),
                };
                self.gated(session, "exec", args, ask, None, run, cancel)?
            }
        } else if scope.access == Access::Folders {
            // The default: no questions inside the folders; clear overreach is refused, and only a
            // request that says why it must go beyond them is asked about, once.
            match scope_guard::check(command, &cwd, scope, &self.home) {
                Ok(()) => run()?,
                Err(blocked) => {
                    let Some(reason) = args
                        .get("beyondScope")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|r| !r.is_empty())
                    else {
                        return Err(blocked.error());
                    };
                    let ask = Ask {
                        session: session.to_string(),
                        action: "exec_beyond",
                        text: format!(
                            "{command}\n\nAI 说明的理由：{}\n为什么要问你：这条命令{}",
                            clip(reason, 500),
                            blocked.what
                        ),
                        cwd: shown_cwd,
                        kind: None,
                    };
                    self.gated(session, "exec", args, ask, None, run, cancel)?
                }
            }
        } else {
            let auto = scope.access == Access::Confirm
                && self.readonly_commands.load(Ordering::SeqCst)
                && allowlist::readonly_allowed(command, &cwd, scope);
            let kind = allowlist::command_kind(command);
            let same_kind = self.allowed(session, |a| match a {
                Allowance::Kind(k) => allowlist::matches_kind(command, k),
                Allowance::Exact(c) => c == command,
                Allowance::Writes => false,
            });
            if auto || same_kind {
                run()?
            } else {
                let (allowance, label) = match kind {
                    Some(k) => (
                        Allowance::Kind(k.clone()),
                        format!("本对话里以「{k}」开头的命令"),
                    ),
                    None => (
                        Allowance::Exact(command.to_string()),
                        "本对话里完全相同的这条命令".to_string(),
                    ),
                };
                let ask = Ask {
                    session: session.to_string(),
                    action: "exec",
                    text: command.to_string(),
                    cwd: shown_cwd,
                    kind: Some(label),
                };
                self.gated(session, "exec", args, ask, Some(allowance), run, cancel)?
            }
        };
        Ok(self.finish_exec(value, timeout - started.elapsed().as_secs_f64(), cancel, 0))
    }

    /// A started job's result after waiting what is left of the request's time.
    fn finish_exec(&self, value: Value, remaining: f64, cancel: &AtomicBool, offset: u64) -> Value {
        let Some(job) = value
            .get("job")
            .and_then(Value::as_str)
            .filter(|j| j.starts_with("job_"))
            .and_then(|id| self.jobs.get(id))
        else {
            return value;
        };
        job.wait(remaining, cancel);
        let mut view = job.view(offset);
        for key in ["approval", "checkpoint"] {
            if let Some(v) = value.get(key) {
                view[key] = v.clone();
            }
        }
        view
    }

    fn job(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let id = args.get("job").and_then(Value::as_str).unwrap_or("");
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let timeout = args
            .get("timeout")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .clamp(0.0, 600.0);
        if id.starts_with("apv_") {
            let p = self
                .approvals
                .get(id, session)
                .ok_or_else(|| DeviceError::new("JOB_NOT_FOUND", "no such job"))?;
            let started = Instant::now();
            match action {
                "output" => {}
                "wait" => p.wait(Duration::from_secs_f64(timeout), cancel),
                "kill" => self.approvals.withdraw(&p),
                "input" => {
                    return Err(DeviceError::new(
                        "JOB_ENDED",
                        "这个请求还在等用户确认，没有在运行",
                    ));
                }
                _ => return Err(DeviceError::new("UNKNOWN_ACTION", "unknown job action")),
            }
            let value = p.answer()?;
            let remaining = if action == "wait" {
                timeout - started.elapsed().as_secs_f64()
            } else {
                0.0
            };
            return Ok(self.finish_exec(value, remaining, cancel, offset));
        }
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| DeviceError::new("JOB_NOT_FOUND", "no such job"))?;
        match action {
            "output" => Ok(job.view(offset)),
            "wait" => {
                job.wait(timeout, cancel);
                Ok(job.view(offset))
            }
            "kill" => {
                self.emit("kill", session, Some(&job.id), "");
                job.kill();
                job.wait(5.0, &AtomicBool::new(false));
                Ok(job.view(offset))
            }
            "input" => {
                let input = args.get("input").and_then(Value::as_str).unwrap_or("");
                if scope.access == Access::Readonly {
                    return Err(DeviceError::denied("这台设备设成了只读")
                        .next(crate::protocol::NEXT_READONLY));
                }
                let run: Run = {
                    let (job, input, console, session) = (
                        job.clone(),
                        input.to_string(),
                        self.console.clone(),
                        session.to_string(),
                    );
                    Box::new(move || {
                        job.write_input(&input)?;
                        emit_to(&console, "input", &session, Some(&job.id), &input);
                        Ok(job.view(offset))
                    })
                };
                if scope.access == Access::Confirm {
                    let ask = Ask {
                        session: session.to_string(),
                        action: "input",
                        text: format!("{input}\n\n任务：{}", job.command),
                        cwd: None,
                        kind: None,
                    };
                    self.gated(session, "job", args, ask, None, run, cancel)
                } else {
                    run()
                }
            }
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown job action")),
        }
    }

    fn read_file(&self, scope: &Scope, session: &str, args: &Value) -> Result<Value, DeviceError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
        if path.is_dir() {
            return Err(DeviceError::new("NOT_FOUND", "这是一个文件夹，不是文件")
                .next("用 list_dir 看文件夹里有什么。"));
        }
        let mut file =
            File::open(&path).map_err(|_| DeviceError::new("NOT_FOUND", "没有这个文件"))?;
        scope.check_opened(&file)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(size);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(READ_DEFAULT)
            .clamp(1, READ_MAX);
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.take(limit).read_to_end(&mut bytes))
            .map_err(|e| DeviceError::new("FAILED", format!("could not read: {e}")))?;
        let base64 = args.get("encoding").and_then(Value::as_str) == Some("base64");
        let shown = display(&path);
        self.emit("read", session, None, &shown);
        let mut out = json!({
            "path": shown,
            "size": size,
            "offset": offset,
            "eof": offset + bytes.len() as u64 >= size,
            "encoding": if base64 { "base64" } else { "utf8" },
            "content": if base64 { b64(&bytes) } else { String::from_utf8_lossy(&bytes).into_owned() },
        });
        if let Some(hash) = file_sha256(&path) {
            out["sha256"] = json!(hash);
        }
        Ok(out)
    }

    fn write_file(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        if scope.access == Access::Readonly {
            return Err(DeviceError::denied("这台设备设成了只读，不能写文件")
                .next(crate::protocol::NEXT_READONLY));
        }
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
        check_writable(scope, &path)?;
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        let bytes = if args.get("encoding").and_then(Value::as_str) == Some("base64") {
            b64_decode(content)
                .ok_or_else(|| DeviceError::new("FAILED", "content is not valid base64"))?
        } else {
            content.as_bytes().to_vec()
        };
        if bytes.len() > WRITE_MAX {
            return Err(DeviceError::new("TOO_LARGE", "at most 8 MiB per write"));
        }
        if path.is_dir() {
            return Err(DeviceError::new("FAILED", "这是一个文件夹，不是文件"));
        }
        let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
        let base = base_hash(args);
        if let Some(b) = &base {
            check_base(&path, b)?;
        }
        let shown = display(&path);
        let ask_text = format!(
            "{shown}（{} 字节{}）",
            bytes.len(),
            if append { "，追加" } else { "" }
        );
        let run: Run = {
            let (scope, console, session) =
                (scope.clone(), self.console.clone(), session.to_string());
            let (checkpoints, folder, turn) = (
                self.checkpoints.clone(),
                folder_of(&scope, &path),
                turn_of(args),
            );
            Box::new(move || {
                if let Some(b) = &base {
                    check_base(&path, b)?;
                }
                let checkpoint = folder.and_then(|f| {
                    checkpoints.before(
                        &session,
                        turn.as_deref(),
                        &f,
                        Change::Files(std::slice::from_ref(&path)),
                    )
                });
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        DeviceError::new("FAILED", format!("could not create the folder: {e}"))
                    })?;
                }
                let size = write_checked(&scope, &path, &bytes, append)?;
                emit_to(
                    &console,
                    "write",
                    &session,
                    None,
                    &format!("{shown} ({} bytes)", bytes.len()),
                );
                Ok(with_checkpoint(
                    json!({"path": shown, "size": size, "sha256": file_sha256(&path)}),
                    checkpoint,
                ))
            })
        };
        if self.writes_allowed(scope, session) {
            return run();
        }
        let ask = Ask {
            session: session.to_string(),
            action: "write_file",
            text: ask_text,
            cwd: None,
            kind: Some("本对话里的写文件".into()),
        };
        self.gated(
            session,
            "write_file",
            args,
            ask,
            Some(Allowance::Writes),
            run,
            cancel,
        )
    }
}

/// Open, check where the handle really points, and only then truncate and write.
fn write_checked(
    scope: &Scope,
    path: &Path,
    bytes: &[u8],
    append: bool,
) -> Result<u64, DeviceError> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(false)
        .open(path)
        .map_err(|e| DeviceError::new("FAILED", format!("could not open: {e}")))?;
    scope.check_opened(&file)?;
    if !append {
        file.set_len(0)
            .map_err(|e| DeviceError::new("FAILED", format!("could not write: {e}")))?;
    }
    file.write_all(bytes)
        .and_then(|_| file.flush())
        .map_err(|e| DeviceError::new("FAILED", format!("could not write: {e}")))?;
    Ok(file.metadata().map(|m| m.len()).unwrap_or(0))
}

/// A short description of a request for the audit log (never file contents).
fn summary(action: &str, args: &Value) -> String {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let text = match action {
        "exec" => {
            let cwd = s("cwd");
            if cwd.is_empty() {
                s("command")
            } else {
                format!("{} (in {cwd})", s("command"))
            }
        }
        "job" => format!("{} {}", s("action"), s("job")),
        "read_file" | "list_dir" => s("path"),
        "write_file" => format!("{} ({} chars)", s("path"), s("content").chars().count()),
        "edit_file" => format!(
            "{} ({} edits)",
            s("path"),
            args.get("edits")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        ),
        "apply_patch" => s("patch")
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                ["*** Add File:", "*** Update File:", "*** Delete File:"]
                    .iter()
                    .find_map(|p| l.strip_prefix(p).map(|rest| rest.trim().to_string()))
            })
            .collect::<Vec<_>>()
            .join(", "),
        "search" => {
            let path = s("path");
            if path.is_empty() {
                s("pattern")
            } else {
                format!("{} (in {path})", s("pattern"))
            }
        }
        "read_files" => args
            .get("paths")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    clip(&text, 4000)
}
