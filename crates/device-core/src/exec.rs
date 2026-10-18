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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
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

fn clip_stream(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if bytes.len() <= STREAM_CAP {
        return text.trim_end().to_string();
    }
    let head = String::from_utf8_lossy(&bytes[..STREAM_CAP]);
    format!("{head}\n…（输出被截断，共 {} 字节）", bytes.len())
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

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

fn read_stream(mut stream: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut taken = std::io::Read::by_ref(&mut stream).take(STREAM_CAP as u64);
        let _ = taken.read_to_end(&mut buf);
        buf
    })
}

/// Run `command` with the device's shell in `cwd`. `timeout` kills the process tree (Windows job) or
/// the child (elsewhere); the output is capped. The caller validates `cwd` first.
pub fn run(command: &str, cwd: &Path, timeout: Duration) -> Result<Outcome, String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(command);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.arg("-lc").arg(command);
        c
    };
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
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
    let out_reader = child.stdout.take().map(read_stream);
    let err_reader = child.stderr.take().map(read_stream);
    let deadline = started + timeout;
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("等命令结果出错：{e}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(windows)]
    drop(job);
    let stdout_bytes = out_reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let stderr_bytes = err_reader.and_then(|r| r.join().ok()).unwrap_or_default();
    Ok(Outcome {
        stdout: clip_stream(&stdout_bytes),
        stderr: clip_stream(&stderr_bytes),
        exit_code,
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
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
