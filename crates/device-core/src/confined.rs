//! A low-integrity spawn for `exec` on Windows (S2b): the write boundary is the OS's, not the
//! command text. Port of the retired `jobs.rs`/ #20 code: a restricted copy of this process's token
//! (`DISABLE_MAX_PRIVILEGE`) carrying the Low mandatory label, the exec cwd labelled Low by
//! `confine::prepare_command`, TEMP and tool caches inside the Low scratch folder, and a kill-on-close
//! Job Object. Windows denies that process every write, delete, rename and create on an object
//! labelled higher (unlabelled = Medium), so a command cannot write outside the labelled workspace
//! folder — even from inside interpreters, with relative/junction/8.3 paths, or through a child.
//!
//! Not covered (same list as confine.rs): volumes without NTFS security, network shares, files moved
//! into a folder that keeps their own label, and requests brokered to medium-integrity programs
//! already running (COM servers, the shell's own UI). Reading stays allowed.

use std::ffi::OsString;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess, INFINITE,
    OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, WaitForSingleObject,
};

use crate::exec::Outcome;

const STREAM_CAP: usize = 256 << 10;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// The device's PowerShell, as the old built-in command tool found it.
fn powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: a handle just returned to us, owned from here on.
    unsafe { OwnedHandle::from_raw_handle(handle as _) }
}

/// The restricted, low-integrity token a confined command runs with (confine.rs).
fn low_integrity_token() -> std::io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, GetLengthSid, PSID, SID_AND_ATTRIBUTES,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const SE_GROUP_INTEGRITY: u32 = 0x20;
    // SAFETY: handles are owned as soon as returned; the SID is freed after use.
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
        let own = owned(own);
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
        let low = owned(low);
        let mut sid: PSID = std::ptr::null_mut();
        let level = wide("S-1-16-4096");
        if ConvertStringSidToSidW(level.as_ptr(), &mut sid) == 0 {
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

/// One argument quoted the way `CommandLineToArgvW` and the C runtime read it back.
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

/// The environment block of the confined shell: our environment, then the sandbox overrides, sorted
/// as `NAME=value\0…\0\0`.
fn environment_block(overrides: &[(String, PathBuf)]) -> Vec<u16> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (key, value) in overrides {
        let key = OsString::from(key);
        env.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
        env.push((key, value.as_os_str().to_owned()));
    }
    // A PowerShell 5.1 child of a connector started from `pwsh` 7 must not see PowerShell 7's
    // $PSHOME\Modules first (it would lose Get-FileHash and other Microsoft.PowerShell.Utility cmdlets).
    env.retain(|(k, _)| !k.eq_ignore_ascii_case("PSModulePath"));
    env.push((
        OsString::from("PSModulePath"),
        OsString::from(crate::util::child_ps_module_path()),
    ));
    env.sort_by_key(|(key, _)| key.to_ascii_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (key, value) in &env {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn read_stream(mut stream: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut taken = std::io::Read::by_ref(&mut stream).take(STREAM_CAP as u64);
        let _ = taken.read_to_end(&mut buf);
        buf
    })
}

fn clip(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_string()
}

struct Job(HANDLE);

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: closes our own handle; kill-on-close ends anything left in the tree.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Run `command` with the device's PowerShell at low integrity in `cwd` (labelled Low already), with
/// the sandbox's TEMP/caches, and a timeout that kills the whole job.
pub fn run(
    command: &str,
    cwd: &Path,
    timeout: Duration,
    sandbox: &crate::confine::Sandbox,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let token = low_integrity_token().map_err(|e| format!("建不了低完整性 token：{e}"))?;
    // SAFETY: plain Win32 calls on handles and buffers owned in this function; every handle is wrapped
    // in an OwnedHandle (closed on every path) or closed explicitly.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!(
                "建不了作业对象：{}",
                std::io::Error::last_os_error()
            ));
        }
        let job = Job(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(format!(
                "作业对象设置失败：{}",
                std::io::Error::last_os_error()
            ));
        }

        let inheritable = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        let pipe = |ours_reads: bool| -> std::io::Result<(OwnedHandle, OwnedHandle)> {
            let (mut read, mut write): (HANDLE, HANDLE) =
                (std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut read, &mut write, &inheritable, 0) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let (ours, theirs) = if ours_reads {
                (owned(read), owned(write))
            } else {
                (owned(write), owned(read))
            };
            if SetHandleInformation(ours.as_raw_handle() as _, HANDLE_FLAG_INHERIT, 0) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((ours, theirs))
        };
        let (stdin_ours, stdin_theirs) = pipe(false).map_err(|e| format!("建不了管道：{e}"))?;
        let (stdout_ours, stdout_theirs) = pipe(true).map_err(|e| format!("建不了管道：{e}"))?;
        let (stderr_ours, stderr_theirs) = pipe(true).map_err(|e| format!("建不了管道：{e}"))?;

        let program = powershell();
        let mut line = String::new();
        quote_arg(&program.to_string_lossy(), &mut line);
        for arg in [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ] {
            line.push(' ');
            quote_arg(arg, &mut line);
        }
        let app = wide(&program.to_string_lossy());
        let mut line = wide(&line);
        let dir = wide(&crate::gate::display(cwd));
        let block = environment_block(&sandbox.env);
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = stdin_theirs.as_raw_handle() as _;
        si.hStdOutput = stdout_theirs.as_raw_handle() as _;
        si.hStdError = stderr_theirs.as_raw_handle() as _;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessAsUserW(
            token.as_raw_handle() as _,
            app.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT,
            block.as_ptr() as *const _,
            dir.as_ptr(),
            &si,
            &mut pi,
        );
        if created == 0 {
            return Err(format!(
                "低完整性命令起不来：{}",
                std::io::Error::last_os_error()
            ));
        }
        drop(owned(pi.hThread));
        let process = owned(pi.hProcess);
        // The shell owns its pipe ends now; ours close so its exit ends the streams.
        drop((stdin_theirs, stdout_theirs, stderr_theirs));
        let _ = stdin_ours;
        let stdout = std::fs::File::from(stdout_ours);
        let stderr = std::fs::File::from(stderr_ours);
        if AssignProcessToJobObject(job.0, process.as_raw_handle() as _) == 0 {
            let err = std::io::Error::last_os_error();
            let _ = TerminateJobObject(job.0, 1);
            return Err(format!("命令进不了作业对象：{err}"));
        }
        let out_reader = read_stream(stdout);
        let err_reader = read_stream(stderr);
        let wait_ms = timeout.as_millis().min(INFINITE as u128 - 1) as u32;
        let mut timed_out = false;
        let waited = WaitForSingleObject(process.as_raw_handle() as _, wait_ms);
        if waited == WAIT_TIMEOUT {
            timed_out = true;
            let _ = TerminateJobObject(job.0, 1);
            WaitForSingleObject(process.as_raw_handle() as _, INFINITE);
        } else if waited != WAIT_OBJECT_0 {
            let err = std::io::Error::last_os_error();
            let _ = TerminateJobObject(job.0, 1);
            return Err(format!("等低完整性命令出错：{err}"));
        }
        // Nothing the command left behind may keep the pipes open.
        let _ = TerminateJobObject(job.0, 1);
        let mut code = 0u32;
        let exit_code = (GetExitCodeProcess(process.as_raw_handle() as _, &mut code) != 0)
            .then_some(code as i32);
        drop(job);
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        Ok(Outcome {
            stdout: clip(&stdout),
            stderr: clip(&stderr),
            exit_code: if timed_out { None } else { exit_code },
            timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}
