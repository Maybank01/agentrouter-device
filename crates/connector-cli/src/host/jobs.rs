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
//!
//! **Events of a space's runtime** (Cloud #3607, #3598 G4; PROJECT-SERVICE §9.8.2): a job the platform starts for a
//! space's runtime instance with `events: {space}` (and its `lease`) may raise that space's declared events by printing
//! one line each on its output — [`EVENT_LINE`] followed by `{"event": "<name>", "payload": {…}, "key"?: "<text>"}`
//! (GitHub Actions' workflow-command pattern: any language, no credential, no file the sandbox must allow). Each such
//! line goes to the aggregator as the notification [`EVENT_METHOD`], which writes it into the host's outbox; the
//! platform accepts it only for the space whose runtime instance runs on this link, as one of that space's declared
//! events. The line stays in the job's output; a line that is not a well-formed event gets one
//! `[agentrouter] event not sent: …` line after it, so the program's author reads why.
//!
//! **Its space's ops** (Cloud #3675, RPS pilot #3641 finding 5): the same job calls one of its space's declared ops (a
//! Helper such as `record_match`, which writes a table and raises the event the op declares) by printing
//! [`OP_LINE`] followed by `{"op": "<name>", "args": {…}, "key"?: "<text>"}`. It travels the same way (the notification
//! [`EVENT_METHOD`] with `op` instead of `event`, the host's outbox); the platform runs it through its one op path as
//! the space's runtime, only for the space whose runtime instance runs on this link. An event an op raises is written
//! only by that op, so a runtime that wants it calls the op.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use agentrouter_device::exec::{self, Spawned};

use super::out::{Out, Spill};
use super::run::{PUSH_BYTES, PUSH_EVERY, utf8_head};
use super::{Call, Fail, Host, arg_str, shell_name, shown, tool};

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
/// A job output line that raises an event of the space whose runtime the job runs (`events`, Cloud #3607).
pub const EVENT_LINE: &str = "::agentrouter-event::";
/// A job output line that calls one of that space's declared ops (Cloud #3675).
pub const OP_LINE: &str = "::agentrouter-op::";
/// How the note after a refused op line starts (an event line's says `event not sent`).
const OP_NOTE: &str = "op call: ";
/// The notification the `host` server sends the aggregator for each event or op line (only the built-in `host` server's
/// are taken).
pub const EVENT_METHOD: &str = "notifications/agentrouter/event";
/// An event line longer than this is not sent (the outbox takes payloads ≤ 32 KiB).
const EVENT_LINE_MAX: usize = 64 << 10;

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
                "lease": {"type": "object", "description": "The platform's control lease this job runs under (an instance and its epoch, Cloud PROJECT-SERVICE §4.8 \"Lease epochs\"): listed after the link reconnects, so a job whose instance ended meanwhile is stopped (lease_lost).", "properties": {
                    "instance": {"type": "string"}, "epoch": {"type": "integer", "minimum": 1},
                }, "required": ["instance", "epoch"]},
                "events": {"type": "object", "description": "The space whose runtime this job is (with lease; Cloud #3607): each output line `::agentrouter-event::{\"event\", \"payload\", \"key\"?}` is sent to the host's outbox as that space's event, and each `::agentrouter-op::{\"op\", \"args\", \"key\"?}` as a call of one of that space's declared ops (Cloud #3675), through the lease's instance.", "properties": {
                    "space": {"type": "string"},
                }, "required": ["space"]},
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
            "End a background job's whole process tree (a polite stop, then a kill after grace_ms, default 2000) and forget it. With instance: only when the job runs under that instance's lease (else LEASE_MISMATCH); reason is answered back.",
            json!({"type": "object", "properties": {
                "job": {"type": "string"},
                "grace_ms": {"type": "integer", "minimum": 0, "maximum": 30_000},
                "instance": {"type": "string"},
                "reason": {"type": "string", "maxLength": 64},
            }, "required": ["job"]}),
            false,
        ),
        tool(
            "jobs",
            "The background jobs of this link: id, command, cwd, pid, running, exit code, output sizes, and the lease a platform instance's job runs under.",
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
    /// `{instance, epoch}`: the platform's control lease this job runs under (Cloud §4.8 "Lease epochs").
    lease: Option<Value>,
    /// Where its event lines go (`events`): the space and instance they are raised for, and the server's notifier.
    events: Option<Arc<EventsTo>>,
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
            "lease": self.lease, "events": self.events.as_ref().map(|e| json!({"space": e.space})),
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

/// Where a job's event lines go (`job_start` `events`).
pub(super) struct EventsTo {
    space: String,
    instance: String,
    notify: super::Notify,
}

/// One stream's unfinished line, for event lines: the bytes since the last newline, or "skipping" a line that grew
/// past [`EVENT_LINE_MAX`] (`true` when that line was an event line, which then gets its note at its end).
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
    skipping: Option<bool>,
}

impl Lines {
    /// The complete lines in `bytes` (after what came before), without their line ends; long ones are left out and
    /// their event-ness kept for the note.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Result<Vec<u8>, &'static str>> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                match self.skipping.take() {
                    Some(true) => out.push(Err("the line is longer than 64 KiB")),
                    Some(false) => {}
                    None => {
                        let mut line = std::mem::take(&mut self.partial);
                        if line.last() == Some(&b'\r') {
                            line.pop();
                        }
                        out.push(Ok(line));
                    }
                }
                self.partial.clear();
                continue;
            }
            if self.skipping.is_some() {
                continue;
            }
            self.partial.push(b);
            if self.partial.len() > EVENT_LINE_MAX {
                self.skipping = Some(
                    self.partial.starts_with(EVENT_LINE.as_bytes())
                        || self.partial.starts_with(OP_LINE.as_bytes()),
                );
                self.partial = Vec::new();
            }
        }
        out
    }
}

/// An event name as the platform takes it: lower-case words joined by dots (project-service host-events `NAME`).
fn event_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name.split('.').all(|w| {
            let mut c = w.chars();
            c.next().is_some_and(|f| f.is_ascii_lowercase())
                && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        })
}

/// An op name as a space declares it (project-service registry `OP_NAME`: letters, digits, `_` and `-`, 1 to 64).
fn op_name_ok(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// The key of an event or op line: none, or a text of 1 to 400 characters without control characters.
fn line_key(obj: &serde_json::Map<String, Value>) -> Result<Option<String>, String> {
    match obj.get("key") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(k))
            if !k.is_empty() && k.len() <= 400 && !k.chars().any(char::is_control) =>
        {
            Ok(Some(k.clone()))
        }
        Some(_) => {
            Err("key is a text of 1 to 400 characters (it makes a repeated line one call)".into())
        }
    }
}

/// The op call an op line makes, as the notification's params (`op` and its `args` as the payload), or why it is not
/// sent (Cloud #3675).
fn op_of(body: &str, events: &EventsTo, job: &str) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body.trim()).map_err(|e| {
        format!("after {OP_LINE} comes one JSON object {{\"op\", \"args\", \"key\"?}} ({e})")
    })?;
    let Some(obj) = parsed.as_object() else {
        return Err(format!(
            "after {OP_LINE} comes one JSON object, not {parsed}"
        ));
    };
    let op = obj.get("op").and_then(Value::as_str).unwrap_or("");
    if !op_name_ok(op) {
        return Err(format!(
            "op {:?} is not an op name (letters, digits, _ and -, as the space declares it under ops:)",
            obj.get("op").unwrap_or(&Value::Null)
        ));
    }
    let args = obj.get("args").cloned().unwrap_or_else(|| json!({}));
    if !args.is_object() {
        return Err(format!(
            "the args of {op} are an object of its declared params"
        ));
    }
    let key = line_key(obj)?;
    if let Some(extra) = obj
        .keys()
        .find(|k| !["op", "args", "key"].contains(&k.as_str()))
    {
        return Err(format!("an op line takes op, args and key, not {extra}"));
    }
    Ok(json!({
        "space": events.space, "instance": events.instance, "op": op, "payload": args, "key": key,
        "cause": format!("runtime:{} {job}", events.instance),
    }))
}

/// The event an event line raises (or the op call an op line makes), as the notification's params, or why it is not
/// sent; None for any other line.
pub(super) fn event_of(line: &str, events: &EventsTo, job: &str) -> Option<Result<Value, String>> {
    if let Some(body) = line.strip_prefix(OP_LINE) {
        return Some(op_of(body, events, job).map_err(|why| format!("{OP_NOTE}{why}")));
    }
    let body = line.strip_prefix(EVENT_LINE)?;
    let parsed: Value = match serde_json::from_str(body.trim()) {
        Ok(v) => v,
        Err(e) => {
            return Some(Err(format!(
                "after {EVENT_LINE} comes one JSON object {{\"event\", \"payload\", \"key\"?}} ({e})"
            )));
        }
    };
    let Some(obj) = parsed.as_object() else {
        return Some(Err(format!(
            "after {EVENT_LINE} comes one JSON object, not {parsed}"
        )));
    };
    let event = obj.get("event").and_then(Value::as_str).unwrap_or("");
    if !event_name_ok(event) {
        return Some(Err(format!(
            "event {:?} is not an event name (lower-case words joined by dots, as the space declares it)",
            obj.get("event").unwrap_or(&Value::Null)
        )));
    }
    let payload = obj.get("payload").cloned().unwrap_or_else(|| json!({}));
    if !payload.is_object() {
        return Some(Err(format!(
            "the payload of {event} is an object of its declared fields"
        )));
    }
    let key = match line_key(obj) {
        Ok(k) => k,
        Err(why) => return Some(Err(why)),
    };
    if let Some(extra) = obj
        .keys()
        .find(|k| !["event", "payload", "key"].contains(&k.as_str()))
    {
        return Some(Err(format!(
            "an event line takes event, payload and key, not {extra}"
        )));
    }
    Some(Ok(json!({
        "space": events.space, "instance": events.instance, "event": event, "payload": payload, "key": key,
        "cause": format!("runtime:{} {job}", events.instance),
    })))
}

fn pump(job: &Arc<Job>, stream: impl std::io::Read + Send + 'static, i: usize) {
    let job = job.clone();
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut buf = vec![0u8; 64 << 10];
        let mut lines = Lines::default();
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    job.state.lock().unwrap().out[i].push(&buf[..n]);
                    if let Some(events) = &job.events {
                        for line in lines.feed(&buf[..n]) {
                            let refused = match line {
                                Err(why) => Some(why.to_string()),
                                Ok(bytes) => match event_of(
                                    &String::from_utf8_lossy(&bytes),
                                    events,
                                    &job.id,
                                ) {
                                    None => None,
                                    Some(Ok(params)) => {
                                        (events.notify)(
                                            &json!({"jsonrpc": "2.0", "method": EVENT_METHOD, "params": params}),
                                        );
                                        None
                                    }
                                    Some(Err(why)) => Some(why),
                                },
                            };
                            if let Some(why) = refused {
                                let note = match why.strip_prefix(OP_NOTE) {
                                    Some(why) => format!("[agentrouter] op call not sent: {why}\n"),
                                    None => format!("[agentrouter] event not sent: {why}\n"),
                                };
                                job.state.lock().unwrap().out[i].push(note.as_bytes());
                            }
                        }
                    }
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

    pub(super) fn job_start(&self, args: &Value, call: &Call) -> Result<Value, Fail> {
        let command = arg_str(args, "command").unwrap_or("");
        if command.trim().is_empty() {
            return Err("command 不能是空的".into());
        }
        let lease = lease_of(args)?;
        let events = events_of(args, lease.as_ref())?.map(|space| {
            Arc::new(EventsTo {
                space,
                instance: lease
                    .as_ref()
                    .and_then(|l| l["instance"].as_str())
                    .unwrap_or("")
                    .to_string(),
                notify: call.notify.clone(),
            })
        });
        let cwd = self.work_dir("job_start", args)?;
        {
            let mut all = self.jobs.all.lock().unwrap();
            if all.iter().filter(|j| j.running()).count() >= RUNNING_MAX {
                return Err(
                    format!("后台任务最多同时 {RUNNING_MAX} 个；先用 job_stop 结束不要的").into(),
                );
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
            lease,
            events,
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
        Ok(self.wait_for_end(&job, started, Duration::from_millis(wait), call)?)
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
        // A stop on an instance's behalf (the platform's lease_lost after a reconnect) ends only that instance's job:
        // job ids restart with hostd, so the id alone may name another job by now.
        if let Some(instance) = arg_str(args, "instance") {
            let runs_under = job
                .lease
                .as_ref()
                .and_then(|l| l.get("instance"))
                .and_then(Value::as_str);
            if runs_under != Some(instance) {
                return Err(format!(
                    "LEASE_MISMATCH: {} runs under {}, not {instance}; nothing was stopped",
                    job.id,
                    runs_under.map_or("no instance's lease".to_string(), |i| format!(
                        "the lease of {i}"
                    ))
                ));
            }
        }
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
        let mut answer = job.summary();
        if let (Some(reason), Some(obj)) = (
            arg_str(args, "reason").filter(|r| !r.is_empty() && r.len() <= 64),
            answer.as_object_mut(),
        ) {
            obj.insert("reason".into(), json!(reason));
        }
        Ok(answer)
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

/// A job's `lease` argument: `{instance: ins_…|inst_…, epoch ≥ 1}` (Cloud §4.8 "Lease epochs"), or none.
fn lease_of(args: &Value) -> Result<Option<Value>, String> {
    let Some(lease) = args.get("lease").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let instance = lease.get("instance").and_then(Value::as_str).unwrap_or("");
    let epoch = lease.get("epoch").and_then(Value::as_u64).unwrap_or(0);
    let id_ok = ["ins_", "inst_"].iter().any(|p| {
        instance.strip_prefix(p).is_some_and(|h| {
            h.len() == 24
                && h.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    });
    if !id_ok || epoch == 0 {
        return Err(format!(
            "lease is {{instance: ins_…|inst_… (24 hex), epoch: ≥ 1}}; got {lease}"
        ));
    }
    Ok(Some(json!({"instance": instance, "epoch": epoch})))
}

/// A job's `events` argument: the space (`wrk_…`) whose runtime it is; only with a `lease` (the instance the platform
/// checks the space against).
fn events_of(args: &Value, lease: Option<&Value>) -> Result<Option<String>, String> {
    let Some(events) = args.get("events").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let space = events.get("space").and_then(Value::as_str).unwrap_or("");
    let ok = space.strip_prefix("wrk_").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 64
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if !ok {
        return Err(format!(
            "events is {{space: wrk_…}}, the space whose runtime this job is; got {events}"
        ));
    }
    if lease.is_none() {
        return Err("events needs lease: a job raises a space's events only as that space's runtime instance".into());
    }
    Ok(Some(space.to_string()))
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
    fn a_job_under_an_instance_lease_is_listed_with_it_and_stopped_only_for_that_instance() {
        let dir = temp("jobs-lease");
        let host = host_in(&dir);
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 60"
        } else {
            "sleep 60"
        };
        let instance = format!("ins_{}", "a1".repeat(12));
        let started = host
            .job_start(
                &json!({"command": command, "lease": {"instance": instance, "epoch": 3}}),
                &Call::detached(),
            )
            .unwrap();
        let id = started["job"].as_str().unwrap().to_string();
        let listed = host.jobs_list().unwrap();
        assert_eq!(
            listed["jobs"][0]["lease"],
            json!({"instance": instance, "epoch": 3})
        );
        // Another instance's stop (an id reused after a restart) stops nothing.
        let other = format!("ins_{}", "b2".repeat(12));
        let refused = host
            .job_stop(&json!({"job": id, "instance": other}))
            .unwrap_err();
        assert!(refused.starts_with("LEASE_MISMATCH"), "{refused}");
        assert_eq!(ids(&host).get(&id), Some(&true));
        let stopped = host
            .job_stop(
                &json!({"job": id, "instance": instance, "reason": "lease_lost", "grace_ms": 100}),
            )
            .unwrap();
        assert_eq!(
            (stopped["running"].as_bool(), stopped["reason"].as_str()),
            (Some(false), Some("lease_lost"))
        );
        // A malformed lease is refused before anything starts.
        for bad in [
            json!({"instance": "ins_x", "epoch": 1}),
            json!({"instance": instance, "epoch": 0}),
        ] {
            assert!(
                host.job_start(
                    &json!({"command": command, "lease": bad}),
                    &Call::detached()
                )
                .is_err()
            );
        }
    }

    /// Cloud #3607: a runtime job's event lines become notifications for the aggregator's outbox; a malformed one gets
    /// a note in the job's own output; other output and a job without `events` raise nothing.
    #[test]
    fn a_runtime_jobs_event_lines_are_sent_and_a_bad_one_is_explained_in_its_output() {
        let dir = temp("jobs-events");
        let host = host_in(&dir);
        let sent: Arc<Mutex<Vec<Value>>> = Arc::default();
        let call = Call {
            token: None,
            cancelled: Arc::default(),
            seq: AtomicU64::new(0),
            notify: {
                let sent = sent.clone();
                Arc::new(move |v: &Value| sent.lock().unwrap().push(v.clone()))
            },
        };
        let instance = "ins_0123456789abcdef01234567";
        let lines = [
            "hello",
            r#"::agentrouter-event::{"event":"round.result","payload":{"round":1,"winner":"a"},"key":"r1"}"#,
            r#"::agentrouter-event::{"event":"Bad Name","payload":{}}"#,
            "::agentrouter-event::not json",
            // Cloud #3675: a call of one of the space's ops, and one with args that are not an object.
            r#"::agentrouter-op::{"op":"record_match","args":{"id":"g1","winner":"a"},"key":"g1"}"#,
            r#"::agentrouter-op::{"op":"record_match","args":[1]}"#,
        ];
        let command = if cfg!(windows) {
            lines
                .iter()
                .map(|l| format!("Write-Output '{l}'"))
                .collect::<Vec<_>>()
                .join("; ")
        } else {
            lines
                .iter()
                .map(|l| format!("echo '{l}'"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let started = host
            .job_start(
                &json!({"command": command, "wait_ms": 20_000, "lease": {"instance": instance, "epoch": 1},
                        "events": {"space": "wrk_709329401e7d8c5a5a44067197b06854"}}),
                &call,
            )
            .unwrap();
        let out = started["stdout"]["data"].as_str().unwrap_or("").to_string();
        let events: Vec<Value> = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == EVENT_METHOD)
            .cloned()
            .collect();
        assert_eq!(events.len(), 2, "{events:?}");
        let op = &events[1]["params"];
        assert_eq!(op["op"], "record_match");
        assert_eq!(op["payload"]["winner"], "a");
        assert_eq!(op["key"], "g1");
        assert!(op.get("event").is_none());
        assert_eq!(
            out.matches("[agentrouter] op call not sent: the args of record_match are an object")
                .count(),
            1,
            "{out}"
        );
        let p = &events[0]["params"];
        assert_eq!(p["space"], "wrk_709329401e7d8c5a5a44067197b06854");
        assert_eq!(p["instance"], instance);
        assert_eq!(p["event"], "round.result");
        assert_eq!(p["payload"]["winner"], "a");
        assert_eq!(p["key"], "r1");
        assert!(
            p["cause"]
                .as_str()
                .unwrap()
                .starts_with(&format!("runtime:{instance} job-"))
        );
        assert_eq!(
            out.matches("[agentrouter] event not sent:").count(),
            2,
            "{out}"
        );
        assert!(
            out.contains("hello") && out.contains("not an event name"),
            "{out}"
        );
        // Without events (or without a lease) nothing is raised.
        sent.lock().unwrap().clear();
        let plain = if cfg!(windows) {
            format!("Write-Output '{}'", lines[1])
        } else {
            format!("echo '{}'", lines[1])
        };
        host.job_start(&json!({"command": plain, "wait_ms": 20_000}), &call)
            .unwrap();
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .all(|v| v["method"] != EVENT_METHOD)
        );
        let refused = host.job_start(
            &json!({"command": plain, "events": {"space": "wrk_ab"}}),
            &call,
        );
        assert!(refused.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn event_lines_split_across_reads_and_overlong_ones_are_noted() {
        let mut lines = Lines::default();
        assert!(lines.feed(b"::agentrouter-event::{\"ev").is_empty());
        let got = lines.feed(b"ent\":\"a.b\"}\r\nnext");
        assert_eq!(
            got,
            vec![Ok(b"::agentrouter-event::{\"event\":\"a.b\"}".to_vec())]
        );
        let mut long = EVENT_LINE.as_bytes().to_vec();
        long.extend(std::iter::repeat_n(b'x', EVENT_LINE_MAX + 10));
        long.push(b'\n');
        let mut lines = Lines::default();
        assert_eq!(
            lines.feed(&long),
            vec![Err("the line is longer than 64 KiB")]
        );
        let mut plain = vec![b'y'; EVENT_LINE_MAX + 10];
        plain.push(b'\n');
        assert!(Lines::default().feed(&plain).is_empty());
        assert!(
            event_name_ok("round.result")
                && event_name_ok("turn_ready")
                && !event_name_ok("Round")
                && !event_name_ok("a..b")
        );
    }

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
                Arc::new(Out::at(temp("kept-spill"), 1 << 30, 0)),
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
