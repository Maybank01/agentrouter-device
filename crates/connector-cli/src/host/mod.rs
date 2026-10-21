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
//! folder: long output stays here, a path travels).
//! Every call runs on its own thread with a [`Call`]: its progress token (output streams as MCP progress) and its
//! cancel flag (`notifications/cancelled` ends what it runs).

mod files;
mod info;
mod jobs;
mod out;
mod pty;
mod run;
mod search;
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
        self.out.remove_all();
    }

    fn home(&self) -> PathBuf {
        self.folders
            .first()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
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

    fn call(&self, tool: &str, args: &Value, call: &Call) -> Result<Value, Fail> {
        Ok(match tool {
            "exec" => self.exec(args, call)?,
            "read" => self.read(args)?,
            "write" => self.write(args)?,
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
            other => return Err(format!("没有这个工具：{other}").into()),
        })
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
            ("out_release", false),
        ];
        let list = tools();
        let got: Vec<(&str, bool)> = list
            .as_array()
            .unwrap()
            .iter()
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
