//! The scope check for commands at the default level (DEVICE-PROTOCOL.md §5.5, owner decision
//! 2026-10-10): inside the linked folders commands run without asking, so a command that clearly acts
//! outside them or harms the system is refused up front (`OUT_OF_SCOPE`), never asked about. It is a
//! heuristic against obvious overreach, not a sandbox: it reads the command text, it cannot see what a
//! script does once it runs.

use std::path::{Component, Path, PathBuf};

use crate::gate::{Scope, display, inside};
use crate::protocol::DeviceError;

/// Why a command was refused, in words for the AI and the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub what: String,
}

impl Blocked {
    fn new(what: impl Into<String>) -> Self {
        Self { what: what.into() }
    }

    pub fn error(&self) -> DeviceError {
        DeviceError::new(
            "OUT_OF_SCOPE",
            format!("这条命令{}，超出了链接的文件夹，没有执行", self.what),
        )
        .next("换一种只在链接的文件夹里做的办法；确实需要的话，在 exec 里带上 beyondScope（一句话说明理由）再发一次，用户会在电脑上确认一次。")
    }
}

const ELEVATION: &[&str] = &["sudo", "su", "doas", "runas", "pkexec", "gsudo"];

/// Programs that change the system itself (disks, boot, registry, services, firewall, security).
const SYSTEM: &[&str] = &[
    "format",
    "diskpart",
    "bcdedit",
    "bootrec",
    "fdisk",
    "sfdisk",
    "parted",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "vssadmin",
    "takeown",
    "icacls",
    "cacls",
    "schtasks",
    "regedit",
    "netsh",
    "wmic",
    "set-executionpolicy",
    "set-mppreference",
    "add-mppreference",
    "new-service",
    "remove-service",
    "set-service",
    "stop-computer",
    "restart-computer",
    "new-itemproperty",
    "set-itemproperty",
    "remove-itemproperty",
    "launchctl",
    "csrutil",
    "spctl",
    "visudo",
    "useradd",
    "userdel",
    "usermod",
    "passwd",
    "net",
];

/// Text that points at stored credentials (lower case, `/` separators).
const CREDENTIALS: &[&str] = &[
    "/.ssh",
    ".ssh/",
    "~/.ssh",
    "/.aws",
    "~/.aws",
    "/.gnupg",
    "/.kube/",
    ".git-credentials",
    "/.netrc",
    "_netrc",
    ".docker/config.json",
    "/.config/gh/",
    "microsoft/credentials",
    "microsoft/protect",
    "microsoft/vault",
    "google/chrome/user data",
    "microsoft/edge/user data",
    "bravesoftware/brave-browser",
    "mozilla/firefox",
    ".mozilla/",
    ".config/google-chrome",
    ".config/chromium",
    "library/application support/google/chrome",
    "library/keychains",
    "login data",
    "cmdkey",
    "vaultcmd",
    "get-storedcredential",
    "find-generic-password",
    "find-internet-password",
    "dump-keychain",
    "export-pfxcertificate",
    "cert:\\",
    "cert:/",
];

/// Commands that write, move or delete files (and so must not name places outside the folders).
const WRITERS: &[&str] = &[
    "rm",
    "rmdir",
    "del",
    "erase",
    "rd",
    "remove-item",
    "ri",
    "mv",
    "move",
    "move-item",
    "mi",
    "cp",
    "copy",
    "copy-item",
    "cpi",
    "xcopy",
    "robocopy",
    "set-content",
    "add-content",
    "ac",
    "out-file",
    "new-item",
    "ni",
    "mkdir",
    "md",
    "touch",
    "tee",
    "tee-object",
    "chmod",
    "chown",
    "chgrp",
    "ln",
    "rename-item",
    "rni",
    "ren",
    "rename",
    "truncate",
    "shred",
    "rsync",
    "install",
    "unzip",
    "tar",
    "expand-archive",
    "compress-archive",
    "dd",
    "clear-content",
    "clc",
    "set-acl",
    "attrib",
    "mklink",
    "subst",
];

const PACKAGE_MANAGERS: &[&str] = &[
    "winget", "choco", "scoop", "brew", "apt", "apt-get", "dnf", "yum", "pacman", "zypper", "snap",
    "port", "apk",
];

fn program(word: &str) -> String {
    let w = word
        .trim_start_matches(['&', '.', '@'])
        .to_ascii_lowercase();
    let w = w.rsplit(['/', '\\']).next().unwrap_or(&w).to_string();
    for ext in [".exe", ".com", ".cmd", ".bat"] {
        if let Some(stem) = w.strip_suffix(ext) {
            return stem.to_string();
        }
    }
    w
}

/// The command split into simple commands (by `;`, `&&`, `||`, `|`, new lines), each as words.
fn segments(command: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => {
                quote = None;
                current.push(' ');
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current.push(' ');
            }
            None if matches!(c, ';' | '|' | '\n' | '\r' | '(' | ')' | '{' | '}')
                || (c == '&'
                    && chars.get(i + 1) != Some(&'1')
                    && chars.get(i + 1) != Some(&'2')) =>
            {
                if c == '&' && i > 0 && chars[i - 1] == '>' {
                    current.push(c);
                } else {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
        i += 1;
    }
    out.push(current);
    out.into_iter()
        .map(|s| s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .filter(|w: &Vec<String>| !w.is_empty())
        .collect()
}

fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir) | None
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Temporary folders count as fair game for builds and tests.
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["TMPDIR", "TEMP", "TMP"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    if !cfg!(windows) {
        dirs.push(PathBuf::from("/tmp"));
        dirs.push(PathBuf::from("/var/tmp"));
    }
    dirs.into_iter()
        .map(|d| std::fs::canonicalize(&d).unwrap_or(d))
        .collect()
}

/// Is this real-or-lexical location one of the linked folders (or a temporary folder)?
fn allowed_place(path: &Path, folders: &[PathBuf]) -> bool {
    let lexical = lexical(path);
    let real = std::fs::canonicalize(&lexical).ok();
    let candidates: Vec<PathBuf> = std::iter::once(lexical.clone()).chain(real).collect();
    let bases: Vec<PathBuf> = folders
        .iter()
        .cloned()
        .chain(temp_dirs())
        .flat_map(|f| {
            let shown = PathBuf::from(display(&f));
            [f, shown]
        })
        .collect();
    candidates
        .iter()
        .any(|c| bases.iter().any(|b| inside(c, b)))
}

/// A word that names a place outside the folders, if it names a place at all.
fn outside_place(word: &str, cwd: &Path, home: &Path, folders: &[PathBuf]) -> Option<String> {
    let w = word.trim_matches(|c| c == ',' || c == '"' || c == '\'');
    let w = w
        .split_once('=')
        .map_or(w, |(k, v)| if k.starts_with('-') { v } else { w });
    if w.is_empty() || w.contains("://") {
        return None;
    }
    let lower = w.to_ascii_lowercase();
    // Environment-relative places.
    for (prefix, base) in [
        ("~", Some(home.to_path_buf())),
        ("$home", Some(home.to_path_buf())),
        ("${home}", Some(home.to_path_buf())),
        ("$env:userprofile", Some(home.to_path_buf())),
        ("%userprofile%", Some(home.to_path_buf())),
        ("$env:homepath", Some(home.to_path_buf())),
    ] {
        if let Some(rest) = lower.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with(['/', '\\']))
        {
            let path = base?.join(rest.trim_start_matches(['/', '\\']));
            return (!allowed_place(&path, folders)).then(|| w.to_string());
        }
    }
    for var in [
        "$env:",
        "%appdata%",
        "%localappdata%",
        "%programdata%",
        "%systemroot%",
        "%windir%",
        "%programfiles",
        "%systemdrive%",
        "$env:appdata",
    ] {
        if lower.starts_with(var)
            && !lower.starts_with("$env:temp")
            && !lower.starts_with("$env:tmp")
        {
            return Some(w.to_string());
        }
    }
    let b = w.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let unc = w.starts_with("\\\\") || w.starts_with("//");
    let rooted = if cfg!(windows) {
        // `/s` and `/q` are options to Windows programs; `/x/y` is a path.
        matches!(w, "/" | "/*" | "/.")
            || (w.starts_with('/') && w[1..].contains('/'))
            || (w.starts_with('\\') && !unc)
    } else {
        w.starts_with('/')
    };
    if unc {
        return Some(w.to_string());
    }
    if drive || rooted {
        let path = if drive && b.len() == 2 {
            PathBuf::from(format!("{w}\\"))
        } else {
            PathBuf::from(w)
        };
        return (!allowed_place(&path, folders)).then(|| w.to_string());
    }
    if w.split(['/', '\\']).any(|part| part == "..") {
        let path = cwd.join(w);
        return (!allowed_place(&path, folders)).then(|| w.to_string());
    }
    None
}

/// Redirection targets (`> file`, `>> file`, `2> file`), not the null device or another stream.
fn redirect_targets(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' && !(i > 0 && matches!(chars[i - 1], '-' | '=')) => {
                let mut j = i + 1;
                while j < chars.len() && (chars[j] == '>' || chars[j] == ' ') {
                    j += 1;
                }
                let target: String = chars[j..]
                    .iter()
                    .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | ')'))
                    .collect();
                let lower = target.to_ascii_lowercase();
                if !target.is_empty()
                    && !target.starts_with('&')
                    && !matches!(lower.as_str(), "/dev/null" | "nul" | "$null" | "null")
                {
                    out.push(target);
                }
                i = j;
                continue;
            }
            None => {}
        }
        i += 1;
    }
    out
}

/// Check a command about to run in `cwd` at the default level.
pub fn check(command: &str, cwd: &Path, scope: &Scope, home: &Path) -> Result<(), Blocked> {
    let lower = command.to_ascii_lowercase().replace('\\', "/");
    for needle in CREDENTIALS {
        if lower.contains(&needle.replace('\\', "/")) {
            return Err(Blocked::new("会碰到这台电脑上保存的密码或密钥"));
        }
    }
    for own in &scope.deny {
        let own = display(own).to_ascii_lowercase().replace('\\', "/");
        if !own.is_empty() && lower.contains(&own) {
            return Err(Blocked::new("会碰到 AgentRouter 小助手自己的数据"));
        }
    }
    if lower.contains("-verb runas") || lower.contains("-verb:runas") {
        return Err(Blocked::new("要以管理员身份运行"));
    }
    for key in [
        "hklm:",
        "hkcu:",
        "registry::",
        "hkey_local_machine",
        "hkey_current_user",
        "hku:",
    ] {
        if lower.contains(key) {
            return Err(Blocked::new("会改注册表"));
        }
    }
    if lower.contains("of=/dev/") {
        return Err(Blocked::new("会直接写磁盘"));
    }
    let segments = segments(command);
    let mut writes = !redirect_targets(command).is_empty();
    for words in &segments {
        let head = program(&words[0]);
        let arg = |i: usize| {
            words
                .get(i)
                .map(|w| w.to_ascii_lowercase())
                .unwrap_or_default()
        };
        let has = |flag: &str| words.iter().skip(1).any(|w| w.eq_ignore_ascii_case(flag));
        if ELEVATION.contains(&head.as_str()) {
            return Err(Blocked::new("要提升权限（管理员 / root）"));
        }
        if SYSTEM.contains(&head.as_str()) || head.starts_with("mkfs") {
            if head == "net" && !matches!(arg(1).as_str(), "user" | "localgroup" | "share" | "use")
            {
                // `net` alone is mostly read-only (`net view`); accounts and shares are not.
            } else {
                return Err(Blocked::new(
                    "会改系统设置（磁盘、启动、服务、账户或安全设置）",
                ));
            }
        }
        if head == "reg"
            && matches!(
                arg(1).as_str(),
                "add" | "delete" | "import" | "load" | "restore" | "copy" | "unload"
            )
        {
            return Err(Blocked::new("会改注册表"));
        }
        if head == "sc"
            && matches!(
                arg(1).as_str(),
                "create" | "delete" | "config" | "stop" | "start" | "failure"
            )
        {
            return Err(Blocked::new("会改系统服务"));
        }
        if head == "systemctl"
            && matches!(
                arg(1).as_str(),
                "enable"
                    | "disable"
                    | "mask"
                    | "unmask"
                    | "stop"
                    | "start"
                    | "restart"
                    | "reload"
                    | "kill"
                    | "isolate"
                    | "daemon-reload"
            )
        {
            return Err(Blocked::new("会改系统服务"));
        }
        if head == "cipher"
            && words
                .iter()
                .any(|w| w.to_ascii_lowercase().starts_with("/w"))
        {
            return Err(Blocked::new("会擦除磁盘上的数据"));
        }
        if head == "crontab" && (has("-r") || has("-e") || words.len() == 2) {
            return Err(Blocked::new("会改计划任务"));
        }
        // Global installs: outside the folders, for every program on the computer.
        let global = has("-g") || has("--global") || has("--location=global");
        let installs = |w: &str| {
            matches!(
                w,
                "install"
                    | "i"
                    | "add"
                    | "link"
                    | "update"
                    | "upgrade"
                    | "uninstall"
                    | "remove"
                    | "rm"
            )
        };
        if matches!(head.as_str(), "npm" | "pnpm" | "bun" | "yarn")
            && global
            && words
                .iter()
                .skip(1)
                .any(|w| installs(&w.to_ascii_lowercase()))
        {
            return Err(Blocked::new("要装到全局（文件夹外面）"));
        }
        if head == "yarn" && arg(1) == "global" {
            return Err(Blocked::new("要装到全局（文件夹外面）"));
        }
        if matches!(head.as_str(), "cargo" | "go" | "pipx" | "gem") && arg(1) == "install" {
            return Err(Blocked::new("要装到全局（文件夹外面）"));
        }
        let pip = matches!(head.as_str(), "pip" | "pip3")
            || (head.starts_with("python") && arg(1) == "-m" && arg(2).starts_with("pip"));
        if pip
            && words.iter().any(|w| w.eq_ignore_ascii_case("install"))
            && (has("--user")
                || has("--target")
                || has("--prefix")
                || has("--break-system-packages"))
        {
            return Err(Blocked::new("要装到全局（文件夹外面）"));
        }
        if head == "dotnet"
            && arg(1) == "tool"
            && arg(2) == "install"
            && (has("-g") || has("--global") || has("--tool-path"))
        {
            return Err(Blocked::new("要装到全局（文件夹外面）"));
        }
        if PACKAGE_MANAGERS.contains(&head.as_str())
            && matches!(
                arg(1).as_str(),
                "install"
                    | "add"
                    | "upgrade"
                    | "uninstall"
                    | "remove"
                    | "-s"
                    | "-syu"
                    | "-r"
                    | "update"
            )
        {
            return Err(Blocked::new("要用系统的包管理器装软件"));
        }
        if words.iter().any(|w| WRITERS.contains(&program(w).as_str())) {
            writes = true;
        }
        if head == "git" && arg(1) == "clone" {
            writes = true;
        }
    }
    if writes {
        let folders = &scope.folders;
        let mut places: Vec<String> = segments.iter().flatten().cloned().collect();
        places.extend(redirect_targets(command));
        for word in places {
            if let Some(place) = outside_place(&word, cwd, home, folders) {
                return Err(Blocked::new(format!(
                    "会写或删链接的文件夹以外的地方（{}）",
                    crate::util::clip(&place, 120)
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Access;

    fn setup() -> (Scope, PathBuf, PathBuf) {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/unit-tmp/scope-guard");
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap();
        let home = std::fs::canonicalize(&root).unwrap();
        (
            Scope {
                access: Access::Folders,
                folders: vec![repo.clone()],
                deny: Vec::new(),
            },
            repo,
            home,
        )
    }

    #[test]
    fn ordinary_work_runs() {
        let (scope, repo, home) = setup();
        for c in [
            "npm install",
            "npm test",
            "cargo build --release",
            "git status; git add -A; git commit -m 'fix: x'",
            "rm -rf node_modules dist",
            "Remove-Item -Recurse -Force .\\dist",
            "python -m pytest -q > test.log 2>&1",
            "dotnet build | Out-Null",
            "Get-ChildItem C:\\Windows",
            "cat /etc/hosts",
            "mkdir -p build/out && cp src/a.txt build/out/",
            "node scripts/gen.js > /dev/null",
            "pip install -r requirements.txt",
            "git clone https://github.com/x/y.git vendor/y",
            "echo hi > out.txt",
            "npx tsc --outDir ./dist",
            "Write-Output 'a' 2>&1",
        ] {
            assert!(
                check(c, &repo, &scope, &home).is_ok(),
                "{c}: {:?}",
                check(c, &repo, &scope, &home)
            );
        }
        let sub = repo.join("src");
        assert!(check("rm -rf ../dist", &sub, &scope, &home).is_ok());
    }

    #[test]
    fn overreach_is_refused() {
        let (scope, repo, home) = setup();
        let mut cases = vec![
            "rm -rf /",
            "rm -rf ~",
            "rm -rf $HOME/projects",
            "rm -rf ../other",
            "cd .. && rm -rf *",
            "cat ~/.ssh/id_rsa",
            "type C:\\Users\\me\\.ssh\\id_ed25519",
            "cp .env ~/backup.env",
            "echo x > ~/.bashrc",
            "sudo apt install jq",
            "Start-Process powershell -Verb RunAs",
            "runas /user:Administrator cmd",
            "npm install -g typescript",
            "npm i --global pnpm",
            "cargo install ripgrep",
            "pip install --user requests",
            "winget install Git.Git",
            "brew install jq",
            "reg add HKCU\\Software\\X /v Y /d 1",
            "Set-ItemProperty -Path HKCU:\\Software\\X -Name Y -Value 1",
            "format D: /q",
            "diskpart",
            "schtasks /create /tn x /tr calc.exe /sc onlogon",
            "set-executionpolicy unrestricted",
            "cmdkey /list",
            "dd if=/dev/zero of=/dev/sda",
            "Remove-Item $env:APPDATA\\x -Recurse",
        ];
        if cfg!(windows) {
            cases.extend([
                "Remove-Item -Recurse C:\\Windows\\Temp2",
                "del /s /q C:\\Users\\me\\Documents",
                "copy a.txt D:\\elsewhere\\",
            ]);
        } else {
            cases.extend(["rm -rf /usr/local/lib", "cp a.txt /etc/"]);
        }
        for c in cases {
            let r = check(c, &repo, &scope, &home);
            assert!(r.is_err(), "{c}");
            assert_eq!(r.unwrap_err().error().code, "OUT_OF_SCOPE");
        }
    }
}
