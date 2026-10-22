//! `agentrouter mcp-serve host <folder>…`: the SSH-like surface of a link (docs/HOSTD-MCP.md §3.7, owner 2026-10-20
//! 「就和Agent使用SSH同样的感觉会更好吧」). A plain stdio MCP server — commands in the real shell, files by their real
//! path — that the console starts with every link like any other local server: inside the sandbox in folder mode (the
//! OS refuses writes outside the folders), with the person's own rights in 完全访问. It decides nothing itself; the
//! boundary is the sandbox's, audit and checkpoints are the aggregator's (driven by each tool's `readOnlyHint`).
//!
//! It is also what a cloud session bound to this computer uses as its "local machine" (Cloud
//! `docs/development/PROJECT-SERVICE.md` §10.6): DSH's native file, search and command tools reach the computer
//! through these tools, so they answer in the shapes those tools need (batched metadata, ripgrep semantics, …).
//!
//! One module per concern: [`files`] (read, write, stat, list), [`search`] (ripgrep), [`run`] (commands, streamed),
//! [`jobs`] (background commands), [`pty`] (interactive programs),
//! [`info`] (what this computer is), [`watch`] (what changed in the folders), [`out`] (the environment's output
//! folder: long output stays here, a path travels), [`task`] (the file work of a declared space task, Control's).
//! Every call runs on its own thread with a [`Call`]: its progress token (output streams as MCP progress) and its
//! cancel flag (`notifications/cancelled` ends what it runs).

#[cfg(unix)]
mod bridge;
mod files;
mod info;
mod jobs;
mod out;
mod pty;
mod run;
mod search;
mod space;
mod task;
mod watch;

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const PROTOCOL: &str = "2025-06-18";
/// One read or write carries at most this much (a transfer chunk; base64 stays under the 8 MiB link frame).
pub const CHUNK_MAX: u64 = 4 << 20;

/// One tool's listing entry. `read_only` drives the aggregator's checkpoints (§7.2): only tools that change nothing
/// may say so.
fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name, "description": description, "inputSchema": schema,
        "annotations": {"readOnlyHint": read_only, "destructiveHint": !read_only, "openWorldHint": name == "exec"},
    })
}

fn tools() -> Value {
    let mut all = run::tools();
    all.extend(files::tools());
    all.extend(search::tools());
    all.extend(jobs::tools());
    all.extend(pty::tools());
    all.extend(info::tools());
    all.extend(watch::tools());
    all.extend(out::tools());
    all.extend(space::tools());
    all.extend(task::tools());
    #[cfg(unix)]
    all.extend(bridge::tools());
    Value::Array(all)
}

/// What the server knows: the linked folders (the first is home).
struct Host {
    folders: Vec<PathBuf>,
    /// Cancel flags of the calls running now, by JSON-RPC id.
    calls: Mutex<HashMap<String, Arc<AtomicBool>>>,
    jobs: jobs::Jobs,
    /// The environment's output folder (what commands and tool results produced beyond what travels inline).
    out: Arc<out::Out>,
    ptys: pty::Ptys,
    changes: watch::Changes,
    /// On-demand checkouts of space versions (Cloud #2767), by folder (Linux only for now).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    spaces: space::Spaces,
}

type Notify = Arc<dyn Fn(&Value) + Send + Sync>;

/// One running `tools/call`.
struct Call {
    /// `params._meta.progressToken`: present when the caller wants progress (streamed output).
    token: Option<Value>,
    cancelled: Arc<AtomicBool>,
    seq: AtomicU64,
    notify: Notify,
}

impl Call {
    #[cfg(test)]
    fn detached() -> Call {
        Call {
            token: None,
            cancelled: Arc::default(),
            seq: AtomicU64::new(0),
            notify: Arc::new(|_| {}),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Whether anyone listens to progress.
    fn streams(&self) -> bool {
        self.token.is_some()
    }

    /// `notifications/progress` with `message` = the JSON text of `event` (the only free-form field MCP progress
    /// has; the aggregator passes it through unchanged). `progress` counts up.
    fn progress(&self, event: &Value) {
        let Some(token) = &self.token else { return };
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        (self.notify)(
            &json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
                "progressToken": token, "progress": n, "message": event.to_string(),
            }}),
        );
    }
}

fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool) == Some(true)
}

/// A list argument of strings (`paths`), or the single `path` when the list is absent.
fn arg_paths(args: &Value, max: usize) -> Result<Vec<String>, String> {
    let paths: Vec<String> = match args.get("paths").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .map(|p| p.as_str().map(str::to_string).ok_or("paths 只能是字符串"))
            .collect::<Result<_, _>>()?,
        None => arg_str(args, "path")
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
    };
    if paths.is_empty() {
        return Err("缺少 path 或 paths".into());
    }
    if paths.len() > max {
        return Err(format!("一次最多 {max} 个路径"));
    }
    Ok(paths)
}

impl Host {
    fn new(folders: Vec<PathBuf>) -> Host {
        Host {
            folders,
            calls: Mutex::default(),
            jobs: jobs::Jobs::default(),
            out: Arc::default(),
            ptys: pty::Ptys::default(),
            changes: watch::Changes::default(),
            spaces: space::Spaces::default(),
        }
    }

    /// The link is ending: what runs is cancelled, every background job's tree ends.
    fn shut_down(&self) {
        for flag in self.calls.lock().unwrap().values() {
            flag.store(true, Ordering::Relaxed);
        }
        self.jobs.end_all();
        self.ptys.end_all();
        self.changes.stop();
        #[cfg(target_os = "linux")]
        self.spaces.end_all();
        // The output folder stays: a reconnect or a reboot keeps its paths; only the Agent removes files (#102).
    }

    fn home(&self) -> PathBuf {
        self.folders
            .first()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// The paths a call names (`path`, `paths`) that are in the output folder mark their scope used.
    fn note_output_use(&self, args: &Value) {
        let named = args.get("path").and_then(Value::as_str).into_iter().chain(
            args.get("paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        for raw in named.filter(|p| !p.trim().is_empty()) {
            self.out.used(&self.absolute(raw));
        }
    }

    /// The path as named, made absolute: relative ones are inside the first folder.
    fn absolute(&self, raw: &str) -> PathBuf {
        let p = PathBuf::from(raw.trim());
        if p.is_absolute() {
            p
        } else {
            self.home().join(p)
        }
    }

    /// A real absolute path: relative ones are inside the first folder; an existing path is canonical.
    fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        if raw.trim().is_empty() {
            return Err("path 不能是空的".into());
        }
        let p = self.absolute(raw);
        Ok(match std::fs::canonicalize(&p) {
            Ok(real) => PathBuf::from(shown(&real)),
            Err(_) => p,
        })
    }

    /// Where a command runs: `cwd` (default the first folder), an existing folder, or a coded failure that names the
    /// path and what is wrong with it.
    fn work_dir(&self, operation: &'static str, args: &Value) -> Result<PathBuf, Fail> {
        let cwd = match arg_str(args, "cwd") {
            Some(c) if !c.trim().is_empty() => self.resolve(c)?,
            _ => self.home(),
        };
        match std::fs::metadata(&cwd) {
            Ok(meta) if meta.is_dir() => Ok(cwd),
            Ok(_) => Err(io_fail(
                operation,
                "cwd 不是文件夹：",
                &cwd,
                &std::io::Error::from(std::io::ErrorKind::NotADirectory),
                &self.folders,
            )),
            Err(e) => Err(io_fail(operation, "cwd 用不了：", &cwd, &e, &self.folders)),
        }
    }

    fn call(&self, tool: &str, args: &Value, call: &Call) -> Result<Value, Fail> {
        // Reading output files is the scope's last use in the accounting (#102).
        if matches!(tool, "read" | "stat" | "list" | "search") {
            self.note_output_use(args);
        }
        let mut answer = match tool {
            "exec" => self.exec(args, call)?,
            "read" => self.read(args)?,
            "write" => self.write(args)?,
            "remove" => self.remove(args)?,
            "stat" => self.stat(args)?,
            "list" => self.list(args)?,
            "search" => self.search(args, call)?,
            "job_start" => self.job_start(args, call)?,
            "job_read" => self.job_read(args, call)?,
            "job_stop" => self.job_stop(args)?,
            "jobs" => self.jobs_list()?,
            "pty_open" => self.pty_open(args)?,
            "pty_write" => self.pty_write(args)?,
            "pty_read" => self.pty_read(args, call)?,
            "pty_resize" => self.pty_resize(args)?,
            "pty_close" => self.pty_close(args)?,
            "info" => self.info()?,
            "changes" => self.changes(args, call)?,
            "spill_write" => self.spill_write(args)?,
            "out_release" => self.out_release(args)?,
            "out_status" => self.out_status()?,
            "_space_mount" => self.space_mount(args)?,
            "_space_changes" => self.space_changes(args)?,
            "_space_unmount" => self.space_unmount(args)?,
            "_task" => self.task(args)?,
            #[cfg(unix)]
            "_port_bridge" => self.port_bridge(args)?,
            other => return Err(format!("没有这个工具：{other}").into()),
        };
        // A call that may add output carries the folder's accounting once a threshold is crossed (#102): the runner
        // turns it into the environment's `environment.storage_low` event; nothing is deleted here.
        if matches!(tool, "exec" | "job_start" | "job_read" | "spill_write")
            && let Some(low) = self.out.notice()
            && let Some(obj) = answer.as_object_mut()
        {
            obj.insert("storage_low".into(), low);
        }
        Ok(answer)
    }
}

/// A tool's failure: a sentence for the model, and for the ones a client branches on a stable `code` plus data
/// (`structuredContent` of the `isError` result).
#[derive(Debug)]
pub(super) struct Fail {
    message: String,
    code: Option<&'static str>,
    data: Value,
}

impl Fail {
    pub(super) fn coded(code: &'static str, message: String, data: Value) -> Fail {
        Fail {
            message,
            code: Some(code),
            data,
        }
    }
}

/// A file-system step that failed on this computer, in a shape the caller can act on (owner rule "specific errors,
/// never a generic failure"): a stable `code` from the OS error, and as data the component, the operation, the target
/// path, the OS's own words (`cause`), whether a retry can help and what to do next. The message keeps the OS's words
/// whole (Control classifies by them). `folders` are the linked folders: in folder mode the sandbox refuses a write
/// anywhere else (read-only file system / permission denied), and the next step says where writing works.
pub(super) fn io_fail(
    operation: &'static str,
    step: &str,
    path: &Path,
    err: &std::io::Error,
    folders: &[PathBuf],
) -> Fail {
    use std::io::ErrorKind as K;
    let shown_folders = folders.iter().map(|f| shown(f)).collect::<Vec<_>>();
    let (code, retryable, next) = match err.kind() {
        K::NotFound => (
            "NOT_FOUND",
            false,
            "路径里有一段不存在：先用 host__stat 或 host__list 看清楚，再用存在的路径；要连文件夹一起建，用 host__write（它会建好上级文件夹）".to_string(),
        ),
        // AlreadyExists: creating a folder where a file of that name is.
        K::NotADirectory | K::AlreadyExists => (
            "NOT_A_DIRECTORY",
            false,
            "路径中间有一段是文件、不是文件夹：换个路径，或先处理那个文件".to_string(),
        ),
        K::IsADirectory => (
            "IS_A_DIRECTORY",
            false,
            "这里是一个文件夹，不能当文件写：换一个文件名".to_string(),
        ),
        K::PermissionDenied | K::ReadOnlyFilesystem => (
            "PERMISSION_DENIED",
            false,
            format!(
                "不要原样重试：文件夹模式下只能写链接的文件夹（{}）；在那里面也被拒绝，就是这个文件本身的权限，请这台电脑的主人处理",
                shown_folders.join("、")
            ),
        ),
        K::StorageFull | K::QuotaExceeded | K::FileTooLarge => (
            "NO_SPACE",
            false,
            "这台电脑的磁盘或配额满了：请电脑的主人腾出空间（host__out_status 看输出文件夹占了多少）".to_string(),
        ),
        K::Interrupted | K::TimedOut | K::WouldBlock | K::ResourceBusy => (
            "IO_BUSY",
            true,
            "这台电脑上的文件一时忙不过来：稍后再试一次".to_string(),
        ),
        _ => (
            "IO_ERROR",
            false,
            "重试没有用：把这句原文告诉这台电脑的主人".to_string(),
        ),
    };
    Fail::coded(
        code,
        format!("{step} {}：{err}；{next}", shown(path)),
        json!({
            "component": "hostd",
            "operation": operation,
            "path": shown(path),
            "cause": {"message": err.to_string(), "kind": format!("{:?}", err.kind()), "os_error": err.raw_os_error()},
            "retryable": retryable,
            "next": next,
            "folders": shown_folders,
        }),
    )
}

impl From<String> for Fail {
    fn from(message: String) -> Fail {
        Fail {
            message,
            code: None,
            data: Value::Null,
        }
    }
}

impl From<&str> for Fail {
    fn from(message: &str) -> Fail {
        Fail::from(message.to_string())
    }
}

fn shell_name() -> String {
    if cfg!(windows) {
        let shell = agentrouter_device::shell::default_shell();
        if shell.is_pwsh() {
            "pwsh".into()
        } else {
            "powershell".into()
        }
    } else {
        "sh".into()
    }
}

fn answer(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Fields that may carry file bytes: the text copy leaves a big one out.
fn shorten_big(value: &mut Value) {
    match value {
        Value::Object(o) => {
            for (key, v) in o.iter_mut() {
                if key == "data" && v.as_str().is_some_and(|s| s.len() > 64 << 10) {
                    *v = json!("(in structuredContent)");
                } else {
                    shorten_big(v);
                }
            }
        }
        Value::Array(list) => list.iter_mut().for_each(shorten_big),
        _ => {}
    }
}

fn tool_result(outcome: Result<Value, Fail>) -> Value {
    match outcome {
        // The bytes go once (structuredContent): the text copy leaves a big `data` out, so a 4 MiB base64 chunk
        // stays under the 8 MiB link frame.
        Ok(v) => {
            let mut text = v.clone();
            shorten_big(&mut text);
            json!({"content": [{"type": "text", "text": text.to_string()}], "structuredContent": v, "isError": false})
        }
        Err(Fail {
            message,
            code: None,
            ..
        }) => json!({"content": [{"type": "text", "text": message}], "isError": true}),
        Err(Fail {
            message,
            code: Some(code),
            data,
        }) => {
            let mut structured = json!({"code": code, "message": message});
            if let Value::Object(extra) = data {
                structured.as_object_mut().unwrap().extend(extra);
            }
            json!({"content": [{"type": "text", "text": format!("{code}: {message}")}],
                "structuredContent": structured, "isError": true})
        }
    }
}

/// One JSON-RPC line; `None` for notifications. `tools/call` runs on its own thread (calls do not wait for each other).
fn handle(
    host: &Arc<Host>,
    line: &str,
    out: &Arc<Mutex<std::io::Stdout>>,
    running: &mut Vec<std::thread::JoinHandle<()>>,
) -> Option<Value> {
    let m: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}),
            );
        }
    };
    let method = m.get("method").and_then(Value::as_str).unwrap_or("");
    let params = m.get("params").cloned().unwrap_or(json!({}));
    if method == "notifications/cancelled" {
        if let Some(id) = params.get("requestId")
            && let Some(flag) = host.calls.lock().unwrap().get(&id.to_string())
        {
            flag.store(true, Ordering::Relaxed);
        }
        return None;
    }
    let id = m.get("id").cloned().filter(|v| !v.is_null())?;
    match method {
        "initialize" => Some(answer(
            &id,
            json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "agentrouter-host", "version": env!("CARGO_PKG_VERSION")},
            }),
        )),
        "ping" => Some(answer(&id, json!({}))),
        "tools/list" => Some(answer(&id, json!({"tools": tools()}))),
        "tools/call" => {
            let host = host.clone();
            let out = out.clone();
            running.retain(|h| !h.is_finished());
            let key = id.to_string();
            let cancelled = Arc::new(AtomicBool::new(false));
            host.calls
                .lock()
                .unwrap()
                .insert(key.clone(), cancelled.clone());
            let notify: Notify = {
                let out = out.clone();
                Arc::new(move |v: &Value| write_line(&out, v))
            };
            running.push(std::thread::spawn(move || {
                let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let call = Call {
                    token: params
                        .get("_meta")
                        .and_then(|m| m.get("progressToken"))
                        .filter(|t| t.is_string() || t.is_number())
                        .cloned(),
                    cancelled,
                    seq: AtomicU64::new(0),
                    notify,
                };
                let outcome = host.call(tool, &args, &call);
                host.calls.lock().unwrap().remove(&key);
                write_line(&out, &answer(&id, tool_result(outcome)));
            }));
            None
        }
        _ => Some(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}}),
        ),
    }
}

fn write_line(out: &Arc<Mutex<std::io::Stdout>>, value: &Value) {
    let out = out.lock().unwrap();
    let mut lock = out.lock();
    let _ = writeln!(lock, "{value}");
    let _ = lock.flush();
}

/// `agentrouter mcp-serve host <folder>…`.
pub fn run(args: &[String]) -> i32 {
    let host = Arc::new(Host::new(args.iter().map(PathBuf::from).collect()));
    // Stopped by a signal (the console ending, a terminal closing): the same as input closing, then gone.
    {
        let host = host.clone();
        let _ = ctrlc::set_handler(move || {
            host.shut_down();
            std::process::exit(0);
        });
    }
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut running = Vec::new();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle(&host, &line, &out, &mut running) {
            write_line(&out, &reply);
        }
    }
    // Input closed (the link ended): cancel what runs, end the jobs, answer, then end.
    host.shut_down();
    for h in running {
        let _ = h.join();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn temp(name: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap();
        let dir = target
            .join("tmp")
            .join(format!("host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    pub(super) fn host_in(dir: &Path) -> Host {
        Host::new(vec![dir.to_path_buf()])
    }

    #[test]
    fn the_tool_list_is_neutral_and_says_what_changes_nothing() {
        // (name, readOnlyHint): only tools that change nothing may skip the aggregator's checkpoint.
        let expected = [
            ("exec", false),
            ("read", true),
            ("write", false),
            ("remove", false),
            ("stat", true),
            ("list", true),
            ("search", true),
            ("job_start", false),
            ("job_read", true),
            ("job_stop", false),
            ("jobs", true),
            ("pty_open", false),
            ("pty_write", false),
            ("pty_read", true),
            ("pty_resize", false),
            ("pty_close", false),
            ("info", true),
            ("changes", true),
            ("spill_write", false),
            ("out_status", true),
            ("out_release", false),
        ];
        let list = tools();
        // `_…` tools are hostd's own (`_port_bridge`, #136): the aggregator never lists them.
        assert_eq!(
            list.as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "_port_bridge"),
            cfg!(unix)
        );
        let got: Vec<(&str, bool)> = list
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| !t["name"].as_str().unwrap().starts_with('_'))
            .map(|t| {
                (
                    t["name"].as_str().unwrap(),
                    t["annotations"]["readOnlyHint"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn a_big_data_field_goes_once_even_nested() {
        let big = "x".repeat(70 << 10);
        let r = tool_result(Ok(json!({"entries": [{"data": big}]})));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < 1024, "{}", text.len());
        assert_eq!(
            r["structuredContent"]["entries"][0]["data"]
                .as_str()
                .unwrap()
                .len(),
            70 << 10
        );
    }
}
