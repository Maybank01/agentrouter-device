//! Pseudo-terminals for interactive programs (DSH's persistent terminal, REPLs, prompts, TUIs): `pty_open` starts
//! the shell (or a command) on a PTY — openpty on Unix, ConPTY on Windows, from wezterm's `portable-pty` — then
//! `pty_write` sends keys, `pty_read` returns the terminal output since an offset (escape sequences included, as a
//! terminal emulator would get them), `pty_resize` and `pty_close`. Like background jobs they live as long as the link
//! and end with their whole process tree.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

use agentrouter_device::command_text::{self, TextFile};
use agentrouter_device::exec::Tree;
use agentrouter_device::util::b64_decode;

use super::jobs::Kept;
use super::{Call, Fail, Host, arg_str, shell_name, shown, tool};

const OPEN_MAX: usize = 16;
const READ_DEFAULT: u64 = 256 << 10;
const READ_MAX: u64 = 4 << 20;
const WAIT_MAX_MS: u64 = 30_000;
const WRITE_MAX: usize = 1 << 20;

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "pty_open",
            "Open a pseudo-terminal running this computer's shell interactively, or `command` in it (for programs that need a terminal: prompts, REPLs, TUIs). Then pty_write keys, pty_read output, pty_close. Ends with the link.",
            json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "cols": {"type": "integer", "minimum": 10, "maximum": 1000},
                "rows": {"type": "integer", "minimum": 5, "maximum": 500},
            }}),
            false,
        ),
        tool(
            "pty_write",
            "Send input to a pseudo-terminal: text as typed (\"\\r\" is Enter, \"\\u0003\" is Ctrl+C), or base64 bytes.",
            json!({"type": "object", "properties": {
                "pty": {"type": "string"},
                "data": {"type": "string"},
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
            }, "required": ["pty", "data"]}),
            false,
        ),
        tool(
            "pty_read",
            "A pseudo-terminal's output since a byte offset (default 0; escape sequences included) and whether its program still runs. wait_ms (≤ 30000) waits for new output first. Pass back next_offset.",
            json!({"type": "object", "properties": {
                "pty": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": READ_MAX},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": WAIT_MAX_MS},
            }, "required": ["pty"]}),
            true,
        ),
        tool(
            "pty_resize",
            "Resize a pseudo-terminal.",
            json!({"type": "object", "properties": {
                "pty": {"type": "string"},
                "cols": {"type": "integer", "minimum": 10, "maximum": 1000},
                "rows": {"type": "integer", "minimum": 5, "maximum": 500},
            }, "required": ["pty", "cols", "rows"]}),
            false,
        ),
        tool(
            "pty_close",
            "Close a pseudo-terminal: its program and everything it started end.",
            json!({"type": "object", "properties": {"pty": {"type": "string"}}, "required": ["pty"]}),
            false,
        ),
    ]
}

#[derive(Default)]
struct State {
    out: Kept,
    exit: Option<u32>,
}

struct Pty {
    id: String,
    pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    tree: Mutex<Option<Tree>>,
    state: Mutex<State>,
    changed: Condvar,
    /// The command's text file (`command_text`): the shell deletes it once read; this removes it if it never was.
    _text: Option<TextFile>,
}

impl Pty {
    fn close(&self) {
        let _ = self.child.lock().unwrap().kill();
        if let Some(mut tree) = self.tree.lock().unwrap().take() {
            tree.kill();
        }
    }
}

#[derive(Default)]
pub(super) struct Ptys {
    all: Mutex<Vec<Arc<Pty>>>,
    next: AtomicU64,
}

impl Ptys {
    fn get(&self, id: &str) -> Result<Arc<Pty>, String> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| format!("没有这个终端：{id}"))
    }

    pub(super) fn end_all(&self) {
        for pty in self.all.lock().unwrap().drain(..) {
            pty.close();
        }
    }
}

fn size(args: &Value) -> PtySize {
    let get = |k: &str, d: u64, lo: u64, hi: u64| {
        args.get(k)
            .and_then(Value::as_u64)
            .unwrap_or(d)
            .clamp(lo, hi) as u16
    };
    PtySize {
        rows: get("rows", 30, 5, 500),
        cols: get("cols", 120, 10, 1000),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The shell on the terminal: interactive, or running `command`, whose text reaches the shell in an owner-only file,
/// never as an argument (`command_text`, #99; the terminal is the program's stdin).
fn command_line(command: Option<&str>) -> Result<(CommandBuilder, Option<TextFile>), String> {
    let start = command
        .map(|c| command_text::in_terminal(c, &command_text::run_dir()))
        .transpose()?;
    #[cfg(windows)]
    {
        let (cmd, file) = match start {
            Some(start) => {
                let mut cmd = CommandBuilder::new(&start.program);
                cmd.args(&start.args);
                if start.windows_powershell {
                    cmd.env(
                        "PSModulePath",
                        agentrouter_device::util::child_ps_module_path(),
                    );
                }
                (cmd, start.file)
            }
            None => {
                let shell = agentrouter_device::shell::default_shell();
                let mut cmd = CommandBuilder::new(&shell.program);
                cmd.args(["-NoLogo", "-NoProfile"]);
                if !shell.is_pwsh() {
                    cmd.env(
                        "PSModulePath",
                        agentrouter_device::util::child_ps_module_path(),
                    );
                }
                (cmd, None)
            }
        };
        Ok((cmd, file))
    }
    #[cfg(not(windows))]
    {
        let (mut cmd, file) = match start {
            Some(start) => {
                let mut c = CommandBuilder::new(&start.program);
                c.args(&start.args);
                (c, start.file)
            }
            // The person's login shell, as an ssh session gets.
            None => (CommandBuilder::new_default_prog(), None),
        };
        // The person's terminal environment (#95); the terminal's own TERM over it.
        if let Some(env) = agentrouter_device::shell_env::for_command() {
            cmd.env_clear();
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
        cmd.env("TERM", "xterm-256color");
        Ok((cmd, file))
    }
}

impl Host {
    pub(super) fn pty_open(&self, args: &Value) -> Result<Value, Fail> {
        let cwd = self.work_dir("pty_open", args)?;
        if self.ptys.all.lock().unwrap().len() >= OPEN_MAX {
            return Err(format!("终端最多同时开 {OPEN_MAX} 个；先 pty_close 不用的").into());
        }
        let command = arg_str(args, "command").filter(|c| !c.trim().is_empty());
        let pair = native_pty_system()
            .openpty(size(args))
            .map_err(|e| format!("开不了终端：{e}"))?;
        let (mut cmd, text) = command_line(command)?;
        cmd.cwd(&cwd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("终端里的程序起不来：{e}"))?;
        // Our copy of the terminal's other end closes, so its output ends when the program's side does.
        drop(pair.slave);
        let pid = child.process_id();
        let tree = pid.and_then(|p| Tree::adopt(p).ok());
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("读不了终端：{e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("写不了终端：{e}"))?;
        let n = self.ptys.next.fetch_add(1, Ordering::Relaxed) + 1;
        let pty = Arc::new(Pty {
            id: format!("pty-{n}"),
            pid,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            tree: Mutex::new(tree),
            state: Mutex::default(),
            changed: Condvar::new(),
            _text: text,
        });
        {
            let pty = pty.clone();
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = vec![0u8; 64 << 10];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // A cursor position query (ConPTY sends one at start and waits for the answer, as
                            // TUIs do): answered like a terminal would, from the top-left corner.
                            let chunk = &buf[..n];
                            let queries = chunk.windows(4).filter(|w| *w == b"\x1b[6n").count();
                            if queries > 0 {
                                let mut writer = pty.writer.lock().unwrap();
                                for _ in 0..queries {
                                    let _ = writer.write_all(b"\x1b[1;1R");
                                }
                                let _ = writer.flush();
                            }
                            pty.state.lock().unwrap().out.push(chunk);
                            pty.changed.notify_all();
                        }
                    }
                }
            });
        }
        {
            let pty = pty.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let status = pty.child.lock().unwrap().try_wait();
                    match status {
                        Ok(None) => continue,
                        Ok(Some(status)) => {
                            pty.state.lock().unwrap().exit = Some(status.exit_code());
                        }
                        Err(_) => pty.state.lock().unwrap().exit = Some(u32::MAX),
                    }
                    pty.changed.notify_all();
                    break;
                }
            });
        }
        self.ptys.all.lock().unwrap().push(pty.clone());
        Ok(json!({"pty": pty.id, "pid": pid, "cwd": shown(&cwd), "shell": shell_name()}))
    }

    pub(super) fn pty_write(&self, args: &Value) -> Result<Value, String> {
        let pty = self.ptys.get(arg_str(args, "pty").unwrap_or(""))?;
        let data = arg_str(args, "data").unwrap_or("");
        let bytes = if arg_str(args, "encoding") == Some("base64") {
            b64_decode(data).ok_or("data 不是有效的 base64")?
        } else {
            data.as_bytes().to_vec()
        };
        if bytes.len() > WRITE_MAX {
            return Err(format!("一次最多写 {WRITE_MAX} 字节"));
        }
        let mut writer = pty.writer.lock().unwrap();
        writer
            .write_all(&bytes)
            .and_then(|_| writer.flush())
            .map_err(|e| format!("写不了终端：{e}"))?;
        Ok(json!({"pty": pty.id, "written": bytes.len()}))
    }

    pub(super) fn pty_read(&self, args: &Value, call: &Call) -> Result<Value, String> {
        let pty = self.ptys.get(arg_str(args, "pty").unwrap_or(""))?;
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let max = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(READ_DEFAULT)
            .clamp(1, READ_MAX) as usize;
        let until = Instant::now()
            + Duration::from_millis(
                args.get("wait_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(WAIT_MAX_MS),
            );
        let mut state = pty.state.lock().unwrap();
        while state.exit.is_none() && state.out.end() <= offset && !call.cancelled() {
            let now = Instant::now();
            if now >= until {
                break;
            }
            state = pty
                .changed
                .wait_timeout(state, (until - now).min(Duration::from_millis(100)))
                .unwrap()
                .0;
        }
        let mut out = state.out.read(offset, max, state.exit.is_some());
        out["pty"] = json!(pty.id);
        out["running"] = json!(state.exit.is_none());
        out["exit_code"] = json!(state.exit);
        Ok(out)
    }

    pub(super) fn pty_resize(&self, args: &Value) -> Result<Value, String> {
        let pty = self.ptys.get(arg_str(args, "pty").unwrap_or(""))?;
        let size = size(args);
        pty.master
            .lock()
            .unwrap()
            .resize(size)
            .map_err(|e| format!("调不了终端大小：{e}"))?;
        Ok(json!({"pty": pty.id, "cols": size.cols, "rows": size.rows}))
    }

    pub(super) fn pty_close(&self, args: &Value) -> Result<Value, String> {
        let pty = self.ptys.get(arg_str(args, "pty").unwrap_or(""))?;
        pty.close();
        self.ptys
            .all
            .lock()
            .unwrap()
            .retain(|p| !Arc::ptr_eq(p, &pty));
        Ok(json!({"pty": pty.id, "pid": pty.pid, "closed": true}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};
    use super::*;

    /// The text without terminal control sequences (ConPTY repaints with cursor moves between words).
    fn plain(raw: &str) -> String {
        let mut out = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.next() {
                    Some('[') => {
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() || c == '~' {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        for c in chars.by_ref() {
                            if c == '\x07' || c == '\x1b' {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            } else if c != '\r' {
                out.push(c);
            }
        }
        out
    }

    fn read_until(host: &Host, id: &str, needle: &str) -> String {
        let (mut raw, mut text) = (String::new(), String::new());
        let mut offset = 0;
        let until = Instant::now() + Duration::from_secs(20);
        while !text.contains(needle) && Instant::now() < until {
            let r = host
                .pty_read(
                    &json!({"pty": id, "offset": offset, "wait_ms": 1000}),
                    &Call::detached(),
                )
                .unwrap();
            raw.push_str(r["data"].as_str().unwrap());
            text = plain(&raw);
            offset = r["next_offset"].as_u64().unwrap();
        }
        text
    }

    /// A terminal command past CreateProcess's 32 767 characters (and with quotes, `$`, newlines, non-ASCII) runs
    /// from its owner-only file, never an argument; the file is gone once the shell has read it (#99).
    #[test]
    fn a_long_terminal_command_runs_from_its_file_and_the_file_goes() {
        let dir = temp("pty-long");
        let host = host_in(&dir);
        let pad = "x".repeat(40_000);
        let command = if cfg!(windows) {
            format!(
                "$pad = '{pad}'\n$q = 'it''s \"q\" $notavar'\nWrite-Output (\"len=\" + $pad.Length + \" u=\" + '中文é'.Length + \" q=\" + $q)\nStart-Sleep -Seconds 30"
            )
        } else {
            format!(
                "pad='{pad}'\nq='it'\\''s \"q\" $notavar'\necho \"len=${{#pad}} q=$q\"\nsleep 30"
            )
        };
        let opened = host
            .pty_open(&json!({"command": command, "cols": 200}))
            .unwrap();
        let id = opened["pty"].as_str().unwrap().to_string();
        let text = read_until(&host, &id, "len=40000");
        assert!(text.contains("len=40000"), "{text}");
        assert!(text.contains("q\" $notavar"), "{text}");
        if cfg!(windows) {
            assert!(text.contains("u=3"), "{text}");
        }
        let left: Vec<_> = std::fs::read_dir(agentrouter_device::command_text::run_dir())
            .map(|d| {
                d.flatten()
                    .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t.contains(&pad)))
                    .collect()
            })
            .unwrap_or_default();
        assert!(left.is_empty(), "the command file is still there: {left:?}");
        host.pty_close(&json!({"pty": id})).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interactive_program_reads_what_is_typed() {
        let dir = temp("pty");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "$name = Read-Host 'name?'; Write-Output \"hello $name\"; Start-Sleep -Seconds 30"
        } else {
            "printf 'name? '; read name; echo \"hello $name\"; [ -t 0 ] && echo tty; sleep 30"
        };
        let opened = host
            .pty_open(&json!({"command": command, "cols": 80}))
            .unwrap();
        let id = opened["pty"].as_str().unwrap().to_string();
        assert!(read_until(&host, &id, "name?").contains("name?"));
        host.pty_write(&json!({"pty": id, "data": "world\r"}))
            .unwrap();
        let text = read_until(&host, &id, "hello world");
        assert!(text.contains("hello world"), "{text}");
        if cfg!(unix) {
            assert!(read_until(&host, &id, "tty").contains("tty"));
        }
        host.pty_resize(&json!({"pty": id, "cols": 100, "rows": 40}))
            .unwrap();
        host.pty_close(&json!({"pty": id})).unwrap();
        assert!(
            host.pty_read(&json!({"pty": id}), &Call::detached())
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
