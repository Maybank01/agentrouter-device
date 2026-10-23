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

    #[cfg(windows)]
    /// Everything read from `offset` until `needle` shows in the plain text (or 20 s), raw, and the offset after it.
    fn read_raw_until(host: &Host, id: &str, offset: &mut u64, needle: &str) -> String {
        read_raw_while(host, id, offset, |text| !text.contains(needle))
    }

    #[cfg(windows)]
    /// Everything read from `offset` while `more(plain text so far)` (at most 20 s), raw; `offset` moves past it.
    fn read_raw_while(
        host: &Host,
        id: &str,
        offset: &mut u64,
        more: impl Fn(&str) -> bool,
    ) -> String {
        let mut raw = String::new();
        let until = Instant::now() + Duration::from_secs(20);
        while more(&plain(&raw)) && Instant::now() < until {
            let r = host
                .pty_read(
                    &json!({"pty": id, "offset": *offset, "wait_ms": 1000}),
                    &Call::detached(),
                )
                .unwrap();
            raw.push_str(r["data"].as_str().unwrap());
            *offset = r["next_offset"].as_u64().unwrap();
            if r["running"] == json!(false) {
                break;
            }
        }
        raw
    }

    #[cfg(windows)]
    /// Types `text` and Enter, then reads until `needle` (read_raw_until).
    fn send(host: &Host, id: &str, offset: &mut u64, text: &str, needle: &str) -> String {
        host.pty_write(&json!({"pty": id, "data": format!("{text}\r")}))
            .unwrap();
        read_raw_until(host, id, offset, needle)
    }

    /// The terminal a cloud Agent gets on a Windows computer (Cloud #3008, dsh-host.mjs `TERMINAL_SHELLS.pwsh`): this
    /// computer's interactive PowerShell on the ConPTY (no `command`), given DSH's terminal settings as the first typed
    /// line, then DSH's pwsh dialect (`ENCODING_PREAMBLE` + `PWSH_PROMPT_SETUP` of @deepseek-ai/dsh-terminal-bash, same
    /// text): settings in the environment, the `dsh> ` prompt, exit status, a question answered, Ctrl+C, cwd, exit code.
    #[cfg(windows)]
    #[test]
    fn the_cloud_terminal_is_this_computers_powershell_in_dshs_dialect() {
        let dir = temp("pty-dsh-pwsh");
        let host = host_in(&dir);
        let opened = host
            .pty_open(&json!({"cwd": dir.to_string_lossy(), "cols": 160, "rows": 40}))
            .unwrap();
        let id = opened["pty"].as_str().unwrap().to_string();
        assert!(
            matches!(opened["shell"].as_str(), Some("pwsh" | "powershell")),
            "{opened}"
        );
        let mut offset = 0;
        // DSH's settings, the marker joined only when PowerShell runs the line (the echo never contains it).
        let out = send(
            &host,
            &id,
            &mut offset,
            "$env:TERM = 'dumb'; $env:NO_COLOR = '1'; $env:DSH_SESSION_ID = 'it''s-k1'; Write-Output ('__AR_TERMINAL_RE' + 'ADY_t__')",
            "__AR_TERMINAL_READY_t__",
        );
        assert!(plain(&out).contains("__AR_TERMINAL_READY_t__"), "{out:?}");
        // DSH's pwsh dialect startup: UTF-8 output, its prompt function (OSC 133;D;<exit> then `dsh> `). Needles are
        // joined at run time, so the shell's echo of a typed line never matches them.
        send(
            &host,
            &id,
            &mut offset,
            "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [System.Text.UTF8Encoding]::new($false); function prompt { [Console]::Write([char]27 + ']133;D;' + [int]$LASTEXITCODE + [char]7); 'dsh> ' }",
            "function prompt",
        );
        let mut out = send(
            &host,
            &id,
            &mut offset,
            "Write-Output ('prompt' + '-ok')",
            "prompt-ok",
        );
        if !plain(&out)
            .split("prompt-ok")
            .nth(1)
            .is_some_and(|t| t.contains("dsh> "))
        {
            out.push_str(&read_raw_until(&host, &id, &mut offset, "dsh> "));
        }
        assert!(
            plain(&out)
                .split("prompt-ok")
                .nth(1)
                .is_some_and(|t| t.contains("dsh> ")),
            "{out:?}"
        );
        let out = send(
            &host,
            &id,
            &mut offset,
            "Write-Output \"$env:TERM/$env:DSH_SESSION_ID/中文\"",
            "dumb/it's-k1/中文",
        );
        assert!(plain(&out).contains("dumb/it's-k1/中文"), "{out:?}");
        // Exit status reaches the prompt marker.
        let out = send(
            &host,
            &id,
            &mut offset,
            "cmd /c exit 3; Write-Output \"code=$LASTEXITCODE\"",
            "code=3",
        );
        assert!(plain(&out).contains("code=3"), "{out:?}");
        let out = format!("{out}{}", read_raw_until(&host, &id, &mut offset, "dsh> "));
        // ConPTY passes DSH's prompt marker through (windows-latest, 2026-10-23), so DSH's readiness is its marker.
        assert!(out.contains("\x1b]133;D;3\x07"), "{out:?}");
        // A program's question, answered by what is typed.
        let out = send(
            &host,
            &id,
            &mut offset,
            "$n = Read-Host ('Display ' + 'name')",
            "Display name:",
        );
        assert!(plain(&out).contains("Display name:"), "{out:?}");
        let out = send(&host, &id, &mut offset, "Star Forge X", "dsh> ");
        assert!(plain(&out).contains("dsh> "), "{out:?}");
        let out = send(
            &host,
            &id,
            &mut offset,
            "Write-Output \"hello $n\"",
            "hello Star Forge X",
        );
        assert!(plain(&out).contains("hello Star Forge X"), "{out:?}");
        // Ctrl+C ends what runs in the foreground and its prompt comes back (the console drops what was typed before
        // that, as a Unix terminal does on SIGINT: DSH sends the next input after the prompt); the shell stays.
        host.pty_write(&json!({"pty": id, "data": "Start-Sleep -Seconds 60\r"}))
            .unwrap();
        let echoed = read_raw_until(&host, &id, &mut offset, "Seconds 60");
        std::thread::sleep(Duration::from_millis(1000));
        let started = Instant::now();
        host.pty_write(&json!({"pty": id, "data": "\u{3}"}))
            .unwrap();
        let back = |text: &str| {
            text.split("Seconds 60")
                .nth(1)
                .is_some_and(|t| t.contains("dsh> "))
        };
        let out = echoed.clone()
            + &read_raw_while(&host, &id, &mut offset, |t| !back(&(plain(&echoed) + t)));
        assert!(back(&plain(&out)), "{out:?}");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "Ctrl+C did not end Start-Sleep"
        );
        let out = send(
            &host,
            &id,
            &mut offset,
            "Write-Output ('after-' + 'ctrl-c')",
            "after-ctrl-c",
        );
        assert!(plain(&out).contains("after-ctrl-c"), "{out:?}");
        // The terminal starts in the folder it was opened in.
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let out = send(
            &host,
            &id,
            &mut offset,
            "Write-Output (Get-Location).Path",
            &name,
        );
        assert!(plain(&out).contains(&name), "{out:?}");
        // The shell's exit code is the terminal's.
        host.pty_write(&json!({"pty": id, "data": "exit 7\r"}))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(20);
        let mut last = json!(null);
        while Instant::now() < until {
            last = host
                .pty_read(
                    &json!({"pty": id, "offset": offset, "wait_ms": 1000}),
                    &Call::detached(),
                )
                .unwrap();
            offset = last["next_offset"].as_u64().unwrap_or(offset);
            if last["running"] == json!(false) {
                break;
            }
        }
        assert_eq!(last["running"], json!(false), "{last}");
        assert_eq!(last["exit_code"], json!(7), "{last}");
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
