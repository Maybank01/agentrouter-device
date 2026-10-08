//! The device's side of each request: protocol checks, then the local gate (access level, folders,
//! confirmation), then the action, with every step in the audit log and on the console stream.
//!
//! Every request also passes the presence gate (`presence.rs`): it is refused while the person has
//! paused AI use, and nothing is done unless the "being controlled" indicator is on screen.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde_json::{Value, json};

use crate::audit::Audit;
use crate::config::Access;
use crate::consent::{Ask, Confirm, Decision, destructive, family};
use crate::gate::{Scope, display, resolve_folders};
use crate::jobs::{EventSink, Jobs, MAX_JOBS, Shell};
use crate::keystore::Identity;
use crate::presence::{Indicator, Presence, Who};
use crate::protocol::{DeviceError, ReplayCache, check_request};
use crate::util::{b64, b64_decode, clip, now_ms};

const READ_DEFAULT: u64 = 262_144;
const READ_MAX: u64 = 1 << 20;
const WRITE_MAX: usize = 8 << 20;
/// A conversation counts as "using" the device this long after its last request.
const IN_USE_MS: i64 = 20_000;

type Console = Arc<Mutex<Option<(Sender<String>, i64)>>>;

pub struct Device {
    scope: RwLock<Scope>,
    identity: Mutex<Option<Identity>>,
    replay: Mutex<ReplayCache>,
    pub jobs: Jobs,
    audit: Mutex<Audit>,
    confirm: Confirm,
    console: Console,
    last_use: Mutex<Option<(String, i64)>>,
    /// Conversations the person let run commands freely (full access), until restart or a level change.
    trusted: Mutex<HashSet<String>>,
    /// (session, kind of command) the person allowed "from now on" in that conversation.
    similar: Mutex<HashSet<(String, String)>>,
    /// Who uses this computer now, pause, and the "being controlled" indicator gate.
    pub presence: Presence,
    home: PathBuf,
    shell: Shell,
}

pub struct Options {
    pub data_dir: PathBuf,
    pub access: Access,
    pub folders: Vec<String>,
    pub confirm: Confirm,
    /// The "being controlled" indicator; requests are refused while it is not on screen.
    pub indicator: Arc<dyn Indicator>,
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

/// What the device is about to do, written by the device itself for the bar's activity line.
fn activity(action: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    match action {
        "exec" => format!("运行：{}", s("command")),
        "read_file" => format!("读取：{}", s("path")),
        "write_file" => format!("写入：{}", s("path")),
        "job" => match s("action") {
            "input" => "给正在跑的命令输入".to_string(),
            "kill" => "停下一个任务".to_string(),
            _ => "看任务的输出".to_string(),
        },
        _ => String::new(),
    }
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
            jobs: Jobs::new(&options.data_dir.join("jobs"), sink),
            audit: Mutex::new(Audit::open(&options.data_dir.join("audit.log"))),
            confirm: options.confirm,
            console,
            last_use: Mutex::new(None),
            trusted: Mutex::new(HashSet::new()),
            similar: Mutex::new(HashSet::new()),
            presence: Presence::new(options.indicator),
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
        self.similar.lock().unwrap().clear();
    }

    pub fn access(&self) -> Access {
        self.scope.read().unwrap().access
    }

    pub fn shell_name(&self) -> &'static str {
        self.shell.name()
    }

    pub fn set_console(&self, stream: Option<(Sender<String>, i64)>) {
        *self.console.lock().unwrap() = stream;
    }

    fn emit(&self, kind: &str, session: &str, job: Option<&str>, text: &str) {
        emit_to(&self.console, kind, session, job, text);
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
        })
    }

    /// Stop everything the AI started here (tray 断开, revocation, quit).
    pub fn stop_all(&self, why: &str) {
        self.jobs.kill_all();
        self.trusted.lock().unwrap().clear();
        self.similar.lock().unwrap().clear();
        self.presence.clear();
        self.record(json!({"event": "stop_all", "reason": why}));
    }

    /// The person ended one session on the bar: its jobs die and it is refused from now on.
    pub fn end_session(&self, session: &str, why: &str) {
        self.jobs.kill_session(session);
        self.trusted.lock().unwrap().remove(session);
        self.similar.lock().unwrap().retain(|(s, _)| s != session);
        self.presence.end(session);
        self.record(
            json!({"event": "session_end", "session": session, "by": "device", "reason": why}),
        );
    }

    /// Pause or resume AI use from the bar: new requests are refused, running commands keep running
    /// (suspending processes is an antivirus red flag, docs/AV-HYGIENE.md); to stop them, disconnect.
    pub fn set_paused(&self, paused: bool) {
        self.presence.set_paused(paused);
        self.record(json!({"event": if paused { "paused" } else { "resumed" }}));
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
        if !verified.share.is_empty() {
            // Share sessions (DEVICE-PROTOCOL.md §11–§13) need the local share table, which this
            // build does not have yet: nothing shared from here is valid.
            let e = DeviceError::new("SHARE_INVALID", "this computer is not shared");
            self.record(json!({"session": verified.session, "action": verified.action, "outcome": e.code, "share": verified.share}));
            return Err(e);
        }
        *self.last_use.lock().unwrap() = Some((verified.session.clone(), now_ms()));
        let mut who = Who::conversation(&verified.session);
        if !verified.client.is_empty() {
            who.client = Some(verified.client.clone());
        }
        self.carry_out(&verified.session, &who, &verified.action, args, cancel)
    }

    /// One call from an AI on this computer through the local MCP (`mcp.rs`): no cloud signature (the
    /// caller is a program of this user, who could run commands anyway), but the same local gate,
    /// confirmations, presence and audit as a cloud request. A local AI only sees its own jobs.
    pub fn serve_local(
        &self,
        session: &str,
        who: &Who,
        action: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        if action == "job"
            && let Some(job) = self
                .jobs
                .get(args.get("job").and_then(Value::as_str).unwrap_or(""))
            && job.session != session
        {
            return Err(DeviceError::new("JOB_NOT_FOUND", "no such job"));
        }
        self.carry_out(session, who, action, args, cancel)
    }

    fn carry_out(
        &self,
        session: &str,
        who: &Who,
        action: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let summary = summary(action, args);
        let base = json!({"session": session, "action": action, "text": summary, "via": who.via, "client": who.client});
        let entry = |extra: Value| {
            let mut e = base.clone();
            if let (Some(e), Some(extra)) = (e.as_object_mut(), extra.as_object()) {
                e.extend(extra.clone());
            }
            e
        };
        let result = match self.presence.begin(session, who) {
            Err(e) => Err(e),
            Ok(fresh) => {
                if fresh {
                    self.record(self.presence.audit_entry("session_start", session, who));
                }
                self.act(action, session, who, args, cancel)
            }
        };
        match &result {
            Ok(value) => self.record(entry(json!({
                "outcome": "ok", "job": value.get("job"), "status": value.get("status"),
            }))),
            Err(e) => {
                if e.code == "DENIED" {
                    self.emit("denied", session, None, &format!("{action}: {}", e.message));
                }
                self.record(entry(json!({"outcome": e.code, "detail": e.message})));
            }
        }
        result
    }

    fn act(
        &self,
        action: &str,
        session: &str,
        who: &Who,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let scope = self.scope.read().unwrap().clone();
        if action == "info" {
            return Ok(json!({"device": self.info(), "jobs": self.jobs.list(session)}));
        }
        // Nothing happens on this computer unless the "being controlled" indicator is on screen.
        self.presence.ensure_shown()?;
        self.presence.note(session, &activity(action, args));
        match action {
            "exec" => self.exec(&scope, session, who, args, cancel),
            "job" => self.job(&scope, session, who, args, cancel),
            "read_file" => self.read_file(&scope, session, args),
            "write_file" => self.write_file(&scope, session, who, args, cancel),
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown action")),
        }
    }

    fn ask(&self, ask: Ask, cancel: &AtomicBool) -> Result<Decision, DeviceError> {
        let _waiting = self.presence.waiting(&ask.session);
        let decision = (self.confirm)(&ask, cancel);
        if decision.allowed() {
            Ok(decision)
        } else {
            Err(DeviceError::denied("用户在设备上拒绝了"))
        }
    }

    fn exec(
        &self,
        scope: &Scope,
        session: &str,
        who: &Who,
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
            return Err(DeviceError::denied("这台设备设成了只读，不能运行命令"));
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
            None => scope
                .default_cwd(&self.home)
                .ok_or_else(|| DeviceError::denied("这台设备没有允许的文件夹"))?,
        };
        let started = Instant::now();
        let deletes = destructive(command);
        let kind = family(command);
        let ask = |action: &'static str| Ask {
            session: session.to_string(),
            action,
            text: command.to_string(),
            place: Some(display(&cwd)),
            destructive: deletes,
            family: if action == "exec" { kind.clone() } else { None },
            who: who.clone(),
        };
        if scope.access != Access::Full {
            let allowed_before = kind.as_ref().is_some_and(|k| {
                self.similar
                    .lock()
                    .unwrap()
                    .contains(&(session.to_string(), k.clone()))
            });
            if !allowed_before {
                let decision = self.ask(ask("exec"), cancel)?;
                if decision == Decision::Similar
                    && let Some(k) = &kind
                {
                    self.similar
                        .lock()
                        .unwrap()
                        .insert((session.to_string(), k.clone()));
                    self.record(json!({"event": "allow_similar", "session": session, "kind": k}));
                }
            }
        } else if !self.trusted.lock().unwrap().contains(session) {
            // Full access: no shell starts before the person says yes here, once per conversation.
            self.ask(ask("exec_full"), cancel)?;
            self.trusted.lock().unwrap().insert(session.to_string());
        }
        // Asking may have taken a while: the indicator must still be up when the shell starts.
        self.presence.ensure_shown()?;
        let _busy = self.presence.busy(session, &format!("运行：{command}"));
        let job = self.jobs.start(command, &cwd, session, self.shell)?;
        job.wait(timeout - started.elapsed().as_secs_f64(), cancel);
        Ok(job.view(0))
    }

    fn job(
        &self,
        scope: &Scope,
        session: &str,
        who: &Who,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        let id = args.get("job").and_then(Value::as_str).unwrap_or("");
        let job = self
            .jobs
            .get(id)
            .ok_or_else(|| DeviceError::new("JOB_NOT_FOUND", "no such job"))?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        match args.get("action").and_then(Value::as_str).unwrap_or("") {
            "output" => Ok(job.view(offset)),
            "wait" => {
                let timeout = args
                    .get("timeout")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .clamp(0.0, 600.0);
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
                    return Err(DeviceError::denied("这台设备设成了只读"));
                }
                if scope.access == Access::Confirm {
                    self.ask(
                        Ask {
                            session: session.to_string(),
                            action: "input",
                            text: input.to_string(),
                            place: Some(job.command.clone()),
                            destructive: false,
                            family: None,
                            who: who.clone(),
                        },
                        cancel,
                    )?;
                    self.presence.ensure_shown()?;
                }
                let _busy = self.presence.busy(session, "给正在跑的命令输入");
                job.write_input(input)?;
                self.emit("input", session, Some(&job.id), input);
                Ok(job.view(offset))
            }
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown job action")),
        }
    }

    fn read_file(&self, scope: &Scope, session: &str, args: &Value) -> Result<Value, DeviceError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
        if path.is_dir() {
            return Err(DeviceError::new("NOT_FOUND", "这是一个文件夹，不是文件"));
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
        Ok(json!({
            "path": shown,
            "size": size,
            "offset": offset,
            "eof": offset + bytes.len() as u64 >= size,
            "encoding": if base64 { "base64" } else { "utf8" },
            "content": if base64 { b64(&bytes) } else { String::from_utf8_lossy(&bytes).into_owned() },
        }))
    }

    fn write_file(
        &self,
        scope: &Scope,
        session: &str,
        who: &Who,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        if scope.access == Access::Readonly {
            return Err(DeviceError::denied("这台设备设成了只读，不能写文件"));
        }
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
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
        let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
        let shown = display(&path);
        if scope.access == Access::Confirm {
            self.ask(
                Ask {
                    session: session.to_string(),
                    action: "write",
                    text: format!(
                        "{shown}（{} 字节{}）",
                        bytes.len(),
                        if append { "，追加" } else { "" }
                    ),
                    place: path.parent().map(display),
                    destructive: false,
                    family: None,
                    who: who.clone(),
                },
                cancel,
            )?;
            self.presence.ensure_shown()?;
        }
        if path.is_dir() {
            return Err(DeviceError::new("FAILED", "这是一个文件夹，不是文件"));
        }
        let _busy = self.presence.busy(session, &format!("写入：{shown}"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DeviceError::new("FAILED", format!("could not create the folder: {e}"))
            })?;
        }
        let size = write_checked(scope, &path, &bytes, append)?;
        self.emit(
            "write",
            session,
            None,
            &format!("{shown} ({} bytes)", bytes.len()),
        );
        Ok(json!({"path": shown, "size": size}))
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
        "read_file" => s("path"),
        "write_file" => format!("{} ({} chars)", s("path"), s("content").chars().count()),
        _ => String::new(),
    };
    clip(&text, 4000)
}
