//! Background jobs: commands that keep running after the call that started them (dev servers, watchers, long
//! builds; DSH's `run_in_background` and its job tools). `job_start` starts one in the real shell, `job_read` returns
//! its output since a byte offset (optionally waiting for more), `job_stop` ends its whole process tree, `jobs`
//! lists them. They live as long as the link: when the host server ends (input closed, a stop signal) every job's
//! tree is ended, and on Windows each job is a kill-on-close job object, so even a killed server takes them along.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use agentrouter_device::exec::{self, Spawned};

use super::run::utf8_head;
use super::{Call, Host, arg_str, shell_name, shown, tool};

/// Output kept per stream of a job; older bytes are dropped (a read from before them says so).
const KEEP: usize = 8 << 20;
const READ_DEFAULT: u64 = 1 << 20;
const READ_MAX: u64 = 4 << 20;
const WAIT_MAX_MS: u64 = 30_000;
const RUNNING_MAX: usize = 32;
/// Jobs remembered (finished ones are forgotten oldest first).
const KEPT_MAX: usize = 64;

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "job_start",
            "Start a command in the background in this computer's shell (dev servers, watchers, long builds); it keeps running after this call. Read its output with job_read, end it with job_stop. Jobs end with the link.",
            json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
            }, "required": ["command"]}),
            false,
        ),
        tool(
            "job_read",
            "A background job's output since byte offsets (stdout_offset/stderr_offset, default 0) and whether it still runs. wait_ms (≤ 30000) waits for new output or its end first. Pass back next_offset to read on.",
            json!({"type": "object", "properties": {
                "job": {"type": "string"},
                "stdout_offset": {"type": "integer", "minimum": 0},
                "stderr_offset": {"type": "integer", "minimum": 0},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": READ_MAX},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": WAIT_MAX_MS},
            }, "required": ["job"]}),
            true,
        ),
        tool(
            "job_stop",
            "End a background job's whole process tree (a polite stop, then a kill after grace_ms, default 2000) and forget it.",
            json!({"type": "object", "properties": {
                "job": {"type": "string"},
                "grace_ms": {"type": "integer", "minimum": 0, "maximum": 30_000},
            }, "required": ["job"]}),
            false,
        ),
        tool(
            "jobs",
            "The background jobs of this link: id, command, cwd, pid, running, exit code, output sizes.",
            json!({"type": "object", "properties": {}}),
            true,
        ),
    ]
}

/// One stream's kept output: bytes `[start, start + data.len())` of everything it wrote.
#[derive(Default)]
pub(super) struct Kept {
    start: u64,
    data: Vec<u8>,
}

impl Kept {
    pub(super) fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// From byte `offset` (clamped to what is kept) at most `max` bytes as UTF-8 (`last`: the stream ended, a
    /// partial character at its end is shown too): `{data, offset, next_offset, more, dropped}`.
    pub(super) fn read(&self, offset: u64, max: usize, last: bool) -> Value {
        let from = offset.clamp(self.start, self.end());
        let lo = (from - self.start) as usize;
        let hi = (lo + max).min(self.data.len());
        let (text, used) = utf8_head(&self.data[lo..hi], last && hi == self.data.len());
        json!({
            "data": text, "offset": from, "next_offset": from + used as u64,
            "dropped": offset < self.start, "more": (from + used as u64) < self.end(),
        })
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > KEEP {
            let drop = self.data.len() - KEEP + KEEP / 4;
            self.data.drain(..drop);
            self.start += drop as u64;
        }
    }
}

#[derive(Default)]
struct State {
    out: [Kept; 2],
    /// `(exit_code, signal)` once it ended.
    exit: Option<(Option<i32>, Option<i32>)>,
    ended_ms: Option<u64>,
}

pub(super) struct Job {
    id: String,
    command: String,
    cwd: PathBuf,
    pid: u32,
    started: Instant,
    spawned: Mutex<Option<Spawned>>,
    state: Mutex<State>,
    changed: Condvar,
}

impl Job {
    fn running(&self) -> bool {
        self.state.lock().unwrap().exit.is_none()
    }

    fn record_exit(&self, status: Option<std::process::ExitStatus>) {
        let mut state = self.state.lock().unwrap();
        if state.exit.is_some() {
            return;
        }
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.and_then(|s| s.signal())
        };
        #[cfg(not(unix))]
        let signal = None;
        state.exit = Some((status.and_then(|s| s.code()), signal));
        state.ended_ms = Some(self.started.elapsed().as_millis() as u64);
        self.changed.notify_all();
    }

    /// End the tree: a polite stop, then a kill after `grace`.
    fn stop(&self, grace: Duration) {
        let mut spawned = self.spawned.lock().unwrap();
        let Some(s) = spawned.as_mut() else { return };
        let status = match s.child.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => {
                s.terminate_tree();
                let until = Instant::now() + grace;
                loop {
                    if let Ok(Some(status)) = s.child.try_wait() {
                        break Some(status);
                    }
                    if Instant::now() >= until {
                        s.kill_tree();
                        break s.child.wait().ok();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        // Whatever the shell left behind goes too.
        s.kill_tree();
        drop(spawned.take());
        self.record_exit(status);
    }

    fn summary(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({
            "job": self.id, "command": self.command, "cwd": shown(&self.cwd), "pid": self.pid,
            "running": state.exit.is_none(),
            "exit_code": state.exit.and_then(|e| e.0), "signal": state.exit.and_then(|e| e.1),
            "ms": state.ended_ms.unwrap_or_else(|| self.started.elapsed().as_millis() as u64),
            "stdout_bytes": state.out[0].end(), "stderr_bytes": state.out[1].end(),
        })
    }
}

/// The jobs of this server (= this link).
#[derive(Default)]
pub(super) struct Jobs {
    all: Mutex<Vec<Arc<Job>>>,
    next: AtomicU64,
}

impl Jobs {
    fn get(&self, id: &str) -> Result<Arc<Job>, String> {
        self.all
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == id)
            .cloned()
            .ok_or_else(|| format!("没有这个后台任务：{id}"))
    }

    /// End every job now (the link is ending).
    pub(super) fn end_all(&self) {
        let all: Vec<_> = self.all.lock().unwrap().drain(..).collect();
        for job in all {
            job.stop(Duration::ZERO);
        }
    }
}

fn pump(job: &Arc<Job>, stream: impl std::io::Read + Send + 'static, i: usize) {
    let job = job.clone();
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    job.state.lock().unwrap().out[i].push(&buf[..n]);
                    job.changed.notify_all();
                }
            }
        }
    });
}

impl Host {
    pub(super) fn job_start(&self, args: &Value) -> Result<Value, String> {
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
        {
            let mut all = self.jobs.all.lock().unwrap();
            if all.iter().filter(|j| j.running()).count() >= RUNNING_MAX {
                return Err(format!(
                    "后台任务最多同时 {RUNNING_MAX} 个；先用 job_stop 结束不要的"
                ));
            }
            while all.len() >= KEPT_MAX {
                match all.iter().position(|j| !j.running()) {
                    Some(i) => {
                        all.remove(i);
                    }
                    None => break,
                }
            }
        }
        let mut spawned = exec::spawn_shell(command, &cwd, std::process::Stdio::null())?;
        let (stdout, stderr) = (spawned.child.stdout.take(), spawned.child.stderr.take());
        let n = self.jobs.next.fetch_add(1, Ordering::Relaxed) + 1;
        let job = Arc::new(Job {
            id: format!("job-{n}"),
            command: command.to_string(),
            cwd: cwd.clone(),
            pid: spawned.child.id(),
            started: Instant::now(),
            spawned: Mutex::new(Some(spawned)),
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        if let Some(s) = stdout {
            pump(&job, s, 0);
        }
        if let Some(s) = stderr {
            pump(&job, s, 1);
        }
        // Notice the end without holding the child's lock for long.
        {
            let job = job.clone();
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let mut spawned = job.spawned.lock().unwrap();
                    let Some(s) = spawned.as_mut() else { break };
                    match s.child.try_wait() {
                        Ok(None) => continue,
                        Ok(Some(status)) => {
                            drop(spawned);
                            job.record_exit(Some(status));
                            break;
                        }
                        Err(_) => {
                            drop(spawned);
                            job.record_exit(None);
                            break;
                        }
                    }
                }
            });
        }
        self.jobs.all.lock().unwrap().push(job.clone());
        Ok(json!({"job": job.id, "pid": job.pid, "cwd": shown(&cwd), "shell": shell_name()}))
    }

    pub(super) fn job_read(&self, args: &Value, call: &Call) -> Result<Value, String> {
        let job = self.jobs.get(arg_str(args, "job").unwrap_or(""))?;
        let offsets = [
            args.get("stdout_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            args.get("stderr_offset")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ];
        let max = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(READ_DEFAULT)
            .clamp(1, READ_MAX) as usize;
        let wait = Duration::from_millis(
            args.get("wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(WAIT_MAX_MS),
        );
        let until = Instant::now() + wait;
        let mut state = job.state.lock().unwrap();
        while state.exit.is_none()
            && state.out[0].end() <= offsets[0]
            && state.out[1].end() <= offsets[1]
            && !call.cancelled()
        {
            let now = Instant::now();
            if now >= until {
                break;
            }
            state = job
                .changed
                .wait_timeout(state, (until - now).min(Duration::from_millis(100)))
                .unwrap()
                .0;
        }
        let ended = state.exit.is_some();
        let stdout = state.out[0].read(offsets[0], max, ended);
        let stderr = state.out[1].read(offsets[1], max, ended);
        let exit = state.exit;
        drop(state);
        Ok(json!({
            "job": job.id, "running": exit.is_none(),
            "exit_code": exit.and_then(|e| e.0), "signal": exit.and_then(|e| e.1),
            "stdout": stdout, "stderr": stderr,
        }))
    }

    pub(super) fn job_stop(&self, args: &Value) -> Result<Value, String> {
        let job = self.jobs.get(arg_str(args, "job").unwrap_or(""))?;
        let grace = Duration::from_millis(
            args.get("grace_ms")
                .and_then(Value::as_u64)
                .unwrap_or(2000)
                .min(30_000),
        );
        job.stop(grace);
        self.jobs
            .all
            .lock()
            .unwrap()
            .retain(|j| !Arc::ptr_eq(j, &job));
        Ok(job.summary())
    }

    pub(super) fn jobs_list(&self) -> Result<Value, String> {
        let all: Vec<Value> = self
            .jobs
            .all
            .lock()
            .unwrap()
            .iter()
            .map(|j| j.summary())
            .collect();
        Ok(json!({"jobs": all}))
    }
}

/// Jobs by id for tests.
#[cfg(test)]
pub(super) fn ids(host: &Host) -> std::collections::HashMap<String, bool> {
    host.jobs
        .all
        .lock()
        .unwrap()
        .iter()
        .map(|j| (j.id.clone(), j.running()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};
    use super::*;

    #[test]
    fn a_job_runs_on_its_output_is_read_by_offset_and_stop_ends_it() {
        let dir = temp("jobs");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "Write-Output first; Start-Sleep -Milliseconds 200; Write-Output second; Start-Sleep -Seconds 60"
        } else {
            "echo first; sleep 0.2; echo second; sleep 60"
        };
        let started = host.job_start(&json!({"command": command})).unwrap();
        let id = started["job"].as_str().unwrap().to_string();
        let call = Call::detached();
        let mut text = String::new();
        let mut offset = 0;
        let until = Instant::now() + Duration::from_secs(20);
        while !text.contains("second") && Instant::now() < until {
            let r = host
                .job_read(
                    &json!({"job": id, "stdout_offset": offset, "wait_ms": 2000}),
                    &call,
                )
                .unwrap();
            assert_eq!(r["running"], true);
            assert_eq!(r["stdout"]["offset"], offset);
            text.push_str(r["stdout"]["data"].as_str().unwrap());
            offset = r["stdout"]["next_offset"].as_u64().unwrap();
        }
        assert!(text.contains("first") && text.contains("second"), "{text}");
        assert_eq!(ids(&host).get(&id), Some(&true));
        let listed = host.jobs_list().unwrap();
        assert_eq!(listed["jobs"][0]["job"], id.as_str());
        let t = Instant::now();
        let stopped = host.job_stop(&json!({"job": id, "grace_ms": 500})).unwrap();
        assert_eq!(stopped["running"], false);
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(host.job_read(&json!({"job": id}), &call).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_job_reports_its_exit_and_end_all_ends_running_ones() {
        let dir = temp("jobs-end");
        let host = host_in(&dir);
        let quick = host.job_start(&json!({"command": "exit 3"})).unwrap();
        let id = quick["job"].as_str().unwrap();
        let r = host
            .job_read(&json!({"job": id, "wait_ms": 10_000}), &Call::detached())
            .unwrap();
        assert_eq!(r["running"], false);
        assert_eq!(r["exit_code"], 3);
        let sleep = if cfg!(windows) {
            "Start-Sleep -Seconds 60"
        } else {
            "sleep 60"
        };
        host.job_start(&json!({"command": sleep})).unwrap();
        host.jobs.end_all();
        assert!(ids(&host).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
