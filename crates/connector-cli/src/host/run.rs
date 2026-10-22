//! Commands in the real shell. `exec` runs one to its end; while it runs, its output streams to the caller as MCP
//! progress notifications (when the call carries a `progressToken`), each `message` the JSON text of
//! `{"stream": "stdout"|"stderr", "data": "<utf-8 text>"}`. The answer still carries the (capped) output, the exit
//! status, and whether a timeout or a cancel ended it.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use agentrouter_device::exec::{self, Event, Stream};

use super::{Call, Fail, Host, arg_str, shell_name, shown, tool};

const TIMEOUT_DEFAULT_S: u64 = 120;
/// Output pushed as one progress notification once this much is waiting…
pub(super) const PUSH_BYTES: usize = 32 << 10;
/// …or this long after the last push.
pub(super) const PUSH_EVERY: Duration = Duration::from_millis(100);
/// Streamed per stream at most (a runaway log must not flood the link); the rest is counted, not sent.
const PUSH_MAX: usize = 16 << 20;

pub(super) fn tools() -> Vec<Value> {
    vec![tool(
        "exec",
        "Run a command in this computer's shell (/bin/sh -c on Linux/macOS, PowerShell on Windows) with the person's own terminal environment (PATH and variables from their shell start-up files, resolved once), like over ssh. cwd defaults to the link's first folder; paths are real. With a progressToken, stdout/stderr stream as progress notifications (message = {\"stream\",\"data\"} JSON) while it runs; cancelling the call ends the command's whole process tree.",
        json!({"type": "object", "properties": {
            "command": {"type": "string"},
            "cwd": {"type": "string"},
            "timeout_s": {"type": "integer", "minimum": 1, "maximum": exec::TIMEOUT_MAX_MS / 1000},
        }, "required": ["command"]}),
        false,
    )]
}

/// The longest prefix of `bytes` that is whole UTF-8 characters, and how many bytes it takes. Invalid bytes in the
/// middle become U+FFFD; an incomplete character at the end waits for its rest (unless `last`).
pub(super) fn utf8_head(bytes: &[u8], last: bool) -> (String, usize) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), bytes.len()),
        Err(e) if e.error_len().is_none() && !last => {
            let n = e.valid_up_to();
            (String::from_utf8_lossy(&bytes[..n]).into_owned(), n)
        }
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), bytes.len()),
    }
}

/// Coalesces output chunks into progress notifications.
pub(super) struct Streamer<'a> {
    call: &'a Call,
    pending: [Vec<u8>; 2],
    pushed: [usize; 2],
    last: Instant,
}

impl<'a> Streamer<'a> {
    pub(super) fn new(call: &'a Call) -> Streamer<'a> {
        Streamer {
            call,
            pending: [Vec::new(), Vec::new()],
            pushed: [0, 0],
            last: Instant::now(),
        }
    }

    pub(super) fn event(&mut self, event: Event<'_>) {
        if !self.call.streams() {
            return;
        }
        match event {
            Event::Output(which, bytes) => {
                let i = which as usize;
                let room = PUSH_MAX.saturating_sub(self.pushed[i] + self.pending[i].len());
                self.pending[i].extend_from_slice(&bytes[..bytes.len().min(room)]);
                if self.pending[i].len() >= PUSH_BYTES {
                    self.flush(false);
                }
            }
            Event::Tick => {
                if self.last.elapsed() >= PUSH_EVERY {
                    self.flush(false);
                }
            }
        }
    }

    /// Push what is waiting; `last` also pushes a trailing partial character.
    pub(super) fn flush(&mut self, last: bool) {
        for (i, name) in [
            (Stream::Stdout as usize, "stdout"),
            (Stream::Stderr as usize, "stderr"),
        ] {
            if self.pending[i].is_empty() {
                continue;
            }
            let (text, used) = utf8_head(&self.pending[i], last);
            if used == 0 {
                continue;
            }
            self.pending[i].drain(..used);
            self.pushed[i] += used;
            self.call.progress(&json!({"stream": name, "data": text}));
        }
        self.last = Instant::now();
    }
}

impl Host {
    pub(super) fn exec(&self, args: &Value, call: &Call) -> Result<Value, Fail> {
        let command = arg_str(args, "command").unwrap_or("");
        if command.trim().is_empty() {
            return Err("command 不能是空的".into());
        }
        let cwd = self.work_dir("exec", args)?;
        let timeout = args
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(TIMEOUT_DEFAULT_S)
            .clamp(1, exec::TIMEOUT_MAX_MS / 1000);
        let mut streamer = Streamer::new(call);
        let out = exec::run_streaming(
            command,
            &cwd,
            Duration::from_secs(timeout),
            &|| call.cancelled(),
            &mut |e| streamer.event(e),
        )?;
        streamer.flush(true);
        Ok(json!({
            "exit_code": out.exit_code,
            "signal": out.signal,
            "timed_out": out.timed_out,
            "cancelled": out.cancelled,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "cwd": shown(&cwd),
            "shell": shell_name(),
            "ms": out.duration_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::super::tests::{host_in, temp};
    use super::*;

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
        let out = host
            .exec(&json!({"command": command}), &Call::detached())
            .unwrap();
        assert_eq!(out["exit_code"], json!(0));
        assert!(out["stdout"].as_str().unwrap().contains("marker.txt"));
        assert_eq!(out["cwd"], json!(shown(&dir)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cloud #2246: a command that writes into a missing folder answers the shell's own error, first, with its exit
    /// status — nothing a start-up file prints comes before it (commands are not login shells; `command_text`).
    #[cfg(unix)]
    #[test]
    fn a_write_into_a_missing_folder_answers_the_shell_s_own_error() {
        let dir = temp("missing-folder");
        let host = host_in(&dir);
        let out = host
            .exec(
                &json!({"command": "echo x > work/wrk_missing/pc.txt"}),
                &Call::detached(),
            )
            .unwrap();
        assert_ne!(out["exit_code"], json!(0), "{out}");
        let stderr = out["stderr"].as_str().unwrap();
        let first = stderr.lines().next().unwrap_or("");
        assert!(first.contains("work/wrk_missing/pc.txt"), "{stderr}");
        assert!(
            ["nonexistent", "No such file"]
                .iter()
                .any(|w| first.contains(w)),
            "{stderr}"
        );
        assert!(!dir.join("work").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_cwd_is_a_coded_failure_naming_the_path() {
        let dir = temp("missing-cwd");
        let host = host_in(&dir);
        let fail = host
            .exec(
                &json!({"command": "true", "cwd": "nope/deeper"}),
                &Call::detached(),
            )
            .unwrap_err();
        assert_eq!(fail.code, Some("NOT_FOUND"), "{fail:?}");
        assert_eq!(fail.data["operation"], "exec");
        assert_eq!(fail.data["path"], json!(shown(&dir.join("nope/deeper"))));
        assert_eq!(fail.data["retryable"], false);
        assert!(fail.message.contains("No such file"), "{fail:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_streams_as_progress_before_the_answer() {
        let dir = temp("stream");
        let host = host_in(&dir);
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let call = Call {
            token: Some(json!("t1")),
            cancelled: Arc::default(),
            seq: AtomicU64::new(0),
            notify: {
                let seen = seen.clone();
                Arc::new(move |v: &Value| seen.lock().unwrap().push(v.clone()))
            },
        };
        let command = if cfg!(windows) {
            "Write-Output one; [Console]::Error.WriteLine('two'); Start-Sleep -Milliseconds 300; Write-Output three"
        } else {
            "echo one; echo two >&2; sleep 0.3; echo three"
        };
        let out = host.exec(&json!({"command": command}), &call).unwrap();
        assert_eq!(out["exit_code"], 0);
        let seen = seen.lock().unwrap();
        assert!(seen.len() >= 2, "{seen:?}");
        let mut text = String::new();
        let mut stderr = String::new();
        for (i, n) in seen.iter().enumerate() {
            assert_eq!(n["method"], "notifications/progress");
            assert_eq!(n["params"]["progressToken"], "t1");
            assert_eq!(n["params"]["progress"], (i + 1) as u64);
            let m: Value = serde_json::from_str(n["params"]["message"].as_str().unwrap()).unwrap();
            match m["stream"].as_str().unwrap() {
                "stdout" => text.push_str(m["data"].as_str().unwrap()),
                _ => stderr.push_str(m["data"].as_str().unwrap()),
            }
        }
        assert!(text.contains("one") && text.contains("three"), "{text}");
        assert!(stderr.contains("two"), "{stderr}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancel_ends_the_command() {
        let dir = temp("cancel");
        let host = host_in(&dir);
        let cancelled = Arc::new(AtomicBool::new(false));
        let call = Call {
            cancelled: cancelled.clone(),
            ..Call::detached()
        };
        let flag = cancelled.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.store(true, Ordering::Relaxed);
        });
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 20"
        } else {
            "sleep 20"
        };
        let started = Instant::now();
        let out = host.exec(&json!({"command": command}), &call).unwrap();
        assert_eq!(out["cancelled"], true);
        assert_eq!(out["exit_code"], Value::Null);
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf8_heads_wait_for_split_characters() {
        let bytes = "日本".as_bytes();
        assert_eq!(utf8_head(&bytes[..4], false), ("日".to_string(), 3));
        assert_eq!(utf8_head(&bytes[..4], true).1, 4);
        assert_eq!(utf8_head(b"a\xffb", false), ("a\u{fffd}b".to_string(), 3));
    }
}
