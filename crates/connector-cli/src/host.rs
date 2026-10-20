//! `agentrouter mcp-serve host <folder>…`: the SSH-like surface of a link (docs/LINKER-MCP.md §3.7, owner 2026-10-20
//! 「就和Agent使用SSH同样的感觉会更好吧」). A plain stdio MCP server with three tools — `exec` (a command in the real
//! shell), `read` and `write` (a file by its real path, in chunks for transfers) — that the console starts with every
//! link like any other local server: inside the sandbox in folder mode (the OS refuses writes outside the folders),
//! with the person's own rights in 完全访问. It decides nothing itself; the boundary is the sandbox's.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use agentrouter_device::exec;
use agentrouter_device::util::{b64, b64_decode};

const PROTOCOL: &str = "2025-06-18";
/// One read or write carries at most this much (a transfer chunk; base64 stays under the 8 MiB link frame).
pub const CHUNK_MAX: u64 = 4 << 20;
const TIMEOUT_DEFAULT_S: u64 = 120;

fn tools() -> Value {
    let tool = |name: &str, description: &str, schema: Value, read_only: bool| {
        json!({
            "name": name, "description": description, "inputSchema": schema,
            "annotations": {"readOnlyHint": read_only, "destructiveHint": !read_only, "openWorldHint": name == "exec"},
        })
    };
    json!([
        tool(
            "exec",
            "Run a command in this computer's shell (/bin/sh -lc on Linux/macOS, PowerShell on Windows), like over ssh. cwd defaults to the link's first folder; paths are real.",
            json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": exec::TIMEOUT_MAX_MS / 1000},
            }, "required": ["command"]}),
            false,
        ),
        tool(
            "read",
            "Read a file by its real path (relative = inside the first folder): utf8 text or base64 bytes, a range with offset/length (at most 4 MiB), sha256 of the whole file on request.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0},
                "length": {"type": "integer", "minimum": 0, "maximum": CHUNK_MAX},
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
                "sha256": {"type": "boolean"},
            }, "required": ["path"]}),
            true,
        ),
        tool(
            "write",
            "Write a file by its real path (parents are created). Without offset: create or replace. With offset: write there (truncate: cut the file to offset first). data is utf8 text or base64 bytes (at most 4 MiB).",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "data": {"type": "string"},
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
                "offset": {"type": "integer", "minimum": 0},
                "truncate": {"type": "boolean"},
            }, "required": ["path", "data"]}),
            false,
        ),
    ])
}

/// What the server knows: the linked folders (the first is home).
struct Host {
    folders: Vec<PathBuf>,
}

fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

impl Host {
    fn home(&self) -> PathBuf {
        self.folders
            .first()
            .cloned()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// A real absolute path: relative ones are inside the first folder; an existing path is canonical.
    fn resolve(&self, raw: &str) -> Result<PathBuf, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("path 不能是空的".into());
        }
        let p = PathBuf::from(raw);
        let p = if p.is_absolute() {
            p
        } else {
            self.home().join(p)
        };
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
            other => Err(format!("没有这个工具：{other}")),
        }
    }

    fn exec(&self, args: &Value) -> Result<Value, String> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        if command.trim().is_empty() {
            return Err("command 不能是空的".into());
        }
        let cwd = match args.get("cwd").and_then(Value::as_str) {
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

    fn read(&self, args: &Value) -> Result<Value, String> {
        let path = self.resolve(args.get("path").and_then(Value::as_str).unwrap_or(""))?;
        let mut file =
            std::fs::File::open(&path).map_err(|e| format!("读不了 {}：{e}", shown(&path)))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(size);
        let length = args
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(size - offset)
            .min(size - offset)
            .min(CHUNK_MAX);
        let base64 = args.get("encoding").and_then(Value::as_str) == Some("base64");
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0u8; length as usize];
        file.read_exact(&mut bytes)
            .map_err(|e| format!("读不了 {}：{e}", shown(&path)))?;
        let data = if base64 {
            b64(&bytes)
        } else {
            match String::from_utf8(bytes) {
                Ok(text) => text,
                // A range may end inside a character: keep the whole characters.
                Err(e) if e.utf8_error().valid_up_to() + 4 > e.as_bytes().len() => {
                    let valid = e.utf8_error().valid_up_to();
                    let bytes = e.into_bytes();
                    String::from_utf8_lossy(&bytes[..valid]).into_owned()
                }
                Err(_) => {
                    return Err(format!(
                        "{} 不是 UTF-8 文本：用 encoding base64 读，或用传文件的工具",
                        shown(&path)
                    ));
                }
            }
        };
        let length = if base64 { length } else { data.len() as u64 };
        let mut answer = json!({
            "path": shown(&path), "size": size, "offset": offset, "length": length,
            "eof": offset + length >= size, "encoding": if base64 { "base64" } else { "utf8" }, "data": data,
        });
        if args.get("sha256").and_then(Value::as_bool) == Some(true) {
            answer["sha256"] = json!(sha256_file(&path)?);
        }
        Ok(answer)
    }

    fn write(&self, args: &Value) -> Result<Value, String> {
        let path = self.resolve(args.get("path").and_then(Value::as_str).unwrap_or(""))?;
        let data = args.get("data").and_then(Value::as_str).unwrap_or("");
        let bytes = if args.get("encoding").and_then(Value::as_str) == Some("base64") {
            b64_decode(data).ok_or("data 不是有效的 base64")?
        } else {
            data.as_bytes().to_vec()
        };
        if bytes.len() as u64 > CHUNK_MAX {
            return Err(format!(
                "一次最多写 {} 字节；大文件分块写（offset）",
                CHUNK_MAX
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建不了 {}：{e}", shown(parent)))?;
        }
        let fail = |e: std::io::Error| format!("写不了 {}：{e}", shown(&path));
        match args.get("offset").and_then(Value::as_u64) {
            None => std::fs::write(&path, &bytes).map_err(fail)?,
            Some(offset) => {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(&path)
                    .map_err(fail)?;
                if args.get("truncate").and_then(Value::as_bool) == Some(true) {
                    file.set_len(offset).map_err(fail)?;
                }
                file.seek(SeekFrom::Start(offset)).map_err(fail)?;
                file.write_all(&bytes).map_err(fail)?;
            }
        }
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(json!({"path": shown(&path), "size": size, "written": bytes.len()}))
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
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

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(v) => {
            json!({"content": [{"type": "text", "text": v.to_string()}], "structuredContent": v, "isError": false})
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

    fn temp(name: &str) -> PathBuf {
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

    #[test]
    fn chunked_write_then_read_back_with_the_same_hash() {
        let dir = temp("chunks");
        let host = Host {
            folders: vec![dir.clone()],
        };
        let first = host
            .write(&json!({"path": "out/a.bin", "data": b64(&[1, 2, 3]), "encoding": "base64", "offset": 0, "truncate": true}))
            .unwrap();
        assert_eq!(first["path"], json!(shown(&dir.join("out").join("a.bin"))));
        host.write(
            &json!({"path": "out/a.bin", "data": b64(&[4, 5]), "encoding": "base64", "offset": 3}),
        )
        .unwrap();
        let back = host
            .read(&json!({"path": dir.join("out/a.bin").to_string_lossy(), "encoding": "base64", "sha256": true}))
            .unwrap();
        assert_eq!(back["size"], json!(5));
        assert_eq!(
            b64_decode(back["data"].as_str().unwrap()).unwrap(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(back["eof"], json!(true));
        assert_eq!(back["sha256"].as_str().unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exec_runs_in_the_first_folder_and_reports_it() {
        let dir = temp("exec");
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let host = Host {
            folders: vec![dir.clone()],
        };
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
    fn the_tool_list_is_three_neutral_tools() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["exec", "read", "write"]);
    }
}
