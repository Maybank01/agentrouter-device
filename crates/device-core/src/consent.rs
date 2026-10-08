//! Local confirmation. It is only ever asked on this device, never on the web: a cloud that was
//! impersonated or broken into could fake a web "yes".

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::Ordering;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use crate::presence::Who;

/// What a request wants the person to approve.
#[derive(Debug, Clone)]
pub struct Ask {
    pub session: String,
    /// `exec`, `exec_full` (full access: the first command of a conversation), `write` or `input`.
    pub action: &'static str,
    /// The command, the file (with its size) or the input, in full.
    pub text: String,
    /// Where a command runs, or the job an input goes to.
    pub place: Option<String>,
    /// Deletes or wipes things: shown in red, never "allowed from now on".
    pub destructive: bool,
    /// The kind of command the person may allow for the rest of this conversation (`git status`,
    /// `get-childitem`…); `None` when that is not offered (compound commands, interpreters, deletes).
    pub family: Option<String>,
    /// Who asks: a conversation, a local AI, a remote helper.
    pub who: Who,
}

/// The person's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    /// Allow this one request.
    Once,
    /// Allow this one and, in this conversation, later commands of the same kind (never destructive).
    Similar,
}

impl Decision {
    pub fn allowed(self) -> bool {
        self != Decision::Deny
    }

    pub fn parse(text: &str) -> Decision {
        match text {
            "once" => Decision::Once,
            "similar" => Decision::Similar,
            _ => Decision::Deny,
        }
    }
}

/// Decide an ask. `cancel` is set when nobody waits for the answer any more.
pub type Confirm = Arc<dyn Fn(&Ask, &AtomicBool) -> Decision + Send + Sync>;

/// How long a question stays open before it counts as "no".
pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);

pub const TITLE: &str = "AgentRouter 设备";

pub fn ask_text(ask: &Ask) -> String {
    let what = match ask.action {
        "exec" => "运行命令",
        "exec_full" => "运行命令（完全访问：允许后，这个对话之后的命令不再逐条询问）",
        "write" => "写入文件",
        "input" => "给正在运行的命令输入",
        other => other,
    };
    let place = ask
        .place
        .as_deref()
        .map(|p| format!("\n\n位置：{p}"))
        .unwrap_or_default();
    let warn = if ask.destructive {
        "\n\n注意：这条命令会删除或抹掉东西，删掉的不进回收站。"
    } else {
        ""
    };
    format!(
        "{}想在这台电脑上{what}：\n\n{}{place}{warn}\n\n对话：{}\n\n确定：允许这一次\n取消：拒绝",
        ask.who.title(),
        crate::util::clip(&ask.text, 1500),
        ask.session
    )
}

/// The native message box (Windows; elsewhere nobody can answer, so the answer is no). The desktop
/// app shows its own window instead; the command line keeps this one.
pub fn native() -> Confirm {
    Arc::new(|ask: &Ask, cancel: &AtomicBool| {
        if question(TITLE, &ask_text(ask), cancel) {
            Decision::Once
        } else {
            Decision::Deny
        }
    })
}

/// Unattended machines: the operator approved commands on this machine when linking it (`link
/// --unattended`); every request is still checked against the folders and audited.
pub fn preapproved() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| Decision::Once)
}

/// Never allow (tests, and command-line mode without a person at the screen).
pub fn deny_all() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| Decision::Deny)
}

/// Words that delete or wipe: a command containing one of them anywhere is destructive. Deliberately
/// broad: a false alarm only costs a red dialog and no "allow from now on".
const DESTRUCTIVE: &[&str] = &[
    "rm",
    "rmdir",
    "rd",
    "del",
    "erase",
    "unlink",
    "shred",
    "remove-item",
    "ri",
    "clear-content",
    "clc",
    "clear-item",
    "clear-recyclebin",
    "remove-itemproperty",
    "format",
    "format-volume",
    "clear-disk",
    "initialize-disk",
    "remove-partition",
    "diskpart",
    "mkfs",
    "dd",
    "wipefs",
    "cipher",
    "truncate",
    "vssadmin",
    "wbadmin",
];

/// Sub-commands that can delete, for tools whose other sub-commands do not.
const DESTRUCTIVE_SUB: &[(&str, &[&str])] = &[
    (
        "git",
        &[
            "clean", "reset", "rm", "checkout", "restore", "stash", "push", "branch",
        ],
    ),
    ("docker", &["rm", "rmi", "prune", "system", "volume"]),
    ("kubectl", &["delete"]),
    ("npm", &["uninstall", "rm", "prune"]),
    ("cargo", &["clean"]),
    ("reg", &["delete"]),
];

fn words(command: &str) -> Vec<String> {
    command
        .split(|c: char| c.is_whitespace() || ";|&(){}[]`\"'<>,=".contains(c))
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_ascii_lowercase();
            let base = w.rsplit(['/', '\\']).next().unwrap_or(&w).to_string();
            base.strip_suffix(".exe")
                .map(str::to_string)
                .unwrap_or(base)
        })
        .collect()
}

/// Does the command delete or wipe something? (Any word, anywhere: `x; rm -rf y` counts.)
pub fn destructive(command: &str) -> bool {
    let words = words(command);
    if words.iter().any(|w| DESTRUCTIVE.contains(&w.as_str())) {
        return true;
    }
    words.windows(2).any(|pair| {
        DESTRUCTIVE_SUB
            .iter()
            .any(|(tool, subs)| pair[0] == *tool && subs.contains(&pair[1].as_str()))
    })
}

/// Programs whose arguments are code in their own right: never "the same kind" as an earlier call.
const INTERPRETERS: &[&str] = &[
    "powershell",
    "pwsh",
    "cmd",
    "bash",
    "sh",
    "zsh",
    "wsl",
    "python",
    "python3",
    "py",
    "node",
    "deno",
    "bun",
    "ruby",
    "perl",
    "php",
    "invoke-expression",
    "iex",
    "start-process",
    "saps",
    "start",
    "invoke-command",
    "icm",
    "invoke-item",
    "ii",
    "call",
    "env",
    "sudo",
    "runas",
    "npx",
    "pnpx",
    "bunx",
    "uvx",
    "pipx",
    "msiexec",
    "rundll32",
    "regsvr32",
    "mshta",
    "wscript",
    "cscript",
    "schtasks",
    "sc",
    "set-executionpolicy",
    "curl",
    "wget",
    "iwr",
    "invoke-webrequest",
    "irm",
    "invoke-restmethod",
];

/// Tools whose first argument says what they do (`git status` and `git push` are different kinds).
const WITH_SUBCOMMAND: &[&str] = &[
    "git", "npm", "pnpm", "yarn", "cargo", "dotnet", "docker", "kubectl", "go", "pip", "pip3",
    "winget", "choco", "scoop", "gh", "uv", "make", "gradle", "mvn",
];

/// The kind of a simple command (`git status`, `get-childitem`), for "allow the same kind from now on".
/// Compound commands (`;`, `|`, `&&`, redirections, subexpressions, script blocks), interpreters and
/// destructive commands have no kind: they are always asked.
pub fn family(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(|c| ";|&`$<>{}()\n\r@%".contains(c))
        || destructive(trimmed)
    {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let first = parts.next()?;
    let program = first
        .trim_matches(['"', '\''])
        .rsplit(['/', '\\'])
        .next()?
        .to_ascii_lowercase();
    let program = [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .find_map(|ext| program.strip_suffix(ext))
        .unwrap_or(&program)
        .to_string();
    if program.is_empty()
        || !program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || INTERPRETERS.contains(&program.as_str())
    {
        return None;
    }
    if WITH_SUBCOMMAND.contains(&program.as_str()) {
        let sub = parts
            .filter(|p| !p.starts_with('-'))
            .find(|p| !p.contains(['\\', '/', ':']))?
            .to_ascii_lowercase();
        if !sub.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        return Some(format!("{program} {sub}"));
    }
    Some(program)
}

/// A yes/no question in a native dialog (OK = yes, Cancel = no; Cancel is the default button). It closes
/// as "no" when `cancel` is set or after [`ASK_TIMEOUT`].
#[cfg(windows)]
pub fn question(title: &str, body: &str, cancel: &AtomicBool) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IDOK, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL, MB_SETFOREGROUND,
        MB_TOPMOST, MessageBoxW, PostMessageW, WM_CLOSE,
    };
    // A title nobody else uses, so the dialog can be found and closed.
    let unique = format!("{title} · {}", &crate::util::short_id("")[..6]);
    let (tx, rx) = std::sync::mpsc::channel();
    let (t, b) = (wide(&unique), wide(body));
    std::thread::spawn(move || {
        // SAFETY: the strings are NUL-terminated UTF-16 that outlive the call.
        let answer = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                b.as_ptr(),
                t.as_ptr(),
                MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
            )
        };
        let _ = tx.send(answer == IDOK);
    });
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(answer) => return answer,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(_) => {
                if cancel.load(Ordering::SeqCst) || started.elapsed() > ASK_TIMEOUT {
                    let t = wide(&unique);
                    // SAFETY: closing the dialog we opened, found by its unique title.
                    unsafe {
                        let hwnd = FindWindowW(std::ptr::null(), t.as_ptr());
                        if !hwnd.is_null() {
                            PostMessageW(hwnd, WM_CLOSE, 0, 0);
                        }
                    }
                    return false;
                }
            }
        }
    }
}

#[cfg(not(windows))]
pub fn question(_title: &str, _body: &str, _cancel: &AtomicBool) -> bool {
    false
}

/// An information dialog that does not block the caller.
#[cfg(windows)]
pub fn inform(title: &str, body: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    let (t, b) = (wide(title), wide(body));
    std::thread::spawn(move || {
        // SAFETY: NUL-terminated UTF-16 strings owned by this thread.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                b.as_ptr(),
                t.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
            );
        }
    });
}

#[cfg(not(windows))]
pub fn inform(title: &str, body: &str) {
    eprintln!("{title}\n{body}");
}

#[cfg(windows)]
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_commands_are_recognised() {
        for c in [
            r"Remove-Item D:\x -Recurse",
            "rm -rf build",
            "del /q *.tmp",
            "git clean -fdx",
            "Get-ChildItem *.log | Remove-Item",
            r"C:\Windows\System32\cmd.exe /c rd /s /q x",
            "format D:",
            "reg delete HKCU\\Software\\X /f",
        ] {
            assert!(destructive(c), "{c}");
        }
        for c in [
            "Get-ChildItem",
            "git status",
            "npm run build",
            r"dir D:\work",
            "Move-Item a b",
        ] {
            assert!(!destructive(c), "{c}");
        }
    }

    #[test]
    fn only_simple_commands_have_a_kind() {
        assert_eq!(family("git status").as_deref(), Some("git status"));
        assert_eq!(
            family(r"git -C D:\w log --oneline").as_deref(),
            Some("git log")
        );
        assert_eq!(
            family(r"Get-ChildItem D:\work").as_deref(),
            Some("get-childitem")
        );
        assert_eq!(family(r"C:\tools\rg.exe foo").as_deref(), Some("rg"));
        assert_eq!(family("npm run build").as_deref(), Some("npm run"));
        for c in [
            "git status; rm -rf x",
            "git status && del x",
            "dir | Remove-Item",
            "echo $(whoami)",
            "Get-Content a > b",
            "powershell -Command dir",
            "python -c 'import os'",
            "iex (irm x)",
            "& { dir }",
            "Remove-Item x",
            "git clean -fdx",
            "cmd /c echo %PATH%",
            "",
        ] {
            assert_eq!(family(c), None, "{c}");
        }
    }
}
