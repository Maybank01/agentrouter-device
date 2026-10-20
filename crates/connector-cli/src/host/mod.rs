//! `agentrouter mcp-serve host <folder>…`: the SSH-like surface of a link (docs/LINKER-MCP.md §3.7, owner 2026-10-20
//! 「就和Agent使用SSH同样的感觉会更好吧」). A plain stdio MCP server — commands in the real shell, files by their real
//! path — that the console starts with every link like any other local server: inside the sandbox in folder mode (the
//! OS refuses writes outside the folders), with the person's own rights in 完全访问. It decides nothing itself; the
//! boundary is the sandbox's, audit and checkpoints are the aggregator's (driven by each tool's `readOnlyHint`).
//!
//! It is also what a cloud session bound to this computer uses as its "local machine" (Cloud
//! `docs/development/PROJECT-SERVICE.md` §10.6): DSH's native file, search and command tools reach the computer
//! through these tools, so they answer in the shapes those tools need (batched metadata, ripgrep semantics, …).
//!
//! One module per concern: [`files`] (read, write, stat, list), [`search`] (ripgrep).

mod files;
mod search;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use agentrouter_device::exec;

const PROTOCOL: &str = "2025-06-18";
/// One read or write carries at most this much (a transfer chunk; base64 stays under the 8 MiB link frame).
pub const CHUNK_MAX: u64 = 4 << 20;
const TIMEOUT_DEFAULT_S: u64 = 120;

/// One tool's listing entry. `read_only` drives the aggregator's checkpoints (§7.2): only tools that change nothing
/// may say so.
fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Value {
    json!({
        "name": name, "description": description, "inputSchema": schema,
        "annotations": {"readOnlyHint": read_only, "destructiveHint": !read_only, "openWorldHint": name == "exec"},
    })
}

fn tools() -> Value {
    let mut all = vec![tool(
        "exec",
        "Run a command in this computer's shell (/bin/sh -lc on Linux/macOS, PowerShell on Windows), like over ssh. cwd defaults to the link's first folder; paths are real.",
        json!({"type": "object", "properties": {
            "command": {"type": "string"},
            "cwd": {"type": "string"},
            "timeout_s": {"type": "integer", "minimum": 1, "maximum": exec::TIMEOUT_MAX_MS / 1000},
        }, "required": ["command"]}),
        false,
    )];
    all.extend(files::tools());
    all.extend(search::tools());
    Value::Array(all)
}

/// What the server knows: the linked folders (the first is home).
struct Host {
    folders: Vec<PathBuf>,
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

    fn call(&self, tool: &str, args: &Value) -> Result<Value, String> {
        match tool {
            "exec" => self.exec(args),
            "read" => self.read(args),
            "write" => self.write(args),
            "stat" => self.stat(args),
            "list" => self.list(args),
            "search" => self.search(args),
            other => Err(format!("没有这个工具：{other}")),
        }
    }

    fn exec(&self, args: &Value) -> Result<Value, String> {
        let command = arg_str(args, "command").unwrap_or("");
        if command.trim().is_empty() {
            return Err("command 不能是空的".into());
        }
        let cwd = match arg_str(args, "cwd") {
            Some(c) if !c.trim().is_empty() => self.resolve(c)?,
            _ => self.home(),
        };
        if !cwd.is_dir() {
            return Err(format!("{} 不是一个文件夹", shown(&cwd)));
        }
        let timeout = args
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(TIMEOUT_DEFAULT_S)
            .clamp(1, exec::TIMEOUT_MAX_MS / 1000);
        let out = exec::run(command, &cwd, Duration::from_secs(timeout))?;
        Ok(json!({
            "exit_code": out.exit_code,
            "timed_out": out.timed_out,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "cwd": shown(&cwd),
            "shell": shell_name(),
            "ms": out.duration_ms,
        }))
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

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        // The bytes go once (structuredContent): the text copy leaves a big `data` out, so a 4 MiB base64 chunk
        // stays under the 8 MiB link frame.
        Ok(v) => {
            let mut text = v.clone();
            shorten_big(&mut text);
            json!({"content": [{"type": "text", "text": text.to_string()}], "structuredContent": v, "isError": false})
        }
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
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
    let id = m.get("id").cloned().filter(|v| !v.is_null())?;
    let method = m.get("method").and_then(Value::as_str).unwrap_or("");
    let params = m.get("params").cloned().unwrap_or(json!({}));
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
            running.push(std::thread::spawn(move || {
                let tool = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let reply = answer(&id, tool_result(host.call(tool, &args)));
                write_line(&out, &reply);
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
    let host = Arc::new(Host {
        folders: args.iter().map(PathBuf::from).collect(),
    });
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
    // Input closed: answer what is still running, then end.
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
        Host {
            folders: vec![dir.to_path_buf()],
        }
    }

    #[test]
    fn exec_runs_in_the_first_folder_and_reports_it() {
        let dir = temp("exec");
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "Get-ChildItem -Name"
        } else {
            "ls"
        };
        let out = host.exec(&json!({"command": command})).unwrap();
        assert_eq!(out["exit_code"], json!(0));
        assert!(out["stdout"].as_str().unwrap().contains("marker.txt"));
        assert_eq!(out["cwd"], json!(shown(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tool_list_is_neutral_and_says_what_changes_nothing() {
        let list = tools();
        let tools = list.as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["exec", "read", "write", "stat", "list", "search"]);
        for t in tools {
            let name = t["name"].as_str().unwrap();
            let read_only = t["annotations"]["readOnlyHint"].as_bool().unwrap();
            assert_eq!(read_only, !matches!(name, "exec" | "write"), "{name}");
        }
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
