//! Jobs: every command runs as a job in the device's default shell (PowerShell with UTF-8 output on
//! Windows, `/bin/sh -c` elsewhere). Output goes to a file on the device (so a dropped channel loses
//! nothing) and is read back from an offset. A kill ends the whole process tree: a Windows Job Object,
//! or the process group on Unix.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::protocol::DeviceError;
use crate::util::{iso_now, short_id};

pub const MAX_JOBS: usize = 4;
/// Output kept per job on disk.
const OUTPUT_CAP: u64 = 64 << 20;
/// Output returned per answer.
const OUTPUT_CHUNK: usize = 64 * 1024;
/// Ended jobs kept for reading back.
const KEEP_ENDED: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited,
    Killed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Exited => "exited",
            Status::Killed => "killed",
        }
    }
}

struct State {
    status: Status,
    exit_code: Option<i32>,
    killed: bool,
    open_streams: u8,
    process_done: bool,
    ended_at: Option<Instant>,
}

/// Console events (kind, session, job, text) go to whoever listens (the gateway's console stream).
pub type EventSink = Arc<dyn Fn(&str, &str, Option<&str>, &str) + Send + Sync>;

pub struct Job {
    pub id: String,
    pub command: String,
    pub session: String,
    pub started_at: String,
    log_path: PathBuf,
    log: Mutex<File>,
    written: Mutex<u64>,
    state: Mutex<State>,
    changed: Condvar,
    stdin: Mutex<Option<ChildStdin>>,
    tree: ProcessTree,
}

impl Job {
    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    fn append(&self, bytes: &[u8]) {
        let mut written = self.written.lock().unwrap();
        if *written >= OUTPUT_CAP {
            return;
        }
        let room = (OUTPUT_CAP - *written) as usize;
        let mut log = self.log.lock().unwrap();
        let slice = &bytes[..bytes.len().min(room)];
        if log.write_all(slice).is_ok() {
            *written += slice.len() as u64;
            if *written >= OUTPUT_CAP {
                let note = "\n[输出超过 64 MiB，后面的不再保留]\n".as_bytes();
                if log.write_all(note).is_ok() {
                    *written += note.len() as u64;
                }
            }
        }
    }

    /// Wait until the job ends, `seconds` pass, or `cancel` is set.
    pub fn wait(&self, seconds: f64, cancel: &AtomicBool) {
        if seconds <= 0.0 {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        let mut state = self.state.lock().unwrap();
        while state.status == Status::Running && !cancel.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = (deadline - now).min(Duration::from_millis(100));
            state = self.changed.wait_timeout(state, slice).unwrap().0;
        }
    }

    /// The job as the protocol reports it, with output from `from` (at most 64 KiB, cut at a UTF-8 boundary).
    pub fn view(&self, from: u64) -> Value {
        let (status, exit_code) = {
            let s = self.state.lock().unwrap();
            (s.status, s.exit_code)
        };
        let written = *self.written.lock().unwrap();
        let start = from.min(written);
        let mut bytes = Vec::new();
        if let Ok(mut f) = File::open(&self.log_path)
            && f.seek(SeekFrom::Start(start)).is_ok()
        {
            let _ = f.take(OUTPUT_CHUNK as u64).read_to_end(&mut bytes);
        }
        let keep = complete_utf8_len(&bytes);
        bytes.truncate(keep);
        let mut out = json!({
            "job": self.id,
            "status": status.as_str(),
            "output": String::from_utf8_lossy(&bytes),
            "offset": start + bytes.len() as u64,
        });
        if status != Status::Running {
            out["exitCode"] = json!(exit_code);
        }
        out
    }

    pub fn write_input(&self, text: &str) -> Result<(), DeviceError> {
        if self.status() != Status::Running {
            return Err(DeviceError::new("JOB_ENDED", "the job has ended"));
        }
        let mut stdin = self.stdin.lock().unwrap();
        match stdin.as_mut() {
            Some(pipe) => pipe
                .write_all(text.as_bytes())
                .and_then(|_| pipe.flush())
                .map_err(|e| DeviceError::new("FAILED", format!("could not write input: {e}"))),
            None => Err(DeviceError::new("JOB_ENDED", "the job's input is closed")),
        }
    }

    /// End the job and every process it started.
    pub fn kill(&self) {
        {
            let mut s = self.state.lock().unwrap();
            if s.status != Status::Running {
                return;
            }
            s.killed = true;
        }
        self.tree.kill();
    }

    fn stream_closed(&self) {
        let mut s = self.state.lock().unwrap();
        s.open_streams = s.open_streams.saturating_sub(1);
        self.maybe_end(&mut s);
    }

    fn process_exited(&self, code: Option<i32>) {
        let mut s = self.state.lock().unwrap();
        s.process_done = true;
        s.exit_code = code;
        self.maybe_end(&mut s);
    }

    fn maybe_end(&self, s: &mut State) {
        // Ended when the shell exited and its output pipes closed (children holding them keep it running),
        // or at once after a kill.
        if s.status == Status::Running && s.process_done && (s.open_streams == 0 || s.killed) {
            s.status = if s.killed {
                Status::Killed
            } else {
                Status::Exited
            };
            s.ended_at = Some(Instant::now());
            self.stdin.lock().unwrap().take();
            self.changed.notify_all();
        }
    }
}

/// How many leading bytes form complete UTF-8 (an incomplete character at the end waits for the next read).
fn complete_utf8_len(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=3.min(n) {
        let b = bytes[n - back];
        if b & 0b1100_0000 == 0b1000_0000 {
            continue; // continuation byte
        }
        let need = if b >= 0b1111_0000 {
            4
        } else if b >= 0b1110_0000 {
            3
        } else if b >= 0b1100_0000 {
            2
        } else {
            1
        };
        return if need > back { n - back } else { n };
    }
    n
}

pub struct Jobs {
    dir: PathBuf,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    events: EventSink,
}

impl Jobs {
    pub fn new(dir: &Path, events: EventSink) -> Jobs {
        let _ = std::fs::create_dir_all(dir);
        // Output of an earlier run is not reachable any more; clear it.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Jobs {
            dir: dir.to_path_buf(),
            jobs: Mutex::new(HashMap::new()),
            events,
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn running(&self) -> usize {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|j| j.status() == Status::Running)
            .count()
    }

    pub fn list(&self, session: &str) -> Vec<Value> {
        let jobs = self.jobs.lock().unwrap();
        let mut list: Vec<&Arc<Job>> = jobs.values().collect();
        list.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        list.iter()
            .map(|j| {
                json!({"job": j.id, "command": j.command, "status": j.status().as_str(), "startedAt": j.started_at, "mine": j.session == session})
            })
            .collect()
    }

    /// The sessions with a running job (for the tray's "in use by").
    pub fn running_sessions(&self) -> Vec<String> {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .filter(|j| j.status() == Status::Running)
            .map(|j| j.session.clone())
            .collect()
    }

    pub fn kill_all(&self) {
        let jobs: Vec<Arc<Job>> = self.jobs.lock().unwrap().values().cloned().collect();
        for job in jobs {
            if job.status() == Status::Running {
                (self.events)("kill", &job.session, Some(&job.id), "stopped on the device");
                job.kill();
            }
        }
    }

    fn prune(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        let mut ended: Vec<(Instant, String)> = jobs
            .values()
            .filter_map(|j| j.state.lock().unwrap().ended_at.map(|t| (t, j.id.clone())))
            .collect();
        if ended.len() <= KEEP_ENDED {
            return;
        }
        ended.sort();
        for (_, id) in ended.iter().take(ended.len() - KEEP_ENDED) {
            if let Some(job) = jobs.remove(id) {
                let _ = std::fs::remove_file(&job.log_path);
            }
        }
    }

    /// Start a command; it runs until it ends or is killed, whoever is listening.
    pub fn start(
        &self,
        command: &str,
        cwd: &Path,
        session: &str,
        shell: Shell,
    ) -> Result<Arc<Job>, DeviceError> {
        self.prune();
        if self.running() >= MAX_JOBS {
            return Err(DeviceError::new(
                "BUSY",
                format!("at most {MAX_JOBS} jobs run at once"),
            ));
        }
        let id = short_id("job_");
        let log_path = self.dir.join(format!("{id}.log"));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| DeviceError::new("FAILED", format!("cannot keep output: {e}")))?;
        let mut cmd = shell.command(command);
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = spawn_tree(cmd)
            .map_err(|e| DeviceError::new("FAILED", format!("could not start the command: {e}")))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let job = Arc::new(Job {
            id: id.clone(),
            command: command.to_string(),
            session: session.to_string(),
            started_at: iso_now(),
            log_path,
            log: Mutex::new(log),
            written: Mutex::new(0),
            state: Mutex::new(State {
                status: Status::Running,
                exit_code: None,
                killed: false,
                open_streams: 2,
                process_done: false,
                ended_at: None,
            }),
            changed: Condvar::new(),
            stdin: Mutex::new(child.stdin.take()),
            tree,
        });
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());
        (self.events)("exec", session, Some(&id), command);
        for stream in [
            stdout.map(|s| Box::new(s) as Box<dyn Read + Send>),
            stderr.map(|s| Box::new(s) as Box<dyn Read + Send>),
        ] {
            let job = job.clone();
            let events = self.events.clone();
            std::thread::spawn(move || {
                if let Some(mut reader) = stream {
                    let mut buf = [0u8; 8192];
                    let mut pending: Vec<u8> = Vec::new();
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                job.append(&buf[..n]);
                                pending.extend_from_slice(&buf[..n]);
                                let keep = complete_utf8_len(&pending);
                                let text = String::from_utf8_lossy(&pending[..keep]).into_owned();
                                pending.drain(..keep);
                                if !text.is_empty() {
                                    events("output", &job.session, Some(&job.id), &text);
                                }
                            }
                        }
                    }
                }
                job.stream_closed();
            });
        }
        let waiter = job.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            waiter.process_exited(code);
            // Wait for the end (pipes closed, or killed) before telling the console.
            let mut state = waiter.state.lock().unwrap();
            while state.status == Status::Running {
                state = waiter.changed.wait(state).unwrap();
            }
            let text = match (state.status, state.exit_code) {
                (Status::Killed, _) => "killed".to_string(),
                (_, Some(c)) => format!("exit {c}"),
                _ => "exit".to_string(),
            };
            drop(state);
            events("exit", &waiter.session, Some(&waiter.id), &text);
        });
        Ok(job)
    }
}

/// The device's default shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    PowerShell,
    Sh,
}

impl Shell {
    pub fn default_for_os() -> Shell {
        if cfg!(windows) {
            Shell::PowerShell
        } else {
            Shell::Sh
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::PowerShell => "powershell",
            Shell::Sh => "sh",
        }
    }

    fn command(self, command: &str) -> Command {
        match self {
            Shell::PowerShell => {
                // UTF-8 everywhere (owner decision 2026-10-08) and no progress records on stderr. The
                // command is passed as it is, in plain text (`-Command`): no encoded commands, no
                // execution-policy override. It only runs after the person allowed it on this computer.
                let script = format!(
                    "$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $OutputEncoding=[System.Text.Encoding]::UTF8; {command}"
                );
                let mut cmd = Command::new("powershell.exe");
                cmd.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-InputFormat",
                    "None",
                    "-Command",
                    &script,
                ])
                .env("PYTHONIOENCODING", "utf-8");
                cmd
            }
            Shell::Sh => {
                let mut cmd = Command::new("/bin/sh");
                cmd.arg("-c").arg(command);
                if std::env::var_os("LANG").is_none() {
                    cmd.env("LANG", "C.UTF-8");
                }
                cmd
            }
        }
    }
}

// ---- process trees ----

#[cfg(windows)]
struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
// SAFETY: a Job Object handle may be used from any thread.
unsafe impl Send for ProcessTree {}
#[cfg(windows)]
// SAFETY: as above; TerminateJobObject is thread-safe.
unsafe impl Sync for ProcessTree {}

#[cfg(windows)]
impl ProcessTree {
    fn kill(&self) {
        // SAFETY: the handle stays open until drop.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // SAFETY: closing our own handle (kill-on-close ends what is left).
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

/// Start the command (no console window: its output goes to the job's log and the web console), then
/// put it in a fresh Job Object with kill-on-close, so a kill or the app's exit ends its whole tree.
#[cfg(windows)]
fn spawn_tree(mut cmd: Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: plain Win32 calls on handles we own; the job handle is kept in ProcessTree.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let tree = ProcessTree { job };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        // No hidden window (docs/AV-HYGIENE.md): from the desktop app the shell gets its own,
        // visible console; from the command line it shares the terminal.
        let mut child = cmd.spawn()?;
        if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
            let err = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        Ok((child, tree))
    }
}

#[cfg(unix)]
struct ProcessTree {
    pgid: i32,
}

#[cfg(unix)]
impl ProcessTree {
    fn kill(&self) {
        // SAFETY: signalling our own child's process group.
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
    }
}

/// Start the command in its own process group, so a kill reaches everything it started.
#[cfg(unix)]
fn spawn_tree(mut cmd: Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let child = cmd.spawn()?;
    let pgid = child.id() as i32;
    Ok((child, ProcessTree { pgid }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_boundaries() {
        let s = "a中".as_bytes(); // 61 e4 b8 ad
        assert_eq!(complete_utf8_len(s), 4);
        assert_eq!(complete_utf8_len(&s[..3]), 1);
        assert_eq!(complete_utf8_len(&s[..2]), 1);
        assert_eq!(complete_utf8_len(b"abc"), 3);
        assert_eq!(complete_utf8_len(&[]), 0);
    }
}
