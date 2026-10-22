//! The person's terminal environment for the commands `hostd` runs (issue #95, Cloud #2383: a cloud Agent bound to
//! this computer works like a local Agent in the person's own terminal).
//!
//! `hostd` is started from a service manager, a desktop launcher or another program, so its own environment lacks
//! what the person's shell start-up files add: `PATH` entries (nvm, mise, pyenv, cargo, Homebrew), exported variables,
//! `SSH_AUTH_SOCK`, `GIT_*`. The same approach as VS Code's "resolve shell environment" (and Zed's): run the person's
//! login shell once as an interactive login shell (`$SHELL -i -l -c …`), print its environment between markers, and
//! start every command (`host__exec`, jobs, PTYs) with it. Nothing is written to disk and nothing is logged: the values
//! stay in this process.
//!
//! - **Once per link, again when it changes:** the capture is kept and redone when a start-up file of the person's
//!   shell changes (its modification time), so a person who edits `~/.zshrc` gets it on the next command without
//!   relinking.
//! - **Bounded:** the capture runs with stdin `/dev/null`, its own process group and a 10 s limit (a start-up file
//!   that waits for input or starts tmux is killed); `AGENTROUTER_RESOLVING_ENVIRONMENT=1` lets a start-up file skip
//!   slow parts. When the person's shell fails, `/bin/sh`'s login environment is captured the same way (`/etc/profile`,
//!   `~/.profile`); when that fails too, the commands keep `hostd`'s own environment.
//! - **The only place start-up files run:** commands start `/bin/sh -c`, not a login shell, with the captured
//!   environment ([`crate::command_text::UNIX_STDIN_RUNNER`]). What a start-up file prints or fails at stays here (its
//!   stderr is dropped) and never lands in a command's output (Cloud #2246: Ubuntu's `im-config` logging through
//!   `systemd-cat` inside the sandbox put `Failed to create stream fd: Operation not permitted` ahead of every
//!   command's own error).
//! - **What `hostd` sets wins:** the egress proxy of a sandbox (folder mode's `srt`, a cloud sandbox's node proxy),
//!   `TMPDIR`, and `AGENTROUTER_*` keep `hostd`'s values; variables only `hostd` has are kept. In folder mode the
//!   capture runs inside the same OS sandbox as the commands, so it only reads what the sandbox lets it read.
//! - **Windows:** a process started by the person already has their user and system variables from the registry; the
//!   capture is Unix only.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::time::{Duration, SystemTime};

/// The environment the person's commands start with: `hostd`'s own, overlaid with the captured one.
pub type Env = BTreeMap<OsString, OsString>;

const MARK: &str = "__AGENTROUTER_ENV_8f3a1c__";
#[cfg(unix)]
const LIMIT: Duration = Duration::from_secs(10);

/// Variables that describe the capturing shell itself, not the person's environment.
const SHELL_OWN: &[&str] = &[
    "_",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "AGENTROUTER_RESOLVING_ENVIRONMENT",
];

/// Variables `hostd`'s placement sets and a start-up file must not replace (the sandbox's way out, its temp folder).
fn host_owned(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    let upper = key.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY" | "FTP_PROXY" | "TMPDIR"
    ) || upper.starts_with("AGENTROUTER_")
        || upper.starts_with("SRT_")
}

/// `hostd`'s environment overlaid with `captured` (see the module docs for which side wins).
pub fn merge(
    own: impl IntoIterator<Item = (OsString, OsString)>,
    captured: &[(OsString, OsString)],
) -> Env {
    let mut env: Env = own.into_iter().collect();
    for (key, value) in captured {
        if key.is_empty() || SHELL_OWN.iter().any(|s| OsStr::new(s) == key) {
            continue;
        }
        if host_owned(key) && env.contains_key(key) {
            continue;
        }
        env.insert(key.clone(), value.clone());
    }
    env
}

/// The `KEY=value` entries printed by `env -0` after the marker line; `None` when the marker is missing (the shell
/// failed before it, or a start-up file ended it).
pub fn parse(output: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let mark = format!("{MARK}\n");
    let start = find(output, mark.as_bytes())? + mark.len();
    let body = &output[start..];
    let end = find(body, format!("\0{MARK}").as_bytes())
        .map(|i| i + 1)
        .or_else(|| find(body, MARK.as_bytes()))?;
    let mut vars = Vec::new();
    for entry in body[..end].split(|b| *b == 0) {
        let Some(eq) = entry.iter().position(|b| *b == b'=') else {
            continue;
        };
        if eq == 0 {
            continue;
        }
        vars.push((os(&entry[..eq]), os(&entry[eq + 1..])));
    }
    (!vars.is_empty()).then_some(vars)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(unix)]
fn os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The person's login shell: `$SHELL` when it is an existing absolute path, else the account's shell, else `/bin/sh`.
#[cfg(unix)]
pub fn person_shell() -> PathBuf {
    let from_env = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_file());
    from_env
        .or_else(account_shell)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[cfg(unix)]
fn account_shell() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getpwuid returns a pointer into static storage (or null); it is read at once, before any other call.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            return None;
        }
        let bytes = std::ffi::CStr::from_ptr((*pw).pw_shell).to_bytes();
        let path = PathBuf::from(OsStr::from_bytes(bytes));
        (path.is_absolute() && path.is_file()).then_some(path)
    }
}

/// The start-up files a login shell of this kind reads; their modification times decide when to capture again.
#[cfg(unix)]
fn startup_files(shell: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let name = shell.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let mut files = vec![
        PathBuf::from("/etc/profile"),
        PathBuf::from("/etc/environment"),
    ];
    let Some(home) = home else { return files };
    let own: &[&str] = match name {
        "zsh" => &[".zshenv", ".zprofile", ".zshrc", ".zlogin"],
        "bash" => &[".bash_profile", ".bash_login", ".profile", ".bashrc"],
        "fish" => &[".config/fish/config.fish"],
        _ => &[".profile"],
    };
    let zdot = (name == "zsh")
        .then(|| std::env::var_os("ZDOTDIR").map(PathBuf::from))
        .flatten();
    files.extend(own.iter().map(|f| zdot.as_deref().unwrap_or(home).join(f)));
    if name == "zsh" {
        files.push(PathBuf::from("/etc/zshenv"));
        files.push(PathBuf::from("/etc/zsh/zshenv"));
    }
    files
}

#[cfg(unix)]
fn stamp(files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files
        .iter()
        .map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
        .collect()
}

/// Runs the person's shell as an interactive login shell and returns its environment; `None` on any failure.
#[cfg(unix)]
pub fn capture(shell: &Path) -> Option<Vec<(OsString, OsString)>> {
    capture_in(shell, None, LIMIT)
}

/// [`capture`] with another `HOME` for the shell (tests) and a time limit.
#[cfg(unix)]
fn capture_in(
    shell: &Path,
    home: Option<&Path>,
    limit: Duration,
) -> Option<Vec<(OsString, OsString)>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let name = shell.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // `env -0` is in GNU coreutils, busybox and the BSDs (macOS included); `command` skips a function named env.
    let script = if name == "fish" {
        format!("printf '\\n%s\\n' {MARK}; command env -0; printf '%s' {MARK}")
    } else {
        format!("printf '\\n%s\\n' '{MARK}'; command env -0; printf '%s' '{MARK}'")
    };
    let mut cmd = Command::new(shell);
    cmd.args(["-i", "-l", "-c", &script])
        .env("AGENTROUTER_RESOLVING_ENVIRONMENT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    // SAFETY: only setsid in the child: no controlling terminal (an interactive shell then never stops on job control)
    // and one process group to end on the limit.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let pid = child.id() as i32;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.by_ref().take(16 << 20).read_to_end(&mut out);
        out
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // SAFETY: a signal to the capture's own process group (setsid above made it the leader).
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = child.wait();
                break;
            }
        }
    }
    // A start-up file may leave a background child holding stdout; the group is ended so the reader finishes.
    // SAFETY: as above; the group may already be gone.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    parse(&reader.join().ok()?)
}

#[cfg(unix)]
struct Cached {
    shell: PathBuf,
    stamp: Vec<Option<SystemTime>>,
    vars: Option<Arc<Vec<(OsString, OsString)>>>,
}

#[cfg(unix)]
static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// The person's captured environment (their shell's, else `/bin/sh`'s login environment), captured again when a
/// start-up file changed; `None` when neither can be captured (the commands then keep `hostd`'s environment) or when
/// `AGENTROUTER_SHELL_ENV=off`.
#[cfg(unix)]
pub fn captured() -> Option<Arc<Vec<(OsString, OsString)>>> {
    if std::env::var_os("AGENTROUTER_SHELL_ENV").is_some_and(|v| v == "off") {
        return None;
    }
    let shell = person_shell();
    let files = startup_files(
        &shell,
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    );
    let now = stamp(&files);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = cache.as_ref()
        && c.shell == shell
        && c.stamp == now
    {
        return c.vars.clone();
    }
    let sh = Path::new("/bin/sh");
    let vars = capture(&shell)
        .or_else(|| (shell != sh).then(|| capture(sh)).flatten())
        .map(Arc::new);
    *cache = Some(Cached {
        shell,
        stamp: now,
        vars: vars.clone(),
    });
    vars
}

#[cfg(not(unix))]
pub fn captured() -> Option<std::sync::Arc<Vec<(OsString, OsString)>>> {
    None
}

/// The environment a command starts with; `None` = `hostd`'s own, unchanged.
pub fn for_command() -> Option<Env> {
    captured().map(|vars| merge(std::env::vars_os(), &vars))
}

/// One variable of the person's environment (falls back to `hostd`'s).
pub fn var_os(key: &str) -> Option<OsString> {
    if !host_owned(OsStr::new(key))
        && let Some(vars) = captured()
        && let Some((_, v)) = vars.iter().find(|(k, _)| k == key)
    {
        return Some(v.clone());
    }
    std::env::var_os(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(k: &str, v: &str) -> (OsString, OsString) {
        (k.into(), v.into())
    }

    #[test]
    fn parse_reads_between_the_markers_and_ignores_start_up_noise() {
        let out = format!("welcome!\n{MARK}\nPATH=/a:/b\0EMPTY=\0MULTI=x\ny\0{MARK}");
        let vars = parse(out.as_bytes()).unwrap();
        assert_eq!(
            vars,
            vec![kv("PATH", "/a:/b"), kv("EMPTY", ""), kv("MULTI", "x\ny")]
        );
        assert!(parse(b"no marker here").is_none());
    }

    #[test]
    fn merge_keeps_what_hostd_owns_and_drops_the_capturing_shell() {
        let own = vec![
            kv("PATH", "/usr/bin"),
            kv("HTTPS_PROXY", "http://sandbox:1"),
            kv("ONLY_HOSTD", "1"),
        ];
        let captured = vec![
            kv("PATH", "/home/p/.nvm/bin:/usr/bin"),
            kv("HTTPS_PROXY", "http://corp:8080"),
            kv("http_proxy", "http://corp:8080"),
            kv("SSH_AUTH_SOCK", "/run/user/1/ssh"),
            kv("SHLVL", "2"),
            kv("PWD", "/home/p"),
        ];
        let env = merge(own, &captured);
        assert_eq!(env[OsStr::new("PATH")], "/home/p/.nvm/bin:/usr/bin");
        assert_eq!(env[OsStr::new("HTTPS_PROXY")], "http://sandbox:1");
        // A proxy hostd does not set comes from the person's shell, as in their terminal.
        assert_eq!(env[OsStr::new("http_proxy")], "http://corp:8080");
        assert_eq!(env[OsStr::new("SSH_AUTH_SOCK")], "/run/user/1/ssh");
        assert_eq!(env[OsStr::new("ONLY_HOSTD")], "1");
        assert!(!env.contains_key(OsStr::new("SHLVL")) && !env.contains_key(OsStr::new("PWD")));
    }

    #[cfg(unix)]
    fn home_with_profile(tag: &str, profile: &str) -> PathBuf {
        // Under the build's target folder (never the system temp one, docs/AV-HYGIENE.md): `<target>/debug/deps/<exe>`.
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).expect("target folder");
        let dir = target
            .join("tmp")
            .join("ar-shell-env-unit")
            .join(format!("{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".profile"), profile).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn capture_runs_the_login_shell_and_sees_its_start_up_files() {
        let home = home_with_profile(
            "rc",
            "export AR_FROM_PROFILE=yes\nPATH=\"$HOME/rc-bin:$PATH\"\necho noise\n",
        );
        let vars = capture_in(Path::new("/bin/sh"), Some(&home), LIMIT);
        let _ = std::fs::remove_dir_all(&home);
        let vars = vars.expect("captured");
        let get = |k: &str| {
            vars.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("AR_FROM_PROFILE").as_deref(), Some(OsStr::new("yes")));
        assert!(get("PATH").unwrap().to_string_lossy().contains("rc-bin"));
        assert_eq!(
            get("AGENTROUTER_RESOLVING_ENVIRONMENT").as_deref(),
            Some(OsStr::new("1"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_start_up_file_that_never_ends_is_cut_at_the_limit() {
        let home = home_with_profile("hang", "sleep 30 &\nsleep 30\n");
        let started = std::time::Instant::now();
        let vars = capture_in(
            Path::new("/bin/sh"),
            Some(&home),
            Duration::from_millis(500),
        );
        let _ = std::fs::remove_dir_all(&home);
        assert!(vars.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the background child was ended too"
        );
    }
}
