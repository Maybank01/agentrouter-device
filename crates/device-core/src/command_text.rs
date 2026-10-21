//! How a command's text reaches the shell: **never as an argument** (workspace AGENTS.md "No data through command
//! lines"; Cloud #2413, device #98 for Unix exec, #99 for Windows and terminals). An argument is visible to every
//! process on the machine, is cut at 128 KiB on Linux (`MAX_ARG_STRLEN`) and at 32 767 characters on Windows
//! (`CreateProcess`), and an Agent's heredoc that writes a file is one command.
//!
//! - **One-shot runs** (exec, background jobs, the confined run): the text goes to the shell on **stdin**. The shell
//!   starts a fixed runner that reads all of stdin, then runs it; a program the command starts reads an empty stdin.
//! - **Terminals** (`pty_open`): stdin is the keyboard there, so the text goes into a file only this account can
//!   open (owner-only DACL on Windows, mode 0600 elsewhere) in `<temp>/agentrouter-run/`; the shell reads and deletes
//!   it first thing, and the [`TextFile`] removes it on drop if the shell never started.
//!
//! PowerShell runs the text as one script in the session's scope and exits as `-Command <text>` would: `exit N`
//! exits N, otherwise 0 when the last statement succeeded and 1 when it did not. No encoded command, no
//! execution-policy change, no script file run with `-File` (docs/AV-HYGIENE.md): the runner is plain visible text.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The Unix one-shot runner (`/bin/sh -lc`): the command text read from stdin, then the command itself with stdin
/// `/dev/null`. No file: a sandboxed host server may have no writable temp folder.
pub const UNIX_STDIN_RUNNER: &str = r#"c=$(cat) && exec </dev/null && eval "$c""#;

/// The Unix terminal runner (`/bin/sh -lc <runner> sh <file>`): the text read from the file, the file removed, then
/// the command on the terminal.
pub const UNIX_FILE_RUNNER: &str = r#"c=$(cat -- "$1") && rm -f -- "$1" && eval "$c""#;

/// The PowerShell runner's second half, after `$__ar_t` holds the text: parse it alone first (a syntax error is
/// reported against the person's lines and exits 1, as `-Command` does), then run it dot-sourced with its last
/// statement's status kept, and exit 0/1 by that status (`exit N` inside exits N; `return` exits 0).
macro_rules! ps_run {
    () => {
        concat!(
            "$null = [scriptblock]::Create($__ar_t); if (-not $?) { exit 1 }; ",
            "$__ar_sb = [scriptblock]::Create($__ar_t + \"`n`n`$__ar_ok = `$?\"); if (-not $?) { exit 1 }; ",
            "$__ar_ok = $true; . $__ar_sb; exit [int](-not $__ar_ok)"
        )
    };
}

/// The PowerShell one-shot runner: stdin read whole as UTF-8 (whatever the console code page), then run.
pub const PS_STDIN_RUNNER: &str = concat!(
    "$__ar_t = [IO.StreamReader]::new([Console]::OpenStandardInput(), [Text.UTF8Encoding]::new($false)).ReadToEnd(); ",
    ps_run!()
);

/// The PowerShell terminal runner for the text in `file` (UTF-8): read, delete, run.
pub fn ps_file_runner(file: &Path) -> String {
    let quoted = file.to_string_lossy().replace('\'', "''");
    format!(
        "$__ar_f = '{quoted}'; $__ar_t = [IO.File]::ReadAllText($__ar_f, [Text.UTF8Encoding]::new($false)); \
         Remove-Item -LiteralPath $__ar_f -Force; {}",
        ps_run!()
    )
}

/// The command's text in a file only this account can open, removed when dropped (if the shell has not already).
#[derive(Debug)]
pub struct TextFile(PathBuf);

impl TextFile {
    /// Write `text` to a new file in `dir` (created if missing).
    pub fn new(dir: &Path, text: &str) -> Result<TextFile, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("建不了 {}：{e}", dir.display()))?;
        let nonce: [u8; 12] = crate::util::random_bytes();
        let name: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("cmd-{name}.txt"));
        let mut file =
            owner_only(&path).map_err(|e| format!("写不了命令文件 {}：{e}", path.display()))?;
        let file_guard = TextFile(path);
        file.write_all(text.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|e| format!("写不了命令文件 {}：{e}", file_guard.0.display()))?;
        Ok(file_guard)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TextFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Where terminal command files go: `<temp>/agentrouter-run/` (`TMPDIR`, `TEMP`, `TMP`; in folder mode that is the
/// link's scratch folder), never inside the person's folders.
pub fn run_dir() -> PathBuf {
    ["TMPDIR", "TEMP", "TMP"]
        .iter()
        .find_map(std::env::var_os)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                r"C:\Windows\Temp"
            } else {
                "/tmp"
            })
        })
        .join("agentrouter-run")
}

/// A new file (it must not exist) that only this account can open.
#[cfg(unix)]
fn owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// A new file (it must not exist) whose DACL is protected and names only this process's user (full control): no
/// inherited entries, no Administrators, no SYSTEM.
#[cfg(windows)]
fn owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: plain Win32 calls on buffers owned here; the token handle, the SID string and the descriptor are freed
    // on every path; the file handle is owned by the returned File.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as _, len, &mut len);
        let err = std::io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_text: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut n = 0;
        while *sid_text.add(n) != 0 {
            n += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, n));
        LocalFree(sid_text as _);
        let sddl: Vec<u16> = format!("D:P(A;;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let handle = CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ,
            &attrs,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        let err = std::io::Error::last_os_error();
        LocalFree(sd as _);
        if handle == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        Ok(std::fs::File::from_raw_handle(handle as _))
    }
}

/// How one command starts: the shell, its arguments (fixed text only) and where the command's text is.
#[derive(Debug)]
pub struct ShellStart {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Write this to the shell's stdin, then close it (one-shot runs).
    pub stdin: Option<String>,
    /// The file holding the text (terminals); keep it until the shell has started.
    pub file: Option<TextFile>,
    /// Windows PowerShell 5.1: the caller sets `PSModulePath` to [`crate::util::child_ps_module_path`] (#32).
    pub windows_powershell: bool,
}

/// A one-shot run of `command`: the device's shell (PowerShell on Windows, `/bin/sh -l` elsewhere) with the text on
/// stdin.
pub fn one_shot(command: &str) -> ShellStart {
    #[cfg(windows)]
    {
        let shell = crate::shell::default_shell();
        ShellStart {
            args: shell.stdin_args(),
            windows_powershell: !shell.is_pwsh(),
            program: shell.program,
            stdin: Some(command.to_owned()),
            file: None,
        }
    }
    #[cfg(not(windows))]
    ShellStart {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-lc".into(), UNIX_STDIN_RUNNER.into()],
        stdin: Some(command.to_owned()),
        file: None,
        windows_powershell: false,
    }
}

/// `command` on a terminal: the device's shell reads it from an owner-only file in `dir` ([`run_dir`] in hostd).
pub fn in_terminal(command: &str, dir: &Path) -> Result<ShellStart, String> {
    let file = TextFile::new(dir, command)?;
    #[cfg(windows)]
    {
        let shell = crate::shell::default_shell();
        Ok(ShellStart {
            args: shell.file_args(file.path()),
            windows_powershell: !shell.is_pwsh(),
            program: shell.program,
            stdin: None,
            file: Some(file),
        })
    }
    #[cfg(not(windows))]
    Ok(ShellStart {
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-lc".into(),
            UNIX_FILE_RUNNER.into(),
            "sh".into(),
            file.path().to_string_lossy().into_owned(),
        ],
        stdin: None,
        file: Some(file),
        windows_powershell: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runners_carry_no_command_text() {
        for runner in [UNIX_STDIN_RUNNER, UNIX_FILE_RUNNER, PS_STDIN_RUNNER] {
            assert!(!runner.contains("{}"), "{runner}");
        }
        let start = one_shot("echo secret-text");
        assert!(
            start.args.iter().all(|a| !a.contains("secret-text")),
            "{start:?}"
        );
        assert_eq!(start.stdin.as_deref(), Some("echo secret-text"));
    }

    #[test]
    fn a_quote_in_the_file_path_stays_inside_the_powershell_string() {
        let runner = ps_file_runner(Path::new(
            r"C:\Users\o'brien\AppData\Local\Temp\agentrouter-run\cmd-1.txt",
        ));
        assert!(
            runner.starts_with(r"$__ar_f = 'C:\Users\o''brien\AppData"),
            "{runner}"
        );
    }

    /// Both Windows shells, on stdin: past CreateProcess's 32 767-character limit, quotes, `$`, newlines and non-ASCII
    /// text arrive intact, the command reads an empty stdin, and the exit code is what `-Command <text>` gives.
    #[cfg(windows)]
    #[test]
    fn both_windows_shells_run_a_long_command_from_stdin_with_command_exit_codes() {
        use std::process::{Command, Stdio};
        let shells = [
            crate::shell::find_pwsh().map(|p| crate::shell::pick_shell(Some(p), None, None)),
            crate::shell::find_windows_powershell()
                .map(|p| crate::shell::pick_shell(Some(p), None, None)),
        ];
        let run = |shell: &crate::shell::Shell, text: &str| {
            let mut cmd = Command::new(&shell.program);
            cmd.args(shell.stdin_args())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if !shell.is_pwsh() {
                cmd.env("PSModulePath", crate::util::child_ps_module_path());
            }
            let mut child = cmd.spawn().unwrap();
            let mut input = child.stdin.take().unwrap();
            let text = text.to_owned();
            let writer = std::thread::spawn(move || input.write_all(text.as_bytes()));
            let out = child.wait_with_output().unwrap();
            let _ = writer.join();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };
        let body = "x".repeat(40_000);
        let long = format!(
            "$s = '{body}'\n$q = 'it''s \"quoted\" $notavar'\nWrite-Output (\"len=\" + $s.Length)\nWrite-Output (\"q=\" + $q)\n\
             Write-Output (\"u=\" + '中文é'.Length)\nWrite-Output (\"in=\" + [Console]::In.ReadToEnd().Length)\n\
             if ($true) {{\n  exit 4\n}}"
        );
        for shell in shells.iter().flatten() {
            let (code, stdout, stderr) = run(shell, &long);
            assert_eq!(code, Some(4), "{shell:?}: {stdout} {stderr}");
            for want in ["len=40000", "q=it's \"quoted\" $notavar", "u=3", "in=0"] {
                assert!(
                    stdout.contains(want),
                    "{shell:?} lacks {want}: {stdout} {stderr}"
                );
            }
            // The exit code is the one `-Command <text>` gave (the old way, short texts only).
            for text in [
                "Write-Output ok",
                "Get-Item C:\\does-not-exist-agentrouter",
                "cmd /c exit 5",
                "cmd /c exit 5; Write-Output after",
                "Write-Error boom",
                "return",
                "exit 7",
                "syntax error (((",
            ] {
                let mut direct = Command::new(&shell.program);
                direct
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", text])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if !shell.is_pwsh() {
                    direct.env("PSModulePath", crate::util::child_ps_module_path());
                }
                let want = direct.status().unwrap().code();
                assert_eq!(run(shell, text).0, want, "{shell:?}: {text}");
            }
        }
    }

    #[test]
    fn a_terminal_command_file_is_private_and_removed_on_drop() {
        // Under the build's own target folder (docs/AV-HYGIENE.md): the unit-test binary is `<target>/debug/deps/…`.
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap();
        let dir = target
            .join("tmp")
            .join(format!("agentrouter-run-test-{}", std::process::id()));
        let text = "echo é ü 中文\n".repeat(4000);
        let start = in_terminal(&text, &dir).unwrap();
        let path = start.file.as_ref().unwrap().path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(start.args.iter().all(|a| !a.contains("中文")), "{start:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(start);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
