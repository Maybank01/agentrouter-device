//! The device's side of each request: protocol checks, then the local gate (access level, folders,
//! confirmation), then the action, with every step in the audit log and on the console stream.

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
use crate::consent::{Ask, Confirm};
use crate::gate::{Scope, display, resolve_folders};
use crate::jobs::{EventSink, Jobs, MAX_JOBS, Shell};
use crate::keystore::Identity;
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
    trusted: Mutex<std::collections::HashSet<String>>,
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
            trusted: Mutex::new(std::collections::HashSet::new()),
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
    }

    pub fn access(&self) -> Access {
        self.scope.read().unwrap().access
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
            Ok(value) => self.record(json!({
                "session": session, "action": verified.action, "text": summary, "outcome": "ok",
                "job": value.get("job"), "status": value.get("status"),
            })),
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
            "info" => Ok(json!({"device": self.info(), "jobs": self.jobs.list(session)})),
            "exec" => self.exec(&scope, session, args, cancel),
            "job" => self.job(&scope, session, args, cancel),
            "read_file" => self.read_file(&scope, session, args),
            "write_file" => self.write_file(&scope, session, args, cancel),
            _ => Err(DeviceError::new("UNKNOWN_ACTION", "unknown action")),
        }
    }

    fn ask(
        &self,
        session: &str,
        action: &'static str,
        text: String,
        cancel: &AtomicBool,
    ) -> Result<(), DeviceError> {
        let ask = Ask {
            session: session.to_string(),
            action,
            text,
        };
        if (self.confirm)(&ask, cancel) {
            Ok(())
        } else {
            Err(DeviceError::denied("用户在设备上拒绝了"))
        }
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
        if scope.access != Access::Full {
            self.ask(
                session,
                "exec",
                format!("{command}\n\n位置：{}", display(&cwd)),
                cancel,
            )?;
        }
        let job = self.jobs.start(command, &cwd, session, self.shell)?;
        job.wait(timeout - started.elapsed().as_secs_f64(), cancel);
        Ok(job.view(0))
    }

    fn job(
        &self,
        scope: &Scope,
        session: &str,
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
                        session,
                        "input",
                        format!("{input}\n\n任务：{}", job.command),
                        cancel,
                    )?;
                }
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
                session,
                "write",
                format!(
                    "{shown}（{} 字节{}）",
                    bytes.len(),
                    if append { "，追加" } else { "" }
                ),
                cancel,
            )?;
        }
        if path.is_dir() {
            return Err(DeviceError::new("FAILED", "这是一个文件夹，不是文件"));
        }
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
