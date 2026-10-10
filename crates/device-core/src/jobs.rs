//! Jobs: every command runs as a job in the device's default shell (PowerShell with UTF-8 output on
//! Windows, `/bin/sh -c` elsewhere). Output goes to a file on the device (so a dropped channel loses
//! nothing) and is read back from an offset. A kill ends the whole process tree: a Windows Job Object
//! the shell is created in (so nothing it starts can leave before it is attached), or the process group
//! on Unix. At the folder levels a Windows shell runs confined (confine.rs): at low integrity, so it can
//! write only in the linked folders and its scratch folder.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::confine::Sandbox;
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
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
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
        sandbox: Option<&Sandbox>,
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
        let spawned = spawn(shell, command, cwd, sandbox)
            .map_err(|e| DeviceError::new("FAILED", format!("could not start the command: {e}")))?;
        let Spawned {
            stdin,
            stdout,
            stderr,
            wait,
            tree,
        } = spawned;
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
            stdin: Mutex::new(stdin),
            tree,
        });
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());
        (self.events)("exec", session, Some(&id), command);
        for stream in [Some(stdout), Some(stderr)] {
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
            let code = wait();
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

    /// The program, its arguments and the extra environment that run `command` in this shell.
    fn argv(self, command: &str) -> (PathBuf, Vec<String>, Vec<(String, String)>) {
        match self {
            Shell::PowerShell => {
                // UTF-8 everywhere (owner decision 2026-10-08) and no progress records on stderr. The
                // command is passed as it is, in plain text (`-Command`): no encoded commands, no
                // execution-policy override. It only runs after the person allowed it on this computer.
                let script = format!(
                    "$ProgressPreference='SilentlyContinue'; [Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $OutputEncoding=[System.Text.Encoding]::UTF8; {command}"
                );
                let args = [
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-InputFormat",
                    "None",
                    "-Command",
                    &script,
                ];
                (
                    powershell_path(),
                    args.iter().map(|a| a.to_string()).collect(),
                    vec![("PYTHONIOENCODING".into(), "utf-8".into())],
                )
            }
            Shell::Sh => {
                let mut env = Vec::new();
                if std::env::var_os("LANG").is_none() {
                    env.push(("LANG".into(), "C.UTF-8".into()));
                }
                (
                    PathBuf::from("/bin/sh"),
                    vec!["-c".into(), command.to_string()],
                    env,
                )
            }
        }
    }
}

/// Windows PowerShell by its full path (not whatever `powershell.exe` the search path finds first).
fn powershell_path() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|root| {
            PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

/// A started shell: its pipes, a way to wait for its exit code, and its process tree.
struct Spawned {
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Box<dyn Read + Send>,
    stderr: Box<dyn Read + Send>,
    wait: Box<dyn FnOnce() -> Option<i32> + Send>,
    tree: ProcessTree,
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

/// One argument quoted the way `CommandLineToArgvW` and the C runtime read it back.
#[cfg(windows)]
fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// The environment block of a new process: the device's own environment, then `overrides` (names
/// compare without case, as Windows does), sorted, as `NAME=value\0…\0\0`.
#[cfg(windows)]
fn environment_block(overrides: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    for (key, value) in overrides {
        env.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
        env.push((key, value));
    }
    env.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in &env {
        block.extend(k.encode_wide());
        block.push('=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// A restricted copy of this process's token (no privileges beyond traversal) at low integrity: what
/// a confined shell runs with (confine.rs).
#[cfg(windows)]
fn low_integrity_token() -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetLengthSid, PSID, SID_AND_ATTRIBUTES,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    const SE_GROUP_INTEGRITY: u32 = 0x20;
    // SAFETY: handles are owned as soon as they are returned; the SID is freed after use.
    unsafe {
        let mut own: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT,
            &mut own,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let own = OwnedHandle::from_raw_handle(own as _);
        let mut low: HANDLE = std::ptr::null_mut();
        if CreateRestrictedToken(
            own.as_raw_handle() as _,
            DISABLE_MAX_PRIVILEGE,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &mut low,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let low = OwnedHandle::from_raw_handle(low as _);
        let mut sid: PSID = std::ptr::null_mut();
        let text: Vec<u16> = "S-1-16-4096".encode_utf16().chain(Some(0)).collect();
        if ConvertStringSidToSidW(text.as_ptr(), &mut sid) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let label = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY,
            },
        };
        let ok = SetTokenInformation(
            low.as_raw_handle() as _,
            TokenIntegrityLevel,
            &label as *const _ as *const _,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
        );
        let err = std::io::Error::last_os_error();
        LocalFree(sid as _);
        if ok == 0 {
            return Err(err);
        }
        Ok(low)
    }
}

/// Start the shell inside a fresh Job Object with kill-on-close, so a kill or the app's exit ends its
/// whole tree. The Job Object is given at creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`): nothing the
/// shell starts can run outside it, and nothing is suspended or resumed (docs/AV-HYGIENE.md). Only the
/// shell's three pipe ends are inherited. With a sandbox it runs confined: the low-integrity token and
/// the sandbox's temporary folder and caches (confine.rs).
#[cfg(windows)]
fn spawn(
    shell: Shell,
    command: &str,
    cwd: &Path,
    sandbox: Option<&Sandbox>,
) -> std::io::Result<Spawned> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    let last = std::io::Error::last_os_error;
    let owned = |h: HANDLE| {
        // SAFETY: a handle just returned to us, owned from here on.
        unsafe { OwnedHandle::from_raw_handle(h as _) }
    };

    let (program, args, extra) = shell.argv(command);
    let mut line = String::new();
    quote_arg(&program.to_string_lossy(), &mut line);
    for arg in &args {
        line.push(' ');
        quote_arg(arg, &mut line);
    }
    let mut line: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
    let app: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
    // As people write it (`C:\x`, not `\\?\C:\x`): shells and tools take the working folder at face value.
    let dir: Vec<u16> = crate::gate::display(cwd)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let overrides = extra
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .chain(sandbox.into_iter().flat_map(|s| {
            s.env
                .iter()
                .map(|(k, v)| (OsString::from(k), v.as_os_str().to_owned()))
        }))
        .collect();
    let block = environment_block(overrides);
    let token = sandbox.map(|_| low_integrity_token()).transpose()?;

    // SAFETY: plain Win32 calls on handles and buffers owned in this function; every handle is wrapped
    // in an OwnedHandle (closed on every path) or kept in the ProcessTree.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(last());
        }
        let tree = ProcessTree { job };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(last());
        }

        // Pipes: the shell's ends are inheritable, ours are not.
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let pipe = |ours_reads: bool| -> std::io::Result<(OwnedHandle, OwnedHandle)> {
            let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut r, &mut w, &inheritable, 0) == 0 {
                return Err(last());
            }
            let (ours, theirs) = if ours_reads {
                (owned(r), owned(w))
            } else {
                (owned(w), owned(r))
            };
            if SetHandleInformation(ours.as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0) == 0 {
                return Err(last());
            }
            Ok((ours, theirs))
        };
        let (stdin_ours, stdin_theirs) = pipe(false)?;
        let (stdout_ours, stdout_theirs) = pipe(true)?;
        let (stderr_ours, stderr_theirs) = pipe(true)?;

        let inherited: [HANDLE; 3] = [
            stdin_theirs.as_raw_handle() as _,
            stdout_theirs.as_raw_handle() as _,
            stderr_theirs.as_raw_handle() as _,
        ];
        let jobs: [HANDLE; 1] = [job];
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size);
        let mut attrs = vec![0u64; size.div_ceil(8)];
        let list = attrs.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, 2, 0, &mut size) == 0 {
            return Err(last());
        }
        struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
        impl Drop for ListGuard {
            fn drop(&mut self) {
                // SAFETY: initialised above, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.0) }
            }
        }
        let _list_guard = ListGuard(list);
        if UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherited.as_ptr() as *const _,
            std::mem::size_of_val(&inherited),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
            || UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr() as *const _,
                std::mem::size_of_val(&jobs),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
        {
            return Err(last());
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = inherited[0];
        si.StartupInfo.hStdOutput = inherited[1];
        si.StartupInfo.hStdError = inherited[2];
        si.lpAttributeList = list;
        // No hidden window (docs/AV-HYGIENE.md): from the desktop app the shell gets its own,
        // visible console; from the command line it shares the terminal.
        let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let created = match &token {
            Some(token) => CreateProcessAsUserW(
                token.as_raw_handle() as _,
                app.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                block.as_ptr() as *const _,
                dir.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            ),
            None => CreateProcessW(
                app.as_ptr(),
                line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                block.as_ptr() as *const _,
                dir.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            ),
        };
        if created == 0 {
            return Err(last());
        }
        drop(owned(pi.hThread));
        let process = owned(pi.hProcess);
        // The shell holds its own ends now; ours close here, so its exit closes the pipes.
        drop((stdin_theirs, stdout_theirs, stderr_theirs));
        let wait = Box::new(move || {
            // The process handle is owned by this closure.
            let h = process.as_raw_handle() as HANDLE;
            WaitForSingleObject(h, INFINITE);
            let mut code = 0u32;
            (GetExitCodeProcess(h, &mut code) != 0).then_some(code as i32)
        });
        Ok(Spawned {
            stdin: Some(Box::new(std::fs::File::from(stdin_ours))),
            stdout: Box::new(std::fs::File::from(stdout_ours)),
            stderr: Box::new(std::fs::File::from(stderr_ours)),
            wait,
            tree,
        })
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

/// Start the command in its own process group, so a kill reaches everything it started. No
/// confinement here yet: the sandbox is not applied (confine.rs).
#[cfg(unix)]
fn spawn(
    shell: Shell,
    command: &str,
    cwd: &Path,
    _sandbox: Option<&Sandbox>,
) -> std::io::Result<Spawned> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let (program, args, env) = shell.argv(command);
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn()?;
    let pgid = child.id() as i32;
    let stdin = child
        .stdin
        .take()
        .map(|s| Box::new(s) as Box<dyn Write + Send>);
    let stdout = Box::new(child.stdout.take().expect("piped")) as Box<dyn Read + Send>;
    let stderr = Box::new(child.stderr.take().expect("piped")) as Box<dyn Read + Send>;
    Ok(Spawned {
        stdin,
        stdout,
        stderr,
        wait: Box::new(move || child.wait().ok().and_then(|s| s.code())),
        tree: ProcessTree { pgid },
    })
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
