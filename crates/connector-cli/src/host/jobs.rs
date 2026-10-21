//! Background jobs: commands that keep running after the call that started them (dev servers, watchers, long
//! builds; DSH's `run_in_background` and its job tools). `job_start` starts one in the real shell, `job_read` returns
//! its output since a byte offset (optionally waiting for more), `job_stop` ends its whole process tree, `jobs`
//! lists them. They live as long as the link: when the host server ends (input closed, a stop signal) every job's
//! tree is ended, and on Windows each job is a kill-on-close job object, so even a killed server takes them along.
//!
//! `job_start` with `wait_ms` also waits for the end (Cloud #2307: a bound session runs every command as a job, so a
//! long one outlives any single call, while a short one still costs one call): while it waits, the output streams as
//! progress like `exec` (for watching: the end's output is never only in progress, which can lose the race with the
//! answer); the answer is `job_read`'s shape and repeats the whole output when a stream fits (≤ 1 MiB), so it is the
//! record and the client keeps what it lacks by the offsets; a job that ended inside the wait and whose output was all
//! answered is forgotten (`forgotten: true`, nothing to stop). Cancelling the call while it waits ends the job, as
//! cancelling `exec` does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use agentrouter_device::exec::{self, Spawned};

use super::out::{Out, Spill};
use super::run::{PUSH_BYTES, PUSH_EVERY, utf8_head};
use super::{Call, Host, arg_str, shell_name, shown, tool};

/// Output kept in memory per stream of a job; older bytes are read from the stream's copy in the output folder.
const KEEP: usize = 8 << 20;
const READ_DEFAULT: u64 = 1 << 20;
const READ_MAX: u64 = 4 << 20;
const WAIT_MAX_MS: u64 = 30_000;
/// `job_start`'s wait for the end, at most (as long as one `exec`).
const START_WAIT_MAX_MS: u64 = exec::TIMEOUT_MAX_MS;
/// The caller's own label of a job (Cloud: the session that started it), shown by `jobs`, so a caller that lost track of
/// its jobs (a restart) can find and end them.
const TAG_MAX: usize = 128;
/// A stream's copy stops at this size unless the caller asks for less (DSH's own `maxSpillBytes` default).
const SPILL_MAX: u64 = 64 << 20;
const TOTAL_SPILL_MAX: u64 = 1 << 30;
/// After the process ended, how long its output pipes may stay open (a grandchild holding them) before the answer.
const DRAIN: Duration = Duration::from_millis(200);
const RUNNING_MAX: usize = 32;
/// Jobs remembered (finished ones are forgotten oldest first).
const KEPT_MAX: usize = 64;

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "job_start",
            "Start a command in the background in this computer's shell (dev servers, watchers, long builds); it keeps running after this call. Read its output with job_read, end it with job_stop. Jobs end with the link. wait_ms waits for its end first (output streams as progress like exec; a job that ended inside the wait is forgotten). A stream past output.<stream>.window bytes is answered as its last window plus spill_path, its complete copy in this environment's output folder (scope: the session's folder there).",
            json!({"type": "object", "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string"},
                "wait_ms": {"type": "integer", "minimum": 0, "maximum": START_WAIT_MAX_MS},
                "tag": {"type": "string", "maxLength": TAG_MAX},
                "scope": {"type": "string", "maxLength": 64},
                "output": {"type": "object", "properties": {
                    "stdout": {"$ref": "#/$defs/stream"}, "stderr": {"$ref": "#/$defs/stream"},
                }},
            }, "required": ["command"], "$defs": {"stream": {"type": "object", "properties": {
                "window": {"type": "integer", "minimum": 1},
                "spill_max": {"type": "integer", "minimum": 1},
            }}}}),
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

/// One stream's kept output: bytes `[start, start + data.len())` of everything it wrote, and (device #100, Cloud
/// #2435) its complete copy in the environment's output folder once it grew past the caller's `window` (or half of
/// [`KEEP`]): nothing is dropped, and a caller that takes only a window reads the tail plus the copy's path.
#[derive(Default)]
pub(super) struct Kept {
    start: u64,
    data: Vec<u8>,
    /// The caller takes at most this much inline: once the stream spilled, reads from before `end - window` start there.
    window: Option<u64>,
    spill: Option<Spill>,
    /// Where the copy goes when it is needed: (folder, scope, file name, size cap). None: never (a terminal's screen).
    spill_to: Option<(Arc<Out>, String, String, u64)>,
}

impl Kept {
    pub(super) fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    fn spills_at(&self) -> u64 {
        let half = (KEEP / 2) as u64;
        self.window.unwrap_or(half).min(half)
    }

    /// Whether a windowed caller is past its window (its output is in the copy; nothing to stream as progress).
    fn past_window(&self) -> bool {
        self.window.is_some() && self.spill.is_some()
    }

    /// From byte `offset` at most `max` bytes as UTF-8 (`last`: the stream ended, a partial character at its end is
    /// shown too): `{data, offset, next_offset, more, dropped}`, plus `spill_path` (the complete copy) once there is
    /// one. A windowed stream that spilled answers from `end - window` at the earliest; bytes before what memory keeps
    /// come from the copy.
    pub(super) fn read(&self, offset: u64, max: usize, last: bool) -> Value {
        let end = self.end();
        let mut from = offset.min(end);
        if let (Some(w), Some(_)) = (self.window, &self.spill) {
            from = from.max(end.saturating_sub(w));
        }
        let from_file = from < self.start && self.spill.as_ref().is_some_and(|s| from < s.written);
        let chunk: Vec<u8> = if from_file {
            let s = self.spill.as_ref().unwrap();
            s.read_at(from, max.min((self.start.min(s.written) - from) as usize))
        } else {
            from = from.max(self.start);
            let lo = (from - self.start) as usize;
            let hi = (lo + max).min(self.data.len());
            self.data[lo..hi].to_vec()
        };
        // A jump may land inside a character: start at the next one.
        let skip = if from != offset {
            chunk
                .iter()
                .take(3)
                .take_while(|b| (**b & 0xC0) == 0x80)
                .count()
        } else {
            0
        };
        let at_end = from + chunk.len() as u64 == end;
        let (text, used) = utf8_head(&chunk[skip..], last && at_end);
        let next = from + (skip + used) as u64;
        let covered = self.spill.as_ref().is_some_and(|s| s.written >= from);
        let mut part = json!({
            "data": text, "offset": from + skip as u64, "next_offset": next,
            "dropped": offset < from && !covered, "more": next < end,
        });
        if let Some(s) = &self.spill {
            part["spill_path"] = json!(s.shown());
            part["spill_bytes"] = json!(s.written);
            if s.full {
                part["spill_full"] = json!(true);
            }
        }
        part
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if let Some(s) = self.spill.as_mut() {
            s.write(bytes);
        } else if self.spill_to.is_some() && self.end() > self.spills_at() {
            let (out, scope, name, max) = self.spill_to.take().unwrap();
            if let Ok((path, file)) = out.create(&scope, &name) {
                let mut s = Spill::new(path, file, max);
                if self.start == 0 {
                    s.write(&self.data);
                } else {
                    s.full = true;
                }
                self.spill = Some(s);
            }
        }
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
    /// Output pipes still open (each pump closes its own at end of file).
    open: u8,
}

pub(super) struct Job {
    id: String,
    command: String,
    tag: Option<String>,
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
            "job": self.id, "command": self.command, "tag": self.tag, "cwd": shown(&self.cwd), "pid": self.pid,
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
        let mut state = job.state.lock().unwrap();
        state.open = state.open.saturating_sub(1);
        drop(state);
        job.changed.notify_all();
    });
}

impl Host {
    /// A job stream's output: the caller's `output.<stream>.window` / `spill_max`, its copy in `scope`'s folder.
    fn kept_for(&self, args: &Value, n: u64, i: usize) -> Kept {
        let name = ["stdout", "stderr"][i];
        let want = args.get("output").and_then(|o| o.get(name));
        let num = |k: &str| {
            want.and_then(|w| w.get(k))
                .and_then(Value::as_u64)
                .filter(|v| *v > 0)
        };
        Kept {
            window: num("window"),
            spill_to: Some((
                self.out.clone(),
                arg_str(args, "scope").unwrap_or("").to_string(),
                format!("job-{n}-{name}.txt"),
                num("spill_max").unwrap_or(SPILL_MAX).min(TOTAL_SPILL_MAX),
            )),
            ..Kept::default()
        }
    }

    pub(super) fn job_start(&self, args: &Value, call: &Call) -> Result<Value, String> {
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
        let mut spawned = exec::spawn_shell(command, &cwd)?;
        let (stdout, stderr) = (spawned.child.stdout.take(), spawned.child.stderr.take());
        let n = self.jobs.next.fetch_add(1, Ordering::Relaxed) + 1;
        let job = Arc::new(Job {
            id: format!("job-{n}"),
            command: command.to_string(),
            tag: arg_str(args, "tag")
                .filter(|t| !t.is_empty() && t.len() <= TAG_MAX)
                .map(str::to_string),
            cwd: cwd.clone(),
            pid: spawned.child.id(),
            started: Instant::now(),
            spawned: Mutex::new(Some(spawned)),
            state: Mutex::new(State {
                open: u8::from(stdout.is_some()) + u8::from(stderr.is_some()),
                out: [0, 1].map(|i| self.kept_for(args, n, i)),
                ..State::default()
            }),
            changed: Condvar::new(),
        });
        if let Some(s) = stdout {
            pump(&job, s, 0);
        }
        if let Some(s) = stderr {
            pump(&job, s, 1);
        }
        // Notice the end without holding the child's lock for long: looked at every 100 ms, and at once (then every
        // 5 ms for a second) when its output pipes have closed, which is usually the moment it ended.
        {
            let job = job.clone();
            std::thread::spawn(move || {
                let mut closed_at: Option<Instant> = None;
                loop {
                    {
                        let state = job.state.lock().unwrap();
                        if state.open == 0 && closed_at.is_none() {
                            closed_at = Some(Instant::now());
                        }
                        let quick = closed_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1));
                        let pause = if quick { 5 } else { 100 };
                        drop(
                            job.changed
                                .wait_timeout(state, Duration::from_millis(pause))
                                .unwrap(),
                        );
                    }
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
        let started =
            json!({"job": job.id, "pid": job.pid, "cwd": shown(&cwd), "shell": shell_name()});
        let wait = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(START_WAIT_MAX_MS);
        if wait == 0 {
            return Ok(started);
        }
        self.wait_for_end(&job, started, Duration::from_millis(wait), call)
    }

    /// `job_start`'s wait: stream new output as progress (coalesced like `exec`), until the job ended and its pipes
    /// drained, the wait is over, or the call is cancelled (which ends the job).
    fn wait_for_end(
        &self,
        job: &Arc<Job>,
        started: Value,
        wait: Duration,
        call: &Call,
    ) -> Result<Value, String> {
        let until = Instant::now() + wait;
        let mut sent = [0u64; 2];
        let mut exited: Option<Instant> = None;
        let mut last_push = Instant::now();
        let done = loop {
            let mut state = job.state.lock().unwrap();
            let finished = |state: &State, exited: &mut Option<Instant>| {
                if state.exit.is_some() && exited.is_none() {
                    *exited = Some(Instant::now());
                }
                state.exit.is_some()
                    && (state.open == 0 || exited.is_some_and(|t| t.elapsed() >= DRAIN))
            };
            let pending = |state: &State| -> usize {
                (0..2)
                    .map(|i| state.out[i].end().saturating_sub(sent[i]) as usize)
                    .sum()
            };
            let mut done = finished(&state, &mut exited);
            let streams = call.streams();
            // Wait for output, the end or the next push time.
            while !done && !call.cancelled() && Instant::now() < until {
                let waiting = if streams { pending(&state) } else { 0 };
                if waiting >= PUSH_BYTES || (waiting > 0 && last_push.elapsed() >= PUSH_EVERY) {
                    break;
                }
                let next = if waiting > 0 {
                    PUSH_EVERY.saturating_sub(last_push.elapsed())
                } else if exited.is_some() {
                    DRAIN.saturating_sub(exited.unwrap().elapsed())
                } else {
                    Duration::from_millis(250)
                };
                let left = until.saturating_duration_since(Instant::now());
                state = job
                    .changed
                    .wait_timeout(state, next.min(left).max(Duration::from_millis(1)))
                    .unwrap()
                    .0;
                done = finished(&state, &mut exited);
            }
            let mut chunks = Vec::new();
            // The end's output goes in the answer, not as progress: a notification sent right before the answer can
            // lose the race with it on the way (the aggregator and the client drop progress of a call that answered).
            if streams && !done {
                for (i, name) in [(0usize, "stdout"), (1usize, "stderr")] {
                    if state.out[i].end() <= sent[i] || state.out[i].past_window() {
                        continue;
                    }
                    let part = state.out[i].read(sent[i], PUSH_BYTES * 4, done);
                    sent[i] = part["next_offset"].as_u64().unwrap_or(sent[i]);
                    let data = part["data"].as_str().unwrap_or("").to_string();
                    if !data.is_empty() {
                        chunks.push((name, data));
                    }
                }
            }
            drop(state);
            if !chunks.is_empty() {
                for (name, data) in chunks {
                    call.progress(&json!({"stream": name, "data": data}));
                }
                last_push = Instant::now();
            }
            if call.cancelled() {
                job.stop(Duration::from_millis(500));
                self.jobs
                    .all
                    .lock()
                    .unwrap()
                    .retain(|j| !Arc::ptr_eq(j, job));
                let mut out = job.summary();
                out["cancelled"] = json!(true);
                return Ok(out);
            }
            if done {
                break true;
            }
            if Instant::now() >= until {
                break false;
            }
        };
        // The answer repeats what was streamed when it all fits (≤ 1 MiB a stream): progress is for watching, the answer
        // is the record, and the client keeps what it has not seen by the offsets.
        let state = job.state.lock().unwrap();
        let from = |i: usize| if sent[i] <= READ_DEFAULT { 0 } else { sent[i] };
        let stdout = state.out[0].read(from(0), READ_DEFAULT as usize, done);
        let stderr = state.out[1].read(from(1), READ_DEFAULT as usize, done);
        let exit = state.exit;
        drop(state);
        let forgotten = done && stdout["more"] == json!(false) && stderr["more"] == json!(false);
        if forgotten {
            self.jobs
                .all
                .lock()
                .unwrap()
                .retain(|j| !Arc::ptr_eq(j, job));
        }
        let mut out = started;
        out["running"] = json!(exit.is_none());
        out["exit_code"] = json!(exit.and_then(|e| e.0));
        out["signal"] = json!(exit.and_then(|e| e.1));
        out["ms"] = json!(job.summary()["ms"]);
        out["stdout"] = stdout;
        out["stderr"] = stderr;
        out["forgotten"] = json!(forgotten);
        Ok(out)
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
    use std::sync::atomic::AtomicU64;

    #[test]
    fn a_job_runs_on_its_output_is_read_by_offset_and_stop_ends_it() {
        let dir = temp("jobs");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "Write-Output first; Start-Sleep -Milliseconds 200; Write-Output second; Start-Sleep -Seconds 60"
        } else {
            "echo first; sleep 0.2; echo second; sleep 60"
        };
        let started = host
            .job_start(&json!({"command": command}), &Call::detached())
            .unwrap();
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
        assert_eq!(listed["jobs"][0]["tag"], Value::Null);
        let t = Instant::now();
        let stopped = host.job_stop(&json!({"job": id, "grace_ms": 500})).unwrap();
        assert_eq!(stopped["running"], false);
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(host.job_read(&json!({"job": id}), &call).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_start_with_wait_ms_answers_a_short_command_in_one_call_and_forgets_it() {
        let dir = temp("jobs-wait");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "Write-Output hello; [Console]::Error.WriteLine('oops'); exit 4"
        } else {
            "echo hello; echo oops >&2; exit 4"
        };
        let t = Instant::now();
        let r = host
            .job_start(
                &json!({"command": command, "wait_ms": 20_000}),
                &Call::detached(),
            )
            .unwrap();
        assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
        assert_eq!(r["running"], false);
        assert_eq!(r["exit_code"], 4);
        assert_eq!(r["forgotten"], true);
        assert!(
            r["stdout"]["data"].as_str().unwrap().contains("hello"),
            "{r}"
        );
        assert!(
            r["stderr"]["data"].as_str().unwrap().contains("oops"),
            "{r}"
        );
        assert!(ids(&host).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn job_start_streams_while_it_waits_and_a_long_job_keeps_running_after_the_wait() {
        let dir = temp("jobs-stream");
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
            "Write-Output one; Start-Sleep -Seconds 30"
        } else {
            "echo one; sleep 30"
        };
        let r = host
            .job_start(
                &json!({"command": command, "wait_ms": 1500, "tag": "ags_1"}),
                &call,
            )
            .unwrap();
        assert_eq!(host.jobs_list().unwrap()["jobs"][0]["tag"], "ags_1");
        assert_eq!(r["running"], true);
        assert_eq!(r["forgotten"], false);
        let streamed: String = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|v| v["params"]["message"].as_str().map(str::to_string))
            .collect();
        assert!(streamed.contains("one"), "{streamed}");
        // The answer repeats what was streamed (it is the record; the client keeps what it lacks by the offsets).
        assert_eq!(r["stdout"]["offset"], 0);
        assert!(r["stdout"]["data"].as_str().unwrap().contains("one"), "{r}");
        assert!(r["stdout"]["next_offset"].as_u64().unwrap() >= 3);
        let id = r["job"].as_str().unwrap().to_string();
        assert_eq!(ids(&host).get(&id), Some(&true));
        host.job_stop(&json!({"job": id, "grace_ms": 200})).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_a_waiting_job_start_ends_the_job() {
        let dir = temp("jobs-cancel");
        let host = host_in(&dir);
        let call = Call::detached();
        let cancel = call.cancelled.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.store(true, Ordering::Relaxed);
        });
        let sleep = if cfg!(windows) {
            "Start-Sleep -Seconds 60"
        } else {
            "sleep 60"
        };
        let t = Instant::now();
        let r = host
            .job_start(&json!({"command": sleep, "wait_ms": 60_000}), &call)
            .unwrap();
        assert!(t.elapsed() < Duration::from_secs(10));
        assert_eq!(r["cancelled"], true);
        assert_eq!(r["running"], false);
        assert!(ids(&host).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_job_reports_its_exit_and_end_all_ends_running_ones() {
        let dir = temp("jobs-end");
        let host = host_in(&dir);
        let quick = host
            .job_start(&json!({"command": "exit 3"}), &Call::detached())
            .unwrap();
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
        host.job_start(&json!({"command": sleep}), &Call::detached())
            .unwrap();
        host.jobs.end_all();
        assert!(ids(&host).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Device #100 / Cloud #2435: a stream past the caller's window answers its tail plus the complete copy's path;
    /// nothing is dropped, and the copy goes when its scope is released.
    #[test]
    fn long_output_stays_in_the_output_folder_and_only_the_window_travels() {
        let dir = temp("jobs-spill");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "1..200000 | ForEach-Object { $_ }"
        } else {
            "seq 1 200000"
        };
        let r = host
            .job_start(
                &json!({"command": command, "wait_ms": 60_000, "scope": "ses-t",
                    "output": {"stdout": {"window": 1000}}}),
                &Call::detached(),
            )
            .unwrap();
        assert_eq!(r["running"], false, "{r}");
        let out = &r["stdout"];
        let data = out["data"].as_str().unwrap();
        assert!(
            data.len() <= 1000 && data.trim_end().ends_with("200000"),
            "{data}"
        );
        assert_eq!(out["dropped"], false);
        let path = out["spill_path"].as_str().expect("a copy");
        let full = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
        let want: String = (1..=200000).map(|n| format!("{n}\n")).collect();
        assert_eq!(full, want);
        assert!(host.out_release(&json!({"scope": "ses-t"})).unwrap()["released"] == true);
        assert!(!std::path::Path::new(path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a window, more than memory keeps is read back from the copy (no "dropped").
    #[test]
    fn output_beyond_memory_is_read_from_the_copy() {
        let mut k = Kept {
            spill_to: Some((
                Arc::new(Out::at(temp("kept-spill"))),
                "s".into(),
                "x.txt".into(),
                SPILL_MAX,
            )),
            ..Kept::default()
        };
        let line = [b'x'; 1 << 20];
        for _ in 0..10 {
            k.push(&line);
        }
        assert!(k.start > 0);
        let r = k.read(0, 16, false);
        assert_eq!(r["dropped"], false);
        assert_eq!(r["data"], "x".repeat(16));
        assert_eq!(r["offset"], 0);
    }
}
