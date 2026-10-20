//! One-shot command execution for the workspace MCP service (S1): the person's shell, a cwd, a
//! timeout, capped output, and a Windows Job Object so a timeout kills the whole process tree.
//!
//! This is the "ssh-like" exec half of the workspace MCP service: the command runs with the shell of
//! the device (PowerShell on Windows, `/bin/sh` elsewhere) and the caller has already validated the
//! cwd against the workspace (project/worktree only).
//!
//! S1 does not bring back the low-integrity token of the retired built-in command tool; the write
//! boundary today is the workspace cwd + the gate + the denial/consent policy. The low-integrity
//! boundary (confine.rs + the retired jobs.rs token code) is a recorded follow-up.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Output kept from each stream; the rest is dropped with a marker (an Agent does not need a 500 MB
/// log through the link).
const STREAM_CAP: usize = 256 << 10;
pub const TIMEOUT_DEFAULT_MS: u64 = 60_000;
pub const TIMEOUT_MAX_MS: u64 = 10 * 60_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the command did not end by itself (timeout, cancel) or ended by a signal.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// Ended because the caller cancelled it.
    pub cancelled: bool,
    /// The signal that ended it (Unix), when it was not ours.
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Danger {
    Safe,
    /// Not permitted at all (catastrophic: formatting a disk, disabling the antivirus…).
    Deny(&'static str),
    /// Runs only after the person confirms it on this computer (or at the `full` level).
    Ask(&'static str),
}

/// The deny list is deliberately short and hard: things no Agent workflow needs and that can destroy
/// the machine or its protection. Everything else destructive is an Ask, never a silent run.
pub fn classify(command: &str) -> Danger {
    let text = command.to_ascii_lowercase();
    let has = |needle: &str| text.contains(needle);
    const DENY: &[(&str, &str)] = &[
        ("format ", "格式化磁盘"),
        ("format c:", "格式化磁盘"),
        ("diskpart", "磁盘分区工具"),
        ("bcdedit", "修改启动配置"),
        ("vssadmin delete", "删除卷影副本"),
        ("cipher /w", "擦除磁盘空闲空间"),
        ("initialize-disk", "初始化磁盘"),
        ("clear-disk", "清空磁盘"),
        ("disablerealtimemonitoring", "关闭杀毒实时保护"),
        ("set-mppreference -disable", "关闭 Defender"),
        ("remove-item -recurse -force c:\\", "递归删除系统盘"),
        ("rm -rf /", "递归删除根目录"),
        ("del /f /s /q c:\\", "递归删除系统盘"),
    ];
    for (needle, why) in DENY {
        if has(needle) {
            return Danger::Deny(why);
        }
    }
    const ASK: &[(&str, &str)] = &[
        ("remove-item", "删除文件"),
        ("rm -rf", "递归删除"),
        ("rm -fr", "递归删除"),
        ("del /s", "递归删除"),
        ("rd /s", "递归删除"),
        ("rmdir /s", "递归删除"),
        ("git push --force", "强推远端分支"),
        ("git reset --hard", "丢弃未提交改动"),
        ("git clean -", "删除未跟踪文件"),
        ("reg add", "修改注册表"),
        ("reg delete", "修改注册表"),
        ("sc create", "创建系统服务"),
        ("sc delete", "删除系统服务"),
        ("sc config", "修改系统服务"),
        ("net user", "修改本机账号"),
        ("new-localuser", "新建本机账号"),
        ("set-executionpolicy", "修改 PowerShell 策略"),
        ("winget install", "安装软件"),
        ("choco install", "安装软件"),
        ("npm install -g", "全局安装 npm 包"),
        ("shutdown", "关机/重启"),
        ("stop-computer", "关机/重启"),
        ("restart-computer", "关机/重启"),
    ];
    for (needle, why) in ASK {
        if has(needle) {
            return Danger::Ask(why);
        }
    }
    Danger::Safe
}

/// The kept head of a stream (`total` bytes were written), with a marker when some was dropped.
fn clip_stream(kept: &[u8], total: usize) -> String {
    let text = String::from_utf8_lossy(kept);
    if total <= kept.len() {
        return text.trim_end().to_string();
    }
    format!("{text}\n…（输出被截断，共 {total} 字节）")
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

// SAFETY: a job object handle is a kernel handle usable from any thread; only Drop closes it, once.
#[cfg(windows)]
unsafe impl Send for Job {}

#[cfg(windows)]
impl Job {
    fn new() -> std::io::Result<Job> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: plain Win32 calls on a job handle we own from here on.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Job(handle);
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    fn assign(&self, child: &std::process::Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        // SAFETY: both handles are valid for the duration of the call; the job outlives the process.
        let ok = unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: closes the handle once; closing a kill-on-close job ends its process tree.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// One of a command's two output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// What a streamed run reports while it runs: a chunk of output as it arrives, or a tick (at least every 20 ms) so
/// the caller can flush what it holds.
pub enum Event<'a> {
    Output(Stream, &'a [u8]),
    Tick,
}

/// A shell command started in its own process group (Unix) or kill-on-close job object (Windows), so the whole tree
/// it starts can be ended at once; dropping it ends the tree.
pub struct Spawned {
    pub child: std::process::Child,
    #[cfg(windows)]
    job: Option<Job>,
}

impl Spawned {
    /// End the command and everything it started (the shell may have exited already).
    pub fn kill_tree(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: the pid leads the process group we created for this command; ESRCH when it is gone.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        {
            // Closing a kill-on-close job ends its whole tree.
            self.job.take();
        }
        let _ = self.child.kill();
    }

    /// Send the whole tree a polite stop first (Unix SIGTERM; Windows has none for a console tree, so it ends it).
    pub fn terminate_tree(&mut self) {
        #[cfg(unix)]
        // SAFETY: as in kill_tree.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        #[cfg(windows)]
        self.kill_tree();
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill_tree();
            let _ = self.child.wait();
        }
    }
}

/// The device's shell running `command` in `cwd` (PowerShell on Windows, `/bin/sh -lc` elsewhere): stdout and
/// stderr piped, stdin as given. The caller validates `cwd` first.
pub fn spawn_shell(command: &str, cwd: &Path, stdin: Stdio) -> Result<Spawned, String> {
    #[cfg(windows)]
    let shell = crate::shell::default_shell();
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new(&shell.program);
        c.args(shell.args(command));
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("/bin/sh");
        c.arg("-lc").arg(command);
        c
    };
    cmd.current_dir(cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A connector started from PowerShell 7 inherits its PSModulePath; a *Windows PowerShell 5.1*
    // child must not resolve Microsoft.PowerShell.Utility from PowerShell 7 (Get-FileHash missing).
    // When `pwsh` itself is the selected shell, its environment is left alone for module discovery.
    #[cfg(windows)]
    if !shell.is_pwsh() {
        cmd.env("PSModulePath", crate::util::child_ps_module_path());
    }
    // Its own process group on Unix, so a timeout ends the shell *and* what it started (the shell may
    // exit but leave a child holding the pipes open).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the child after fork; it only sets the child's process group and,
        // on Linux, turns off setuid/setgid escalation (`PR_SET_NO_NEW_PRIVS`; no filesystem boundary
        // without a sandbox helper).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    libc::prctl(38, 1, 0, 0, 0);
                }
                Ok(())
            });
        }
    }
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("命令起不来（{}）：{e}", cwd.display()))?;
    #[cfg(windows)]
    let job = match Job::new() {
        Ok(job) => match job.assign(&child) {
            Ok(()) => Some(job),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("命令进不了受限作业对象：{e}"));
            }
        },
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("建不了作业对象：{e}"));
        }
    };
    Ok(Spawned {
        child,
        #[cfg(windows)]
        job,
    })
}

/// Read `stream` in chunks into `tx` until it ends.
fn pump(
    mut stream: impl Read + Send + 'static,
    which: Stream,
    tx: std::sync::mpsc::Sender<(Stream, Vec<u8>)>,
) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((which, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Unconfined run (access level `full`: the person explicitly asked for unrestricted commands).
pub fn run(command: &str, cwd: &Path, timeout: Duration) -> Result<Outcome, String> {
    run_streaming(command, cwd, timeout, &|| false, &mut |_| {})
}

/// Confined run for the workspace MCP service (access level `folders`, S2b): the exec cwd is labelled
/// Low by confine.rs and, on Windows, the command runs in a low-integrity restricted token with a Low
/// TEMP/caches. On Unix there is no portable OS write boundary yet (documented in README): the gate,
/// deny/consent, audit and process group are what apply.
pub fn run_confined(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    data_dir: &Path,
) -> Result<Outcome, String> {
    let confinement = crate::confine::Confinement::new(data_dir);
    let sandbox = confinement
        .prepare_command(cwd)
        .map_err(|e| format!("{}: {}", e.code, e.message))?;
    #[cfg(windows)]
    if let Some(sandbox) = sandbox.as_ref() {
        return crate::confined::run(
            command,
            cwd,
            timeout,
            sandbox,
            &crate::shell::default_shell(),
        );
    }
    #[cfg(not(windows))]
    let _ = sandbox;
    run(command, cwd, timeout)
}

/// Run `command` with the device's shell in `cwd`, handing every chunk of output to `on_event` as it arrives (and a
/// tick at least every 20 ms). `timeout` and `cancel` end the whole process tree; whatever the command left running
/// when the shell exits is ended too. The output kept in the [`Outcome`] is capped per stream.
pub fn run_streaming(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &dyn Fn() -> bool,
    on_event: &mut dyn FnMut(Event<'_>),
) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut spawned = spawn_shell(command, cwd, Stdio::null())?;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(out) = spawned.child.stdout.take() {
        pump(out, Stream::Stdout, tx.clone());
    }
    if let Some(err) = spawned.child.stderr.take() {
        pump(err, Stream::Stderr, tx.clone());
    }
    drop(tx);
    let deadline = started + timeout;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (mut stdout_total, mut stderr_total) = (0usize, 0usize);
    let mut status: Option<std::process::ExitStatus> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut streams_open = true;
    // After the shell ends, what it left behind is ended and the pipes get a short time to drain (a daemon that
    // escaped the group must not hold the answer forever).
    let mut drain_until: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok((which, bytes)) => {
                let (keep, total) = match which {
                    Stream::Stdout => (&mut stdout, &mut stdout_total),
                    Stream::Stderr => (&mut stderr, &mut stderr_total),
                };
                *total += bytes.len();
                let room = STREAM_CAP.saturating_sub(keep.len());
                keep.extend_from_slice(&bytes[..bytes.len().min(room)]);
                on_event(Event::Output(which, &bytes));
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => streams_open = false,
        }
        on_event(Event::Tick);
        if status.is_none() {
            match spawned.child.try_wait() {
                Ok(Some(s)) => status = Some(s),
                Ok(None) => {}
                Err(e) => {
                    spawned.kill_tree();
                    let _ = spawned.child.wait();
                    return Err(format!("等命令结果出错：{e}"));
                }
            }
        }
        if status.is_some() {
            if drain_until.is_none() {
                spawned.kill_tree();
                drain_until = Some(Instant::now() + Duration::from_secs(2));
            }
            if !streams_open || drain_until.is_some_and(|t| Instant::now() >= t) {
                break;
            }
            continue;
        }
        if Instant::now() >= deadline {
            timed_out = true;
        } else if cancel() {
            cancelled = true;
        } else {
            continue;
        }
        spawned.kill_tree();
        status = spawned.child.wait().ok();
        drain_until = Some(Instant::now() + Duration::from_secs(2));
    }
    let ended = timed_out || cancelled;
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|s| s.signal()).filter(|_| !ended)
    };
    #[cfg(not(unix))]
    let signal = None;
    Ok(Outcome {
        stdout: clip_stream(&stdout, stdout_total),
        stderr: clip_stream(&stderr, stderr_total),
        exit_code: if ended {
            None
        } else {
            status.and_then(|s| s.code())
        },
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        cancelled,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_is_short_and_hard() {
        assert_eq!(classify("git status"), Danger::Safe);
        assert!(matches!(classify("format C:"), Danger::Deny(_)));
        assert!(matches!(classify("Remove-Item -Recurse x"), Danger::Ask(_)));
        assert!(matches!(
            classify("git push --force origin main"),
            Danger::Ask(_)
        ));
    }

    #[test]
    fn runs_and_reports_exit_code() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ok = run("echo hello", cwd, Duration::from_secs(20)).unwrap();
        assert!(ok.stdout.contains("hello"), "{ok:?}");
        assert_eq!(ok.exit_code, Some(0));
        assert!(!ok.timed_out);
        let failed = run("exit 3", cwd, Duration::from_secs(20)).unwrap();
        assert_eq!(failed.exit_code, Some(3));
    }

    #[test]
    fn a_timeout_ends_the_command() {
        let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 5"
        } else {
            "sleep 5"
        };
        let started = Instant::now();
        let out = run(command, cwd, Duration::from_millis(300)).unwrap();
        assert!(out.timed_out, "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
