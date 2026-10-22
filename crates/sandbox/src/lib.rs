//! The sandbox launcher (docs/HOSTD-MCP.md §6, issue #42): every local MCP server process — and everything it
//! starts — runs inside an OS sandbox whose only writable places are the linked folder(s) and a scratch folder of
//! its own. A write, rename or delete anywhere else fails in the kernel, whatever the server or a command it runs
//! tries (2026-10-18: home dot-folders were deleted on the owner's PC by an unconfined process; #39).
//!
//! Backend: Anthropic `sandbox-runtime` (`srt`, Apache-2.0), pinned to [`SRT_VERSION`] with an npm lockfile that
//! carries the integrity of every package (`runtime/`), installed with `npm ci` beside the device data folder and used
//! as a process wrapper (`node cli.js --settings <policy> -- <program> <args>`). Node (and uv, for `uvx` servers) are
//! the device's own pinned copies ([`runtimes`], #116): the person installs nothing.
//!
//! - macOS: Seatbelt (`sandbox-exec`) profile; no setup.
//! - Linux: bubblewrap + a network namespace. bubblewrap, and srt's ripgrep and socat, are the device's own pinned
//!   copies (#119, #122); the person installs nothing. Where AppArmor restricts unprivileged user namespaces (Ubuntu
//!   23.10+), the bubblewrap that runs is a root-owned copy that one AppArmor profile lets make them ([`userns`]), set
//!   up by one admin line that [`Runtime::status`] names ([`NEEDS_ADMIN`]).
//! - Windows (alpha upstream): the command runs as the dedicated `srt-sandbox` account (one elevated setup:
//!   [`Runtime::setup`]); write access is explicit ACEs on the folder and the scratch folder, network is a WFP
//!   fence keyed on that account. srt-win gives the child a fresh environment and runs it through `cmd /c`, so the
//!   device starts [`ENTRY_JS`] in the sandbox, which starts the real server with its arguments and environment.
//!   Two grants make that work from the person's profile (device #66, both set by [`Runtime::setup`] and checked on
//!   every start): srt's **profile read grant** (`srt-win profile grant`, since 0.0.81: the profile readable to the
//!   account except credential stores and the device data folder — the read rule of §6.1, as on macOS and Linux;
//!   without it Node cannot even resolve a script path under the profile), and a read+execute ACE on the installed
//!   runtime folder, because srt-win's broker starts its runner as that account from `srt-win.exe` before any
//!   session grant (it also covers a data folder outside the profile, `AGENTROUTER_DEVICE_HOME`). What sandboxes read
//!   and run lives beside the data folder (`<data>-shared`), never inside it.
//!
//! Policy (§6.1): write = folders + scratch (TEMP and the npm/uv/pip caches point there); `.git/hooks` and
//! `.git/config` stay unwritable (srt's mandatory denies, plus explicit ones); reads are allowed except the
//! person's secrets and the device data folder; network = package mirrors + the server's catalog list, through
//! srt's proxy, or the open web for a catalog preset whose list is [`OPEN_WEB`] ([`Launch::open_web`]); local ports
//! as in the person's terminal ([`Runtime::local_binding`], #139); the person's tool configuration read-only
//! ([`PROFILE_ENV`], #138); file watching and terminals on macOS ([`MACOS_MACH_SERVICES`], #141; `allowPty`). There is no unconfined
//! fallback: when the sandbox cannot run, [`Unavailable`] says what to do.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
#[cfg(not(windows))]
use std::sync::OnceLock;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub mod runtimes;
pub mod userns;

/// The npm package and the exact version the device runs (re-vet licence, release notes and Windows status on each
/// bump; the lockfile in `runtime/` pins every dependency's integrity).
pub const SRT_PACKAGE: &str = "@anthropic-ai/sandbox-runtime";
pub const SRT_VERSION: &str = "0.0.81";
const PACKAGE_JSON: &str = include_str!("../runtime/package.json");
const PACKAGE_LOCK: &str = include_str!("../runtime/package-lock.json");
/// Started inside the sandbox on Windows (see the module docs).
pub const ENTRY_JS: &str = include_str!("entry.cjs");
/// Started instead of srt's `cli.js` (same arguments) for an open-web launch: srt's own CLI, with an ask callback that
/// allows public destinations only (see the file).
pub const OPEN_WEB_JS: &str = include_str!("open-web.mjs");
/// The Mach services a sandboxed process may reach on macOS beyond srt's own list (`network.allowMachLookup`): the
/// file-system events service, which every file watcher on macOS goes through (FSEventStream; libuv's `fs.watch` on a
/// folder; chokidar).
const MACOS_MACH_SERVICES: &[&str] = &["com.apple.FSEvents"];
/// The loopback ports srt's proxies may use on Windows (the WFP fence permits only these). srt's default is ten
/// ports (two per server) and srt-win accepts at most 65; 64 ports cover far more servers than one link runs.
pub const WINDOWS_PROXY_PORTS: (u16, u16) = (60080, 60143);
/// The error code a call gets when its server cannot be confined (never run unconfined instead).
pub const UNAVAILABLE: &str = "SANDBOX_UNAVAILABLE";
/// The sandbox needs a one-time change only the computer's administrator can make (a system package such as
/// bubblewrap, or a kernel setting): `agentrouter sandbox setup` cannot help, the person runs the one line in `next`
/// (Cloud error catalog).
pub const NEEDS_ADMIN: &str = "HOSTD_SANDBOX_NEEDS_ADMIN";

/// Reachable from every server (owner 2026-10-19, Q6 「可以」): package registries and their mirrors, and where uv
/// downloads Python from. A server's own catalog list is added to this.
pub const PACKAGE_MIRRORS: &[&str] = &[
    "registry.npmjs.org",
    "registry.npmmirror.com",
    "cdn.npmmirror.com",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "pypi.tuna.tsinghua.edu.cn",
    "mirrors.aliyun.com",
    "mirrors.cloud.tencent.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "releases.astral.sh",
];

/// The catalog `network` entry that asks for the open web (owner 2026-10-20 「可以开放到完整的互联网」, the browser
/// presets). The device honours it only for a pinned catalog preset; anywhere else it is dropped.
pub const OPEN_WEB: &str = "*";

/// Variables that point a tool at the person's own configuration when the sandbox's home is not theirs (device #138):
/// on Windows the sandbox account has its own profile, on macOS and Linux `HOME` is the server's scratch folder. The
/// list srt-win offers (`srt-win profile status` → `env`; srt README "Readable files that can contain package-feed
/// tokens"): on Windows taken from there, on macOS and Linux found the same way here ([`Runtime::profile_env`]). The
/// files stay read-only; a file on the read deny list is never named; a variable the server's environment sets wins.
pub const PROFILE_ENV: &[&str] = &[
    "NPM_CONFIG_USERCONFIG",
    "GIT_CONFIG_GLOBAL",
    "PIP_CONFIG_FILE",
    "MAVEN_ARGS",
    "RUSTUP_HOME",
    "CARGO_HOME",
];

/// What the open web never includes beyond srt's own resolved-address deny set (loopback, link-local, multicast,
/// cloud metadata, this host's addresses): private-use space, so the LAN stays closed (srt README
/// `deniedResolvedAddresses`).
pub const PRIVATE_RANGES: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "fc00::/7",
    "fec0::/10",
];

/// Why a server cannot be started confined, and what the person can do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    /// [`UNAVAILABLE`] (`agentrouter sandbox setup` fixes it) or [`NEEDS_ADMIN`] (only the line in `next` does).
    pub code: &'static str,
    pub message: String,
    pub next: String,
}

impl Unavailable {
    fn new(message: impl Into<String>, next: impl Into<String>) -> Unavailable {
        Unavailable {
            code: UNAVAILABLE,
            message: message.into(),
            next: next.into(),
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn needs_admin(message: impl Into<String>, next: impl Into<String>) -> Unavailable {
        Unavailable {
            code: NEEDS_ADMIN,
            message: message.into(),
            next: next.into(),
        }
    }

    /// `agentrouter sandbox setup` can fix it (it cannot install system packages or change kernel settings).
    pub fn setup_helps(&self) -> bool {
        self.code != NEEDS_ADMIN
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.message, self.next)
    }
}

/// User namespaces the sandbox cannot get, and the one admin line that gives them (#122). Where AppArmor restricts them,
/// the line installs the profile for the device's root-owned bubblewrap ([`userns::install`]). Where a kernel switch
/// refuses them, the line is that switch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn userns_needs_admin(detail: Option<&str>) -> Unavailable {
    let seen = detail.map(|d| format!("（{d}）")).unwrap_or_default();
    if userns::restricted() {
        return Unavailable::needs_admin(
            format!(
                "这台电脑只让登记过的程序建带权限的用户命名空间（Ubuntu 23.10 起默认这样），文件夹模式的隔离靠它；AgentRouter 自己的隔离程序 bubblewrap 还没登记{seen}"
            ),
            format!(
                "请用户在终端运行这一行（会要电脑的登录密码；只给 AgentRouter 自己的这一个隔离程序加一条 AppArmor 规则，重启后也有效）：{}；然后再试。撤销用 {}。或者在信任的情况下用 agentrouter start --full。",
                userns::admin_line("install"),
                userns::admin_line("remove")
            ),
        );
    }
    match userns::kernel_line() {
        Some(line) => Unavailable::needs_admin(
            format!("这台电脑的内核关掉了普通程序的用户命名空间，文件夹模式的隔离靠它{seen}"),
            format!(
                "请用户在终端运行这一行（会要电脑的登录密码，重启后也有效）：{line}；然后再试。或者在信任的情况下用 agentrouter start --full。"
            ),
        ),
        None => Unavailable::new(
            format!("隔离环境试运行失败：用户命名空间建不起来{seen}"),
            NEXT_SETUP,
        ),
    }
}

const NEXT_SETUP: &str = "请用户在这台电脑上运行 agentrouter sandbox setup（装好隔离环境；Windows 会弹一次管理员确认），然后再试。";

/// One process to start confined.
#[derive(Debug, Clone)]
pub struct Launch<'a> {
    /// Names the server (one scratch folder and one policy file per key).
    pub key: &'a str,
    /// The resolved program.
    pub program: &'a Path,
    pub args: &'a [String],
    /// Its environment on top of the device's (PATH included). [`Runtime::scratch_env`] is added when missing.
    pub env: &'a [(String, String)],
    pub cwd: &'a Path,
    /// Where it may write (the linked folders; `cwd` is added when it is not inside one).
    pub folders: &'a [PathBuf],
    /// Domains beyond [`PACKAGE_MIRRORS`] (the catalog's list for this server).
    pub network: &'a [String],
    /// Read-only exceptions inside hidden places (the device's shim folder and checked packages).
    pub readable: &'a [PathBuf],
    /// `agentrouter link --read-only`: the folders stay readable but not writable (only scratch is).
    pub read_only: bool,
    /// Any public host on any port besides `network` (a pinned catalog preset whose list holds [`OPEN_WEB`]; the
    /// caller decides). Loopback, link-local, multicast, cloud metadata, [`PRIVATE_RANGES`] and this computer's own
    /// addresses stay closed, by name and by IP literal.
    pub open_web: bool,
    /// Windows (device #129): the named Job object the sandbox entry joins before it starts the program, so the
    /// environment's limits hold on the confined tree. The caller made the job (and holds its limits); the entry
    /// joins it with `<program> sandbox job-join` and refuses to start the program when that fails. Ignored
    /// elsewhere (Linux scopes and macOS sampling work from outside).
    pub job: Option<JobJoin<'a>>,
}

/// [`Launch::job`]: the job's name and the device program that joins it (`agentrouter`, which the sandbox account
/// already runs as the link's `host` server).
#[derive(Debug, Clone, Copy)]
pub struct JobJoin<'a> {
    pub name: &'a str,
    pub program: &'a Path,
}

/// What `agentrouter sandbox status` shows and `hello` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `srt@0.0.81`.
    pub backend: String,
    /// The OS mechanism (`Seatbelt`, `bubblewrap`, `Windows 隔离账户`).
    pub mechanism: &'static str,
    pub ready: bool,
    /// What is missing, each with what to do.
    pub problems: Vec<Unavailable>,
    /// What works less than it could, without stopping anything (Windows: loopback still closed to the sandbox, so a
    /// command cannot reach a dev server it started; `agentrouter sandbox setup` opens it, device #139).
    pub notes: Vec<Unavailable>,
}

/// The installed runtime and the device data folder it protects.
#[derive(Debug, Clone)]
pub struct Runtime {
    data_dir: PathBuf,
    scratch_root: PathBuf,
    shared_root: PathBuf,
}

struct Ready {
    node: PathBuf,
    cli: PathBuf,
    /// The sandbox account (`srt-win status`).
    #[cfg(windows)]
    sid: String,
    /// The machine's loopback mode ([`Runtime::local_binding`]); every session asks for the installed one.
    #[cfg(windows)]
    loopback: bool,
    /// `srt-win profile status` → `env` ([`PROFILE_ENV`]).
    #[cfg(windows)]
    profile_env: Vec<(String, String)>,
}

/// A confined stdio server ([`Runtime::stdio_command`]): its command and, on Windows, the pipe its stdin and stdout
/// travel over.
pub struct Confined {
    pub command: Command,
    pub pipe: Option<StdioPipe>,
}

/// The server end of the named pipe that carries a confined server's stdin and stdout on Windows (#112). srt-win
/// hands a sandboxed child an empty stdin by design (upstream R14) and uses the runner's stdin for its own launch
/// spec, so a stdio server inside the sandbox would read end-of-file and exit before its first request. Instead
/// [`ENTRY_JS`] connects to this pipe from inside the sandbox — the sandbox side holds only a client end, as with
/// srt's own pipes — and relays it to the server's stdin and from its stdout; stderr stays on srt's pumped stderr.
///
/// The pipe exists before the command starts: one instance only (`FILE_FLAG_FIRST_PIPE_INSTANCE`, so a pipe someone
/// else created under the name is refused), no remote clients, and a protected DACL whose one entry lets the sandbox
/// account read and write it. Its name (128 random bits) is in the launch file only. Never on another OS.
pub struct StdioPipe {
    /// Created with `FILE_FLAG_OVERLAPPED` (what tokio's `NamedPipeServer::from_raw_handle` asks for).
    #[cfg(windows)]
    pub handle: std::os::windows::io::OwnedHandle,
    #[cfg(not(windows))]
    _never: std::convert::Infallible,
}

impl Runtime {
    /// The runtime of the device whose data folder is `data_dir` (hidden from every sandbox). Beside it, never inside
    /// it: the scratch folders (`<data>-scratch`) and the software sandboxes read and run, srt itself and the pinned
    /// MCP packages (`<data>-shared`; srt refuses a session that grants a read inside the data folder, which the
    /// Windows profile read grant excludes, #66).
    pub fn new(data_dir: &Path) -> Runtime {
        let sibling = |suffix: &str| {
            let mut p = data_dir.as_os_str().to_os_string();
            p.push(suffix);
            PathBuf::from(p)
        };
        Runtime {
            data_dir: data_dir.to_path_buf(),
            scratch_root: sibling("-scratch"),
            shared_root: sibling("-shared"),
        }
    }

    /// Where the pinned MCP packages live (readable to sandboxes; `mcp_pkg`).
    pub fn packages_dir(&self) -> PathBuf {
        self.shared_root.join("mcp-packages")
    }

    pub fn backend() -> String {
        format!("srt@{SRT_VERSION}")
    }

    /// Where the device's own Node.js and uv live ([`runtimes`]; readable and runnable from every sandbox).
    pub fn runtimes_dir(&self) -> PathBuf {
        self.shared_root.join("runtimes")
    }

    /// The device's pinned Node.js (srt, the Windows sandbox entry and `npm ci` run on it).
    pub fn node(&self) -> Result<PathBuf, String> {
        runtimes::NODE
            .program(&self.runtimes_dir(), "node")
            .ok_or_else(|| "隔离环境要用的 Node.js 还没准备好".to_string())
    }

    /// The program folders of the installed runtimes local tools run on ([`runtimes::TOOLS`]): first on every local
    /// tool's PATH, so `npx` runs on the device's Node and `uvx` is the device's uv.
    pub fn runtime_bins(&self) -> Vec<PathBuf> {
        let root = self.runtimes_dir();
        runtimes::TOOLS
            .iter()
            .filter(|r| r.installed(&root))
            .map(|r| r.bin_dir(&root))
            .collect()
    }

    /// A local tool's program: `node`, `npm`, `npx`, `uv` and `uvx` from the device's runtimes when they are
    /// installed; anything else (and those, where the device has none, e.g. a cloud sandbox image) from PATH.
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let root = self.runtimes_dir();
        runtimes::TOOLS
            .iter()
            .find_map(|r| r.program(&root, name))
            .or_else(|| find_program(name, None))
    }

    /// `path` (a PATH value) with [`Runtime::runtime_bins`] in front.
    pub fn path_with_runtimes(&self, path: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
        let mut dirs = self.runtime_bins();
        if let Some(path) = path {
            dirs.extend(std::env::split_paths(path).filter(|p| !p.as_os_str().is_empty()));
        }
        std::env::join_paths(dirs)
            .unwrap_or_else(|_| path.map(|p| p.to_os_string()).unwrap_or_default())
    }

    /// Download and unpack the pinned runtimes that are missing ([`runtimes`]; no elevation): what local tools run on
    /// and, for the sandbox (`confined`), what srt runs on this OS (Linux: ripgrep and socat). A required one that
    /// fails is the error (Node: srt runs on it; ripgrep and socat: srt cannot confine without them); without uv only
    /// `uvx` tools are missing, so that is a line, not a failure.
    pub fn provision(&self, confined: bool) -> Result<Vec<String>, String> {
        let root = self.runtimes_dir();
        let mut done = Vec::new();
        let sandbox: &[&runtimes::Pinned] = if confined {
            runtimes::sandbox_programs()
        } else {
            &[]
        };
        for rt in runtimes::TOOLS.iter().chain(sandbox) {
            match rt.ensure(&root) {
                Ok(true) => done.push(format!(
                    "准备好了 {} {}（AgentRouter 自己用，不改这台电脑上已有的）",
                    rt.title, rt.version
                )),
                Ok(false) => {}
                Err(e) if rt.required => return Err(e),
                Err(e) => done.push(format!(
                    "{} 没准备好，要用它的本机工具这次起不来；下次运行 agentrouter sandbox setup 会再试（{e}）",
                    rt.title
                )),
            }
        }
        Ok(done)
    }

    /// npm's own entry in the device's Node (run as `node npm-cli.js …`: no `npm.cmd`, no cmd.exe).
    fn npm_cli(&self) -> PathBuf {
        let root = self.runtimes_dir();
        let base = if cfg!(windows) {
            runtimes::NODE.dir(&root)
        } else {
            runtimes::NODE.dir(&root).join("lib")
        };
        base.join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js")
    }

    fn root(&self) -> PathBuf {
        self.shared_root.join("sandbox")
    }

    fn dir(&self) -> PathBuf {
        self.root().join(format!("srt-{SRT_VERSION}"))
    }

    fn package_dir(&self) -> PathBuf {
        self.dir()
            .join("node_modules")
            .join("@anthropic-ai")
            .join("sandbox-runtime")
    }

    fn cli(&self) -> PathBuf {
        self.package_dir().join("dist").join("cli.js")
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn srt_win(&self) -> PathBuf {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        self.package_dir()
            .join("vendor")
            .join("srt-win")
            .join(arch)
            .join("srt-win.exe")
    }

    /// The pinned version is installed (from the pinned lockfile).
    pub fn installed(&self) -> bool {
        let manifest = std::fs::read_to_string(self.package_dir().join("package.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let lock_same = std::fs::read_to_string(self.dir().join("package-lock.json"))
            .is_ok_and(|t| t == PACKAGE_LOCK);
        manifest.is_some_and(|m| m["version"] == SRT_VERSION) && lock_same && self.cli().is_file()
    }

    /// Install the pinned runtime: `npm ci` from the pinned lockfile (npm checks every package's integrity
    /// against it), no install scripts. Older versions are removed.
    pub fn install(&self) -> Result<(), String> {
        let node = self.node()?;
        let npm = self.npm_cli();
        if !npm.is_file() {
            return Err(format!("找不到 {}（随 Node.js 一起来）", shown(&npm)));
        }
        let dir = self.dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("清不掉旧的 {}：{e}", shown(&dir)))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("建不了 {}：{e}", shown(&dir)))?;
        std::fs::write(dir.join("package.json"), PACKAGE_JSON).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("package-lock.json"), PACKAGE_LOCK).map_err(|e| e.to_string())?;
        let out = Command::new(&node)
            .arg(&npm)
            .args([
                "ci",
                "--ignore-scripts",
                "--omit=dev",
                "--no-audit",
                "--no-fund",
                "--loglevel=error",
            ])
            .current_dir(&dir)
            .env(
                "PATH",
                self.path_with_runtimes(std::env::var_os("PATH").as_deref()),
            )
            .output()
            .map_err(|e| format!("npm 起不来：{e}"))?;
        if !out.status.success() {
            return Err(format!(
                "npm ci 没成功：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if !self.installed() {
            return Err(format!("装完了但找不到 {SRT_PACKAGE}@{SRT_VERSION}"));
        }
        // The runtime of builds before #66 sat inside the data folder.
        let _ = std::fs::remove_dir_all(self.data_dir.join("sandbox"));
        // Earlier pinned versions.
        if let Ok(entries) = std::fs::read_dir(self.root()) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("srt-") && e.path() != dir {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Ok(())
    }

    /// `agentrouter sandbox setup`: the device's own Node.js and uv (and on Linux ripgrep and socat,
    /// [`Runtime::provision`]), srt when it is missing and, on Windows, the sandbox account and its network fence (one
    /// UAC prompt, owner Q4). Returns what was done.
    pub fn setup(&self) -> Result<Vec<String>, String> {
        let mut done = self.provision(true)?;
        if !self.installed() {
            self.install()?;
            done.push(format!("装好了 {SRT_PACKAGE}@{SRT_VERSION}"));
        }
        #[cfg(windows)]
        {
            let before = windows_install(&self.srt_win())?;
            let closed = !before.loopback.unwrap_or_else(|| self.loopback_marked());
            if before.sid.is_none() || before.stale || closed {
                let range = format!("{}-{}", WINDOWS_PROXY_PORTS.0, WINDOWS_PROXY_PORTS.1);
                let mut cmd = Command::new(self.srt_win());
                // Loopback open to the sandbox (device #139): a command reaches the dev server it started, as in the
                // person's own terminal and as on macOS. Machine-wide; changing an existing install needs --force.
                cmd.args(["install", "--proxy-port-range", &range, "--allow-loopback"]);
                if before.sid.is_some() && !before.stale {
                    cmd.arg("--force");
                }
                let out = cmd.output().map_err(|e| format!("srt-win 起不来：{e}"))?;
                match out.status.code() {
                    Some(0) => {}
                    Some(10) => return Err("管理员确认被取消了，什么都没改".into()),
                    _ => {
                        return Err(format!(
                            "隔离账户没建成：{}{}",
                            String::from_utf8_lossy(&out.stdout).trim(),
                            String::from_utf8_lossy(&out.stderr).trim()
                        ));
                    }
                }
                done.push(if before.sid.is_none() {
                    "建好了 Windows 隔离账户 srt-sandbox 和它的网络限制（命令能连本机端口，比如自己起的开发服务器）".into()
                } else if before.stale {
                    "更新了 Windows 隔离账户和它的网络限制".into()
                } else {
                    "隔离环境里的命令现在能连本机端口了（比如自己起的开发服务器）".into()
                });
                let marker = self.loopback_marker();
                std::fs::write(&marker, "open\n")
                    .map_err(|e| format!("写不了 {}：{e}", shown(&marker)))?;
            }
            let after = windows_install(&self.srt_win())?;
            let sid = after
                .sid
                .filter(|_| !after.stale)
                .ok_or_else(|| "隔离账户没建成（srt-win status 仍然没有它）".to_string())?;
            self.windows_runtime_readable(&sid)?;
            // Also a refresh: what was created in the profile since the last run becomes readable.
            let first = windows_profile_state(&self.srt_win())?.0 != "current";
            self.windows_profile_grant()?;
            if first {
                done.push(
                    "隔离账户现在能读你的用户文件夹（密钥、凭据、浏览器数据和 AgentRouter 的数据除外），和 macOS、Linux 一样；只读，不能写"
                        .into(),
                );
            }
        }
        Ok(done)
    }

    /// `agentrouter sandbox remove`: the Windows account and fence (one UAC prompt) and the installed runtime.
    pub fn remove(&self) -> Result<Vec<String>, String> {
        let mut done = Vec::new();
        #[cfg(windows)]
        if self.srt_win().is_file()
            && windows_install(&self.srt_win()).map_or(true, |i| i.sid.is_some())
        {
            let out = Command::new(self.srt_win())
                .arg("uninstall")
                .output()
                .map_err(|e| format!("srt-win 起不来：{e}"))?;
            if out.status.code() == Some(10) {
                return Err("管理员确认被取消了，什么都没改".into());
            }
            if !out.status.success() {
                return Err(format!(
                    "隔离账户没删掉：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            done.push("删掉了 Windows 隔离账户 srt-sandbox 和它的网络限制".into());
        }
        for dir in [self.root(), self.runtimes_dir()] {
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
                done.push(format!("删掉了 {}", shown(&dir)));
            }
        }
        let _ = std::fs::remove_dir_all(self.data_dir.join("sandbox"));
        if self.scratch_root.exists() {
            let _ = std::fs::remove_dir_all(&self.scratch_root);
            done.push(format!("删掉了 {}", shown(&self.scratch_root)));
        }
        Ok(done)
    }

    pub fn status(&self) -> Status {
        let mechanism = if cfg!(windows) {
            "Windows 隔离账户"
        } else if cfg!(target_os = "macos") {
            "Seatbelt"
        } else {
            "bubblewrap"
        };
        let (problems, notes) = match self.ready() {
            #[cfg(windows)]
            Ok(ready) if !ready.loopback => (
                Vec::new(),
                vec![Unavailable::new(
                    "这台电脑的隔离环境还不让命令连本机端口，命令连不上自己起的开发服务器",
                    "请用户在这台电脑上运行 agentrouter sandbox setup（会弹一次管理员确认），再重新链接。",
                )],
            ),
            Ok(_) => (Vec::new(), Vec::new()),
            Err(e) => (vec![e], Vec::new()),
        };
        Status {
            backend: Runtime::backend(),
            mechanism,
            ready: problems.is_empty(),
            problems,
            notes,
        }
    }

    /// Whether sandboxed commands may use local ports directly (srt `network.allowLocalBinding`, device #139): a dev
    /// server binds and the command that started it connects to it, as in the person's own terminal. macOS: yes
    /// (without it a server cannot even bind). Windows: the machine's installed mode, which every session must ask
    /// for (srt: `wfp_loopback_mismatch` otherwise) — `srt-win status` when it can read the filters, else what this
    /// device's setup installed. Linux: not used (the sandbox has its own network namespace and loopback).
    pub fn local_binding(&self) -> bool {
        #[cfg(windows)]
        {
            windows_install(&self.srt_win())
                .ok()
                .and_then(|i| i.loopback)
                .unwrap_or_else(|| self.loopback_marked())
        }
        #[cfg(not(windows))]
        {
            cfg!(target_os = "macos")
        }
    }

    /// Written by [`Runtime::setup`] after it installed the Windows fence with loopback open; read when `srt-win
    /// status` cannot see the filter set (no elevation).
    #[cfg_attr(not(windows), allow(dead_code))]
    fn loopback_marker(&self) -> PathBuf {
        self.root().join("windows-loopback-open")
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn loopback_marked(&self) -> bool {
        self.loopback_marker().is_file()
    }

    fn ready(&self) -> Result<Ready, Unavailable> {
        let node = self.node().map_err(|m| Unavailable::new(m, NEXT_SETUP))?;
        if !self.installed() {
            return Err(Unavailable::new(
                format!("隔离环境（{}）还没装", Runtime::backend()),
                NEXT_SETUP,
            ));
        }
        #[cfg(target_os = "linux")]
        {
            let root = self.runtimes_dir();
            if let Some(rt) = runtimes::SANDBOX_LINUX
                .iter()
                .find(|rt| !rt.installed(&root))
            {
                return Err(Unavailable::new(
                    format!("隔离环境要用的 {} 还没准备好", rt.title),
                    NEXT_SETUP,
                ));
            }
            if userns::restricted() && !userns::installed() {
                return Err(userns_needs_admin(None));
            }
        }
        #[cfg(not(windows))]
        self.probe(&node)?;
        #[cfg(windows)]
        let sid = {
            let install =
                windows_install(&self.srt_win()).map_err(|e| Unavailable::new(e, NEXT_SETUP))?;
            let sid = match install.sid {
                None => return Err(Unavailable::new("Windows 隔离账户还没建", NEXT_SETUP)),
                Some(_) if install.stale => {
                    return Err(Unavailable::new(
                        "Windows 隔离账户是另一个版本的隔离环境建的，要更新一次",
                        NEXT_SETUP,
                    ));
                }
                Some(sid) => sid,
            };
            self.windows_runtime_readable(&sid)
                .map_err(|e| Unavailable::new(e, NEXT_SETUP))?;
            let profile_env = match windows_profile_state(&self.srt_win()) {
                Ok((state, env)) if state == "current" => env,
                Ok((state, _)) => {
                    return Err(Unavailable::new(
                        format!("隔离账户还不能读你的用户文件夹（{state}）"),
                        NEXT_SETUP,
                    ));
                }
                Err(e) => return Err(Unavailable::new(e, NEXT_SETUP)),
            };
            let loopback = install.loopback.unwrap_or_else(|| self.loopback_marked());
            (sid, loopback, profile_env)
        };
        #[cfg(windows)]
        let (sid, loopback, profile_env) = sid;
        Ok(Ready {
            node,
            cli: self.cli(),
            #[cfg(windows)]
            sid,
            #[cfg(windows)]
            loopback,
            #[cfg(windows)]
            profile_env,
        })
    }

    /// The sandbox account may read and run the installed runtime (`srt-win.exe` is started as that account; see the
    /// module docs, #66). One inheriting read+execute ACE for the account's SID on the runtime folder, set with the
    /// person's own rights (they own the data folder; no elevation); once per process and folder, as `icacls`
    /// rewrites the whole tree. The data folder above it stays unreadable to the account.
    #[cfg(windows)]
    fn windows_runtime_readable(&self, sid: &str) -> Result<(), String> {
        // srt, and the device's Node.js and uv (the sandbox entry and `npx`/`uvx` servers run on them, #116).
        for dir in [self.dir(), self.runtimes_dir()] {
            if dir.exists() {
                self.windows_read_grant(&dir, sid)?;
            }
        }
        Ok(())
    }

    /// The sandbox account's SID (Windows): what a Job object or pipe made for a confined server grants (#112, #129).
    #[cfg(windows)]
    pub fn windows_sid(&self) -> Result<String, Unavailable> {
        Ok(self.ready()?.sid)
    }

    #[cfg(windows)]
    fn windows_read_grant(&self, dir: &Path, sid: &str) -> Result<(), String> {
        static DONE: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());
        let dir = dir.to_path_buf();
        let key = (dir.clone(), sid.to_string());
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.contains(&key) {
            return Ok(());
        }
        let args = icacls_read_grant(&dir, sid)?;
        let icacls = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("icacls.exe"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("icacls.exe"));
        let out = Command::new(icacls)
            .args(&args)
            .output()
            .map_err(|e| format!("icacls 起不来：{e}"))?;
        if !out.status.success() {
            return Err(format!(
                "隔离账户读不了隔离环境 {}（icacls：{}{}）",
                shown(&dir),
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        done.push(key);
        Ok(())
    }

    /// `srt-win profile grant`, as the person (no elevation): the profile readable to the sandbox account except the
    /// built-in credential stores and, here, the device data folder (§6.1). The first run can take minutes on a large
    /// profile; a later one writes only what changed.
    #[cfg(windows)]
    fn windows_profile_grant(&self) -> Result<(), String> {
        let mut cmd = Command::new(self.srt_win());
        cmd.args(["profile", "grant", "--json", "--quiet"]);
        if plain(&self.data_dir).starts_with(plain(&home_dir())) {
            cmd.arg("--exclude").arg(&self.data_dir);
        }
        let out = cmd.output().map_err(|e| format!("srt-win 起不来：{e}"))?;
        match out.status.code() {
            Some(0) => Ok(()),
            Some(24) | Some(25) => Err(
                "有正在运行的隔离命令，这次没改；关掉这台电脑上其他 agentrouter 链接后再运行 agentrouter sandbox setup".into(),
            ),
            _ => Err(format!(
                "隔离账户读不了你的用户文件夹（srt-win profile grant：{}{}）",
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
        }
    }

    /// One real confined no-op (once per process): the binaries can be present and srt still unable to confine
    /// (#63: Ubuntu 24.04 restricts unprivileged user namespaces, srt's seccomp helper then fails with
    /// `setgroups … Permission denied` and every server loops on its handshake).
    #[cfg(not(windows))]
    fn probe(&self, node: &Path) -> Result<(), Unavailable> {
        static PROBE: OnceLock<Result<(), Unavailable>> = OnceLock::new();
        PROBE
            .get_or_init(|| {
                let scratch = self.scratch("\u{1}probe");
                let _ = std::fs::create_dir_all(&scratch);
                let mut policy = json!({
                    "network": {"allowedDomains": [], "deniedDomains": []},
                    "filesystem": {"denyRead": [], "allowRead": [shown(&self.package_dir()), shown(&self.runtimes_dir())], "allowWrite": [shown(&scratch)], "denyWrite": []},
                });
                self.linux_programs(&mut policy);
                let file = self.root().join("policies").join("probe.json");
                let _ = std::fs::create_dir_all(self.root().join("policies"));
                std::fs::write(&file, policy.to_string())
                    .map_err(|e| Unavailable::new(format!("写不了隔离设置：{e}"), NEXT_SETUP))?;
                let truth = ["/usr/bin/true", "/bin/true"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|p| p.is_file())
                    .unwrap_or_else(|| PathBuf::from("true"));
                let child = Command::new(node)
                    .arg(self.cli())
                    .arg("--settings")
                    .arg(&file)
                    .arg("--")
                    .arg(&truth)
                    .current_dir(&scratch)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| Unavailable::new(format!("隔离环境起不来：{e}"), NEXT_SETUP))?;
                let out = wait_with_timeout(child, std::time::Duration::from_secs(30));
                match out {
                    Some(out) if out.status.success() => Ok(()),
                    Some(out) => {
                        let text = format!(
                            "{}{}",
                            String::from_utf8_lossy(&out.stderr),
                            String::from_utf8_lossy(&out.stdout)
                        );
                        let text = text.trim();
                        let clipped: String = text.chars().take(300).collect();
                        let userns = ["setgroups", "user namespace", "uid_map", "Permission denied", "Operation not permitted"]
                            .iter()
                            .any(|n| text.contains(n));
                        if cfg!(target_os = "linux") && userns {
                            Err(userns_needs_admin(Some(clipped.as_str())))
                        } else {
                            Err(Unavailable::new(
                                format!("隔离环境试运行失败：{clipped}"),
                                NEXT_SETUP,
                            ))
                        }
                    }
                    None => Err(Unavailable::new("隔离环境试运行 30 秒没有结束", NEXT_SETUP)),
                }
            })
            .clone()
    }

    /// On the read deny list (§6.1): the person's secrets and the device data folder.
    fn read_denied(&self, path: &Path) -> bool {
        let path = plain(path);
        secret_paths(&home_dir())
            .into_iter()
            .chain(std::iter::once(self.data_dir.clone()))
            .any(|d| path.starts_with(plain(&d)))
    }

    /// The scratch folder of one server.
    pub fn scratch(&self, key: &str) -> PathBuf {
        let digest = Sha256::digest(key.as_bytes());
        let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        self.scratch_root.join(name)
    }

    /// TEMP and the package caches, pointed into the server's scratch folder (the only writable place besides the
    /// folders). Add these to the server's environment before it starts.
    pub fn scratch_env(&self, key: &str) -> Vec<(String, String)> {
        let s = self.scratch(key);
        let p = |sub: &str| shown(&s.join(sub));
        let mut env = vec![
            ("TMPDIR".to_string(), p("tmp")),
            ("TEMP".to_string(), p("tmp")),
            ("TMP".to_string(), p("tmp")),
            ("npm_config_cache".to_string(), p("npm")),
            ("UV_CACHE_DIR".to_string(), p("uv")),
            ("UV_PYTHON_INSTALL_DIR".to_string(), p("uv-python")),
            // uvx installs the tool itself here (#63: `~/.local/share/uv/tools` is read-only in the sandbox).
            ("UV_TOOL_DIR".to_string(), p("uv-tools")),
            ("UV_TOOL_BIN_DIR".to_string(), p("uv-bin")),
            ("PIP_CACHE_DIR".to_string(), p("pip")),
        ];
        if !cfg!(windows) {
            // srt hands its child TMPDIR = CLAUDE_CODE_TMPDIR, else /tmp/claude, which bubblewrap can only make writable
            // when it already exists on the host (#130: missing on a person's Ubuntu, so `<temp>/agentrouter-run`
            // failed read-only). One temp folder: the scratch one the policy grants. (srt drops TMPDIR on Windows.)
            env.push(("CLAUDE_CODE_TMPDIR".to_string(), p("tmp")));
            env.push(("XDG_CACHE_HOME".to_string(), p("cache")));
            // A home of its own: servers that keep state in `~` (DesktopCommander's `~/.claude-server-commander`,
            // #63) can start; the person's real home stays readable (minus the secrets) but not writable. On Windows
            // the sandbox account has its own profile already.
            env.push(("HOME".to_string(), p("home")));
        }
        env
    }

    /// The srt settings for one launch (written to the device data folder, which no sandbox can read or write).
    pub fn policy(&self, launch: &Launch) -> Result<Value, Unavailable> {
        self.policy_with(launch, self.local_binding())
    }

    /// [`Runtime::policy`] with the loopback mode already known ([`Runtime::local_binding`]).
    fn policy_with(&self, launch: &Launch, local_binding: bool) -> Result<Value, Unavailable> {
        let home = home_dir();
        let mut writable: Vec<PathBuf> = launch.folders.iter().map(|f| plain(f)).collect();
        let cwd = plain(launch.cwd);
        if !writable.iter().any(|f| cwd.starts_with(f)) {
            writable.push(cwd);
        }
        for f in &writable {
            if f.parent().is_none()
                || home.starts_with(f)
                || plain(&self.data_dir).starts_with(f)
                || !f.is_absolute()
            {
                return Err(Unavailable::new(
                    format!(
                        "{} 太大了：不能把用户主目录、磁盘根目录或 AgentRouter 自己的数据文件夹交给本机工具",
                        shown(f)
                    ),
                    "请用户链接一个具体的项目文件夹（agentrouter link --folder <文件夹>）。",
                ));
            }
        }
        let scratch = self.scratch(launch.key);
        let mut allow_write: Vec<String> = if launch.read_only {
            Vec::new()
        } else {
            writable.iter().map(|f| shown(f)).collect()
        };
        allow_write.push(shown(&scratch));
        let mut deny_write = Vec::new();
        for f in &writable {
            for p in [f.join(".git").join("hooks"), f.join(".git").join("config")] {
                if p.exists() {
                    deny_write.push(shown(&p));
                }
            }
        }
        let mut deny_read: Vec<String> = secret_paths(&home)
            .into_iter()
            .chain(std::iter::once(plain(&self.data_dir)))
            .filter(|p| p.exists())
            .map(|p| shown(&p))
            .collect();
        deny_read.dedup();
        // The runtime's helpers that run inside the sandbox (Linux `apply-seccomp`) stay readable.
        let mut allow_read: Vec<String> = launch
            .readable
            .iter()
            .chain([&self.package_dir(), &self.runtimes_dir()])
            .filter(|p| p.exists())
            .map(|p| shown(&plain(p)))
            .collect();
        if cfg!(windows) {
            // Per-user runtimes (nvm/fnm Node, npm's global folder, uv) created after the last profile read grant are
            // not readable through it yet: read+execute grants for this session, removed again when the server ends.
            let mut extra = vec![launch.program.parent().map(Path::to_path_buf)];
            for var in ["APPDATA", "LOCALAPPDATA"] {
                if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
                    extra.push(Some(base.join("npm")));
                    extra.push(Some(base.join("uv")));
                    extra.push(Some(base.join("fnm")));
                    extra.push(Some(base.join("nvm")));
                }
            }
            extra.push(Some(home.join(".local").join("bin")));
            extra.push(Some(home.join(".cargo").join("bin")));
            for p in extra.into_iter().flatten() {
                let p = plain(&p);
                if p.exists() && p.starts_with(&home) && !plain(&self.data_dir).starts_with(&p) {
                    let s = shown(&p);
                    if !allow_read.contains(&s) {
                        allow_read.push(s);
                    }
                }
            }
        }
        let mut domains: Vec<String> = PACKAGE_MIRRORS.iter().map(|d| d.to_string()).collect();
        for d in launch.network {
            if valid_domain(d) && !domains.contains(d) {
                domains.push(d.clone());
            }
        }
        let mut network = json!({"allowedDomains": domains, "deniedDomains": []});
        if launch.open_web {
            network["deniedResolvedAddresses"] = json!(PRIVATE_RANGES);
        }
        if !cfg!(target_os = "linux") {
            // Every loopback port, the person's own local services included (srt README "Local servers on
            // Windows"); the domain list still decides everything beyond this computer. Linux: the sandbox's own
            // loopback, nothing to set.
            network["allowLocalBinding"] = json!(local_binding);
        }
        if cfg!(target_os = "macos") {
            // File watching (device #141): srt's Seatbelt profile lists the Mach services a sandboxed process may
            // reach, and FSEvents is not among them, so every watch inside the sandbox was silently blind — the host
            // server's change feed, and the person's own tools (`fs.watch` on a folder, chokidar, a dev server's hot
            // reload). FSEvents only reports changed paths, never contents, and reads are open in folder mode anyway.
            network["allowMachLookup"] = json!(MACOS_MACH_SERVICES);
        }
        let mut policy = json!({
            "network": network,
            "filesystem": {
                "denyRead": deny_read,
                "allowRead": allow_read,
                "allowWrite": allow_write,
                "denyWrite": deny_write,
            },
        });
        if cfg!(windows) {
            policy["windows"] =
                json!({"proxyPortRange": [WINDOWS_PROXY_PORTS.0, WINDOWS_PROXY_PORTS.1]});
        }
        if cfg!(target_os = "macos") {
            // Terminals (Cloud #2383, parity run 39718508921 T5: `openpty: Operation not permitted`): srt's Seatbelt
            // profile denies pseudo-terminals unless asked (srt README `allowPty`: `(allow pseudo-tty)` and /dev/ptmx,
            // /dev/ttys*). The host server's persistent terminals and every command that opens its own pty need it, as in
            // the person's terminal. Linux and Windows do not read it.
            policy["allowPty"] = json!(true);
        }
        self.linux_programs(&mut policy);
        Ok(policy)
    }

    /// Linux: the programs srt runs, by absolute path (#119, #122): the device's pinned ripgrep, socat and bubblewrap,
    /// never the person's own (whatever version that is). Nothing elsewhere.
    fn linux_programs(&self, policy: &mut Value) {
        if !cfg!(target_os = "linux") {
            return;
        }
        let root = self.runtimes_dir();
        policy["ripgrep"] = json!({"command": shown(&runtimes::RG.bin_dir(&root).join("rg"))});
        policy["socatPath"] = json!(shown(&runtimes::SOCAT.bin_dir(&root).join("socat")));
        policy["bwrapPath"] = json!(shown(&self.bwrap()));
    }

    /// The bubblewrap srt runs: where AppArmor restricts user namespaces, the root-owned copy its profile names
    /// ([`userns`]); elsewhere the device's own copy in its runtimes folder.
    fn bwrap(&self) -> PathBuf {
        if userns::restricted() {
            userns::bwrap()
        } else {
            runtimes::BWRAP.bin_dir(&self.runtimes_dir()).join("bwrap")
        }
    }

    /// The confined command for one program: cwd and environment are set; the caller pipes stdio and spawns. On
    /// Windows the program's stdin is empty (srt-win); a server that reads requests on stdin uses
    /// [`Runtime::stdio_command`].
    pub fn command(&self, launch: &Launch) -> Result<Command, Unavailable> {
        self.confined(launch, false).map(|c| c.command)
    }

    /// The confined command for a stdio server (an MCP server): as [`Runtime::command`], and on Windows its stdin and
    /// stdout travel over [`Confined::pipe`], which the caller connects and uses instead of the child's own stdio
    /// (#112). Elsewhere `pipe` is `None` and the child's stdio is the server's.
    pub fn stdio_command(&self, launch: &Launch) -> Result<Confined, Unavailable> {
        self.confined(launch, true)
    }

    fn confined(&self, launch: &Launch, stdio_pipe: bool) -> Result<Confined, Unavailable> {
        let ready = self.ready()?;
        #[cfg(windows)]
        let policy = self.policy_with(launch, ready.loopback)?;
        #[cfg(not(windows))]
        let policy = self.policy_with(launch, self.local_binding())?;
        let scratch = self.scratch(launch.key);
        let mut env: Vec<(String, String)> = launch.env.to_vec();
        for (k, v) in self.scratch_env(launch.key) {
            if !env.iter().any(|(e, _)| e.eq_ignore_ascii_case(&k)) {
                env.push((k, v));
            }
        }
        for (k, _) in self.scratch_env(launch.key) {
            if let Some((_, v)) = env.iter().find(|(e, _)| e.eq_ignore_ascii_case(&k)) {
                let _ = std::fs::create_dir_all(v);
            }
        }
        // The person's own tool configuration, read-only (git identity, npm/pip/Maven settings, rustup and cargo
        // homes, #138): the sandbox's home is not theirs on any OS.
        #[cfg(windows)]
        let found = ready.profile_env.clone();
        #[cfg(not(windows))]
        let found = profile_env_of(&home_dir(), |k| std::env::var_os(k));
        for (k, v) in found {
            let set = env.iter().any(|(e, _)| e.eq_ignore_ascii_case(&k))
                || (!cfg!(windows) && std::env::var_os(&k).is_some());
            if !set && (k == "MAVEN_ARGS" || !self.read_denied(Path::new(&v))) {
                env.push((k, v));
            }
        }
        std::fs::create_dir_all(&scratch).map_err(|e| {
            Unavailable::new(format!("建不了 {}：{e}", shown(&scratch)), NEXT_SETUP)
        })?;
        let policies = self.root().join("policies");
        let _ = std::fs::create_dir_all(&policies);
        let file_name = scratch
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let policy_file = policies.join(format!("{file_name}.json"));
        std::fs::write(
            &policy_file,
            serde_json::to_string_pretty(&policy).unwrap_or_default(),
        )
        .map_err(|e| Unavailable::new(format!("写不了隔离设置：{e}"), NEXT_SETUP))?;

        let srt = if launch.open_web {
            // Beside srt's node_modules (it imports srt by relative path), in the data folder no sandbox can write.
            let file = self.dir().join("open-web.mjs");
            if std::fs::read_to_string(&file).ok().as_deref() != Some(OPEN_WEB_JS) {
                std::fs::write(&file, OPEN_WEB_JS).map_err(|e| {
                    Unavailable::new(format!("写不了 {}：{e}", shown(&file)), NEXT_SETUP)
                })?;
            }
            file
        } else {
            ready.cli
        };
        let mut cmd = Command::new(&ready.node);
        cmd.arg(&srt).arg("--settings").arg(&policy_file);
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut pipe = None;
        if cfg!(windows) {
            let entry = scratch.join("entry.cjs");
            std::fs::write(&entry, ENTRY_JS).map_err(|e| {
                Unavailable::new(format!("写不了 {}：{e}", shown(&entry)), NEXT_SETUP)
            })?;
            let nonce: [u8; 8] = random();
            let launch_file = scratch.join(format!(
                "launch-{}.json",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            #[cfg_attr(not(windows), allow(unused_mut))]
            let mut spec = json!({
                "program": shown(launch.program),
                "args": launch.args,
                "env": env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect::<serde_json::Map<String, Value>>(),
                "cwd": shown(launch.cwd),
            });
            #[cfg(windows)]
            if let Some(job) = &launch.job {
                spec["job"] = windows_job_spec(job)?;
            }
            #[cfg(windows)]
            if stdio_pipe {
                let (name, handle) = windows_stdio_pipe(&ready.sid).map_err(|e| {
                    Unavailable::new(
                        format!("建不了本机工具的输入输出通道：{e}"),
                        "请重新运行 agentrouter；仍然不行就在 agentrouter-device 仓库报告这一行。",
                    )
                })?;
                spec["pipe"] = Value::String(name);
                pipe = Some(StdioPipe { handle });
            }
            std::fs::write(&launch_file, spec.to_string())
                .map_err(|e| Unavailable::new(format!("写不了启动文件：{e}"), NEXT_SETUP))?;
            // `cmd /d /s /c "<line>"` inside the sandbox: quoted paths only, nothing cmd would expand.
            let parts = [ready.node.clone(), entry, launch_file];
            for p in &parts {
                let s = shown(p);
                if s.contains(['"', '%', '!', '^', '\r', '\n']) {
                    return Err(Unavailable::new(
                        format!("路径里有 Windows 命令行不能安全传递的字符：{s}"),
                        "请把 AgentRouter 的数据文件夹（AGENTROUTER_DEVICE_HOME）放在不含 \" % ! ^ 的路径下。",
                    ));
                }
            }
            let line = parts
                .iter()
                .map(|p| format!("\"{}\"", shown(p)))
                .collect::<Vec<_>>()
                .join(" ");
            cmd.arg("-c").arg(line);
        } else {
            cmd.arg("--").arg(launch.program).args(launch.args);
        }
        #[cfg(not(windows))]
        let _ = stdio_pipe;
        cmd.current_dir(launch.cwd);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        Ok(Confined { command: cmd, pipe })
    }
}

/// `{name, exe}` of the launch file's `job` (device #129): the job's name and the device program, which the entry runs
/// as `<exe> sandbox job-join <name> <its pid>` (it must answer `joined`).
#[cfg(windows)]
fn windows_job_spec(job: &JobJoin) -> Result<Value, Unavailable> {
    let fail = |e: String| {
        Unavailable::new(
            format!("资源上限的作业对象接不上隔离环境：{e}"),
            "请重新运行 agentrouter；仍然不行就在 agentrouter-device 仓库报告这一行（device #129）。",
        )
    };
    let name = job.name;
    if !name.starts_with("Local\\")
        || !name[6..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(fail(format!("作业对象的名字不对：{name}")));
    }
    if !job.program.is_file() {
        return Err(fail(format!("找不到 {}", shown(job.program))));
    }
    Ok(json!({"name": name, "exe": shown(job.program)}))
}

/// A new [`StdioPipe`] for one server: its name and the server end. The DACL is protected (nothing inherited) and has
/// one entry: read and write data and attributes for the sandbox account (`FILE_GENERIC_READ | FILE_GENERIC_WRITE`,
/// what a client's `GENERIC_READ | GENERIC_WRITE` open asks for; another instance cannot be made anyway, as the pipe
/// allows one). The device itself needs no entry: it holds the server end from the creation. The low mandatory label
/// keeps the open working should srt ever run the child below medium integrity.
#[cfg(windows)]
fn windows_stdio_pipe(
    sandbox_sid: &str,
) -> Result<(String, std::os::windows::io::OwnedHandle), String> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    if !sandbox_sid.starts_with("S-1-")
        || !sandbox_sid[4..]
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
    {
        return Err(format!("隔离账户的 SID 不对：{sandbox_sid}"));
    }
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let sddl = wide(&format!("D:P(A;;0x12019f;;;{sandbox_sid})S:(ML;;NW;;;LW)"));
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; on success `sd` is a LocalAlloc'd descriptor freed below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(format!("安全描述符：{}", std::io::Error::last_os_error()));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let random: [u8; 16] = random();
    let name = format!(
        r"\\.\pipe\agentrouter-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let wide_name = wide(&name);
    // SAFETY: the name is NUL-terminated and `attributes` (with `sd`) lives across the call.
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: `sd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW and is not used again.
    unsafe { LocalFree(sd) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(format!("CreateNamedPipeW：{error}"));
    }
    // SAFETY: a valid handle that nothing else owns.
    Ok((name, unsafe { OwnedHandle::from_raw_handle(handle) }))
}

/// A child's output, or None (killed) after `timeout`.
#[cfg(not(windows))]
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system's random source failed");
    out
}

/// [`PROFILE_ENV`] on macOS and Linux, where `HOME` inside the sandbox is the server's scratch folder: each variable
/// that `var` (the device's own environment) does not set already, pointing at the person's file or folder under `home`
/// when it exists — the places each tool reads by default.
#[cfg_attr(windows, allow(dead_code))]
fn profile_env_of(
    home: &Path,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(String, String)> {
    let xdg = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let file = |c: &[PathBuf]| c.iter().find(|p| p.is_file()).map(|p| shown(p));
    let dir = |p: PathBuf| p.is_dir().then(|| shown(&p));
    let mut pip = vec![
        xdg.join("pip").join("pip.conf"),
        home.join(".pip").join("pip.conf"),
    ];
    if cfg!(target_os = "macos") {
        pip.insert(0, home.join("Library/Application Support/pip/pip.conf"));
    }
    let m2 = home.join(".m2");
    let maven = file(&[m2.join("settings.xml")])
        .filter(|s| !s.contains(char::is_whitespace))
        .map(|s| {
            let security = m2.join("settings-security.xml");
            match file(&[security]).filter(|t| !t.contains(char::is_whitespace)) {
                Some(t) => format!("-s {s} -Dsettings.security={t}"),
                None => format!("-s {s}"),
            }
        });
    [
        ("NPM_CONFIG_USERCONFIG", file(&[home.join(".npmrc")])),
        (
            "GIT_CONFIG_GLOBAL",
            file(&[home.join(".gitconfig"), xdg.join("git").join("config")]),
        ),
        ("PIP_CONFIG_FILE", file(&pip)),
        ("MAVEN_ARGS", maven),
        ("RUSTUP_HOME", dir(home.join(".rustup"))),
        ("CARGO_HOME", dir(home.join(".cargo"))),
    ]
    .into_iter()
    .filter(|(k, _)| var(k).is_none())
    .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
    .collect()
}

/// The person's secrets (§6.1 read deny list) under `home`, for this OS.
fn secret_paths(home: &Path) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = [
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker/config.json",
        ".config/gh",
        ".git-credentials",
        ".npmrc",
        ".pypirc",
        ".netrc",
        ".config/gcloud",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect();
    if cfg!(target_os = "macos") {
        for p in [
            "Library/Keychains",
            "Library/Cookies",
            "Library/Safari",
            "Library/Application Support/Google/Chrome",
            "Library/Application Support/Microsoft Edge",
            "Library/Application Support/Firefox",
            "Library/Application Support/BraveSoftware",
        ] {
            list.push(home.join(p));
        }
    } else if cfg!(target_os = "linux") {
        for p in [
            ".mozilla",
            ".config/google-chrome",
            ".config/chromium",
            ".config/microsoft-edge",
            ".config/BraveSoftware",
            ".local/share/keyrings",
            ".password-store",
        ] {
            list.push(home.join(p));
        }
    }
    // Windows: the sandbox account has no access to the person's profile in the first place (browser profiles,
    // DPAPI keys and the credential store included); the list above still covers a linked folder that holds them.
    list
}

fn valid_domain(d: &str) -> bool {
    (1..=253).contains(&d.len())
        && d.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '*' | ':'))
        && !d.starts_with('.')
        && d != "*"
}

/// A path as srt and the person read it: no `\\?\` prefix, no `.`/`..`.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let mut out = PathBuf::new();
    for c in Path::new(stripped).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn shown(path: &Path) -> String {
    plain(path).to_string_lossy().into_owned()
}

fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    plain(&std::env::var_os(var).map(PathBuf::from).unwrap_or_default())
}

/// A program on PATH (and beside `near`, for npm next to node), with Windows extensions.
pub fn find_program(name: &str, near: Option<&Path>) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let mut dirs: Vec<PathBuf> = near
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if !cfg!(windows) {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
    }
    dirs.iter().find_map(|d| {
        exts.iter()
            .map(|e| d.join(format!("{name}{e}")))
            .find(|c| c.is_file())
    })
}

/// What `srt-win status` says about this machine's sandbox install (no elevation needed).
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
struct WindowsInstall {
    /// The `srt-sandbox` account's SID, when the account and its credential exist.
    sid: Option<String>,
    /// Set up by another srt release (its setup version differs from this one's): srt-win refuses to run on it until
    /// `srt-win install` runs again.
    stale: bool,
    /// The fence's loopback mode (`wfp.allow_loopback`, srt README "Local servers on Windows"), when `srt-win status`
    /// could read the filter set (it may not without elevation): `Some(true)` = loopback open to the sandbox.
    loopback: Option<bool>,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_install_of(v: &Value) -> Result<WindowsInstall, String> {
    let user = &v["user"];
    let loopback = (v["wfp"]["state"] == "installed")
        .then(|| v["wfp"]["allow_loopback"].as_bool())
        .flatten();
    if user["user"]["exists"] != true || user["cred_present"] != true {
        return Ok(WindowsInstall {
            sid: None,
            stale: false,
            loopback: None,
        });
    }
    let sid = user["user"]["sid"]
        .as_str()
        .ok_or("srt-win status 没给出隔离账户的 SID")?
        .to_string();
    let stale = match (
        user["setup_version"].as_u64(),
        user["marker_version"].as_u64(),
    ) {
        (Some(want), have) => have != Some(want),
        (None, _) => false,
    };
    Ok(WindowsInstall {
        sid: Some(sid),
        stale,
        loopback,
    })
}

#[cfg(windows)]
fn windows_install(srt_win: &Path) -> Result<WindowsInstall, String> {
    if !srt_win.is_file() {
        return Ok(WindowsInstall {
            sid: None,
            stale: false,
            loopback: None,
        });
    }
    windows_install_of(&srt_win_json(srt_win, &["status"])?)
}

/// `srt-win profile status`'s `state` (`current` is the only one a session can rely on) and, when current, the
/// variables that point the person's tools at their own configuration files ([`PROFILE_ENV`]).
#[cfg(windows)]
fn windows_profile_state(srt_win: &Path) -> Result<(String, Vec<(String, String)>), String> {
    Ok(windows_profile_of(&srt_win_json(
        srt_win,
        &["profile", "status"],
    )?))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_profile_of(v: &Value) -> (String, Vec<(String, String)>) {
    let state = v["state"].as_str().unwrap_or("unknown").to_string();
    let env = if state == "current" {
        PROFILE_ENV
            .iter()
            .filter_map(|name| {
                v["env"][*name]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(|value| (name.to_string(), value.to_string()))
            })
            .collect()
    } else {
        Vec::new()
    };
    (state, env)
}

#[cfg(windows)]
fn srt_win_json(srt_win: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new(srt_win)
        .args(args)
        .output()
        .map_err(|e| format!("srt-win 起不来：{e}"))?;
    serde_json::from_slice(&out.stdout).map_err(|_| {
        format!(
            "srt-win {} 看不懂：{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })
}

/// `icacls <dir> /grant:r *<sid>:(OI)(CI)RX /Q`: replace this SID's explicit grant on `dir` with read+execute,
/// inherited by everything below. Only a well-formed SID is passed on.
#[cfg_attr(not(windows), allow(dead_code))]
fn icacls_read_grant(dir: &Path, sid: &str) -> Result<Vec<std::ffi::OsString>, String> {
    let well_formed = sid.starts_with("S-1-")
        && sid[4..]
            .split('-')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !well_formed {
        return Err(format!("隔离账户的 SID 格式不对：{sid}"));
    }
    Ok(vec![
        dir.as_os_str().to_os_string(),
        "/grant:r".into(),
        format!("*{sid}:(OI)(CI)RX").into(),
        "/Q".into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_install_of_another_srt_release_is_stale() {
        let status = |marker: Value| {
            json!({"user": {"user": {"exists": true, "sid": "S-1-5-21-1-2-3-1001"}, "cred_present": true,
                "setup_version": 7, "marker_version": marker}})
        };
        let current = windows_install_of(&status(json!(7))).unwrap();
        assert_eq!(current.sid.as_deref(), Some("S-1-5-21-1-2-3-1001"));
        assert!(!current.stale);
        assert!(windows_install_of(&status(json!(6))).unwrap().stale);
        assert!(windows_install_of(&status(Value::Null)).unwrap().stale);
        let none = windows_install_of(
            &json!({"user": {"user": {"exists": false}, "cred_present": false}}),
        )
        .unwrap();
        assert_eq!(
            none,
            WindowsInstall {
                sid: None,
                stale: false,
                loopback: None,
            }
        );
        assert!(
            windows_install_of(&json!({"user": {"user": {"exists": true}, "cred_present": true}}))
                .is_err()
        );
    }

    #[test]
    fn the_loopback_mode_is_read_only_from_an_installed_fence() {
        let status = |wfp: Value| {
            json!({"user": {"user": {"exists": true, "sid": "S-1-5-21-1-2-3-1001"}, "cred_present": true},
                "wfp": wfp})
        };
        let mode = |wfp: Value| windows_install_of(&status(wfp)).unwrap().loopback;
        assert_eq!(
            mode(json!({"state": "installed", "allow_loopback": true})),
            Some(true)
        );
        assert_eq!(
            mode(json!({"state": "installed", "allow_loopback": false})),
            Some(false)
        );
        // Without elevation the filter set may be unreadable: unknown, never a guess.
        assert_eq!(mode(json!({"state": "cannot-read"})), None);
        assert_eq!(mode(Value::Null), None);
    }

    #[test]
    fn the_profile_env_is_srt_wins_list_and_only_when_the_grant_is_current() {
        let (state, env) = windows_profile_of(&json!({"state": "current", "env": {
            "GIT_CONFIG_GLOBAL": "C:\\Users\\p\\.gitconfig", "CARGO_HOME": "", "PATH": "C:\\evil"}}));
        assert_eq!(state, "current");
        assert_eq!(
            env,
            vec![(
                "GIT_CONFIG_GLOBAL".to_string(),
                "C:\\Users\\p\\.gitconfig".to_string()
            )]
        );
        let (state, env) =
            windows_profile_of(&json!({"state": "outdated", "env": {"GIT_CONFIG_GLOBAL": "x"}}));
        assert_eq!(state, "outdated");
        assert!(env.is_empty());
    }

    #[test]
    fn the_person_s_tool_config_is_found_under_their_home() {
        let home = temp("profile-home");
        std::fs::create_dir_all(home.join(".config").join("git")).unwrap();
        std::fs::write(
            home.join(".config").join("git").join("config"),
            "[user]\nname = P\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".rustup")).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        let none = |_: &str| None;
        let env = profile_env_of(&home, none);
        let get = |env: &[(String, String)], k: &str| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        // Only the XDG file exists: it is the global config (git reads it when ~/.gitconfig is missing).
        assert_eq!(
            get(&env, "GIT_CONFIG_GLOBAL"),
            Some(shown(&home.join(".config").join("git").join("config")))
        );
        assert_eq!(get(&env, "RUSTUP_HOME"), Some(shown(&home.join(".rustup"))));
        assert_eq!(get(&env, "CARGO_HOME"), Some(shown(&home.join(".cargo"))));
        assert_eq!(get(&env, "NPM_CONFIG_USERCONFIG"), None);
        // ~/.gitconfig wins over the XDG file, as for git with both present.
        std::fs::write(home.join(".gitconfig"), "[user]\nname = Q\n").unwrap();
        assert_eq!(
            get(&profile_env_of(&home, none), "GIT_CONFIG_GLOBAL"),
            Some(shown(&home.join(".gitconfig")))
        );
        // What the device's environment sets already is left alone.
        let set = |k: &str| (k == "CARGO_HOME").then(|| std::ffi::OsString::from("/elsewhere"));
        assert_eq!(get(&profile_env_of(&home, set), "CARGO_HOME"), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn local_ports_are_open_on_macos_and_windows_only() {
        let dir = temp("loopback");
        let (rt, folder) = launch_in(&dir);
        let folders = vec![folder.clone()];
        let launch = Launch {
            key: "host\u{1}work",
            program: Path::new("node"),
            args: &[],
            env: &[],
            cwd: &folder,
            folders: &folders,
            network: &[],
            readable: &[],
            read_only: false,
            open_web: false,
            job: None,
        };
        for open in [true, false] {
            let p = rt.policy_with(&launch, open).unwrap();
            let got = &p["network"]["allowLocalBinding"];
            if cfg!(target_os = "linux") {
                assert!(got.is_null(), "Linux: the sandbox's own loopback");
            } else {
                assert_eq!(got, &json!(open));
            }
        }
        if !cfg!(windows) {
            assert_eq!(rt.local_binding(), cfg!(target_os = "macos"));
        }
        // macOS: file watchers reach FSEvents (device #141) and commands open terminals; nothing extra elsewhere.
        let p = rt.policy_with(&launch, false).unwrap();
        let mach = &p["network"]["allowMachLookup"];
        if cfg!(target_os = "macos") {
            assert_eq!(mach, &json!(["com.apple.FSEvents"]));
            assert_eq!(p["allowPty"], json!(true));
        } else {
            assert!(mach.is_null() && p["allowPty"].is_null(), "{p}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_runtime_read_grant_names_one_sid_and_inherits() {
        let args = icacls_read_grant(
            Path::new("C:\\data\\sandbox\\srt-0.0.81"),
            "S-1-5-21-1-2-3-1001",
        )
        .unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "C:\\data\\sandbox\\srt-0.0.81",
                "/grant:r",
                "*S-1-5-21-1-2-3-1001:(OI)(CI)RX",
                "/Q"
            ]
        );
        for bad in [
            "",
            "S-1-",
            "S-1-5-21-1-x",
            "S-1-5-21-1-2 /T",
            "Everyone",
            "S-1-5--1",
            "S-1-5-21-1-2-3-1001:F",
        ] {
            assert!(icacls_read_grant(Path::new("C:\\d"), bad).is_err(), "{bad}");
        }
    }

    fn launch_in(dir: &Path) -> (Runtime, PathBuf) {
        let data = dir.join("data");
        let folder = dir.join("work");
        std::fs::create_dir_all(folder.join(".git").join("hooks")).unwrap();
        std::fs::write(folder.join(".git").join("config"), "").unwrap();
        (Runtime::new(&data), folder)
    }

    fn temp(name: &str) -> PathBuf {
        // Under the build's own target folder (never the system temp one, docs/AV-HYGIENE.md): the unit-test
        // binary is `<target>/debug/deps/<name>`.
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("target folder");
        let dir = target.join("tmp").join(format!(
            "agentrouter-sandbox-unit-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_policy_writes_only_the_folder_and_scratch() {
        let dir = temp("policy");
        let (rt, folder) = launch_in(&dir);
        let folders = vec![folder.clone()];
        let network = vec!["api.example.com".to_string(), "bad domain".to_string()];
        let launch = Launch {
            key: "files\u{1}work",
            program: Path::new("node"),
            args: &[],
            env: &[],
            cwd: &folder,
            folders: &folders,
            network: &network,
            readable: &[],
            read_only: false,
            open_web: false,
            job: None,
        };
        let p = rt.policy(&launch).unwrap();
        let writes: Vec<&str> = p["filesystem"]["allowWrite"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(writes, vec![shown(&folder), shown(&rt.scratch(launch.key))]);
        let deny: Vec<&Value> = p["filesystem"]["denyWrite"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(deny.len(), 2, "hooks and config are never writable");
        let domains = p["network"]["allowedDomains"].as_array().unwrap();
        assert!(domains.iter().any(|d| d == "registry.npmjs.org"));
        assert!(domains.iter().any(|d| d == "api.example.com"));
        assert!(!domains.iter().any(|d| d == "bad domain"));
        assert!(!rt.scratch(launch.key).starts_with(&rt.data_dir));
        // Linux: srt runs the device's own ripgrep, socat and bubblewrap (#119, #122), never whatever is on PATH;
        // elsewhere none. Where AppArmor restricts user namespaces, bubblewrap is the root-owned copy its profile names.
        let runtimes = shown(&rt.runtimes_dir());
        for (got, program) in [
            (&p["ripgrep"]["command"], "rg"),
            (&p["socatPath"], "socat"),
            (&p["bwrapPath"], "bwrap"),
        ] {
            if cfg!(target_os = "linux") {
                let got = got.as_str().unwrap();
                let base = if program == "bwrap" && userns::restricted() {
                    userns::ROOT.to_string()
                } else {
                    runtimes.clone()
                };
                assert!(got.starts_with(&base) && got.ends_with(program), "{got}");
            } else {
                assert!(got.is_null());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_namespaces_the_sandbox_cannot_get_name_an_admin_line() {
        let missing = userns_needs_admin(Some("setgroups: Permission denied"));
        if userns::restricted() {
            assert_eq!(missing.code, NEEDS_ADMIN);
            assert!(
                missing.next.contains(&userns::admin_line("install")),
                "{}",
                missing.next
            );
            assert!(
                missing.next.contains(&userns::admin_line("remove")),
                "{}",
                missing.next
            );
            assert!(
                !missing
                    .next
                    .contains("apparmor_restrict_unprivileged_userns=0")
            );
        } else if userns::kernel_line().is_some() {
            assert_eq!(missing.code, NEEDS_ADMIN);
        } else {
            assert_eq!(missing.code, UNAVAILABLE);
        }
        assert!(missing.message.contains("setgroups: Permission denied"));
        assert!(
            !Unavailable::needs_admin("x", "y").setup_helps(),
            "sandbox setup cannot change a system setting"
        );
        assert!(Unavailable::new("x", NEXT_SETUP).setup_helps());
    }

    #[test]
    fn read_only_writes_nothing_but_scratch() {
        let dir = temp("readonly");
        let (rt, folder) = launch_in(&dir);
        let folders = vec![folder.clone()];
        let launch = Launch {
            key: "files",
            program: Path::new("node"),
            args: &[],
            env: &[],
            cwd: &folder,
            folders: &folders,
            network: &[],
            readable: &[],
            read_only: true,
            open_web: false,
            job: None,
        };
        let p = rt.policy(&launch).unwrap();
        assert_eq!(
            p["filesystem"]["allowWrite"],
            json!([shown(&rt.scratch("files"))])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_open_web_keeps_private_ranges_out_and_never_lists_a_bare_star() {
        let dir = temp("openweb");
        let (rt, folder) = launch_in(&dir);
        let folders = vec![folder.clone()];
        let network = vec![OPEN_WEB.to_string(), "playwright.dev".to_string()];
        let mut launch = Launch {
            key: "browser",
            program: Path::new("node"),
            args: &[],
            env: &[],
            cwd: &folder,
            folders: &folders,
            network: &network,
            readable: &[],
            read_only: false,
            open_web: false,
            job: None,
        };
        for open_web in [false, true] {
            launch.open_web = open_web;
            let p = rt.policy(&launch).unwrap();
            let domains = p["network"]["allowedDomains"].as_array().unwrap();
            assert!(
                !domains.iter().any(|d| d == OPEN_WEB),
                "srt refuses a bare *"
            );
            assert!(domains.iter().any(|d| d == "playwright.dev"));
            let denied = &p["network"]["deniedResolvedAddresses"];
            if open_web {
                assert_eq!(*denied, json!(PRIVATE_RANGES));
            } else {
                assert!(denied.is_null(), "{p}");
            }
        }
        for lan in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"] {
            assert!(PRIVATE_RANGES.contains(&lan));
        }
        // The launcher is srt's own CLI with the callback, beside srt's node_modules.
        assert!(OPEN_WEB_JS.contains("'./node_modules/@anthropic-ai/sandbox-runtime/dist/'"));
        assert!(OPEN_WEB_JS.contains("import(pkg + 'cli.js')"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn home_root_and_the_data_folder_cannot_be_the_folder() {
        let dir = temp("refuse");
        let (rt, _) = launch_in(&dir);
        for bad in [
            home_dir(),
            PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" }),
            dir.clone(),
        ] {
            let folders = vec![bad.clone()];
            let launch = Launch {
                key: "x",
                program: Path::new("node"),
                args: &[],
                env: &[],
                cwd: &bad,
                folders: &folders,
                network: &[],
                readable: &[],
                read_only: false,
                open_web: false,
                job: None,
            };
            assert!(
                rt.policy(&launch).is_err(),
                "{} must be refused",
                bad.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scratch_env_points_caches_into_scratch() {
        let rt = Runtime::new(Path::new("/data/agentrouter"));
        let s = rt.scratch("k");
        for (k, v) in rt.scratch_env("k") {
            assert!(Path::new(&v).starts_with(&s), "{k}={v}");
        }
        assert_ne!(rt.scratch("a"), rt.scratch("b"));
    }

    #[test]
    fn the_pinned_lockfile_carries_integrity_for_every_package() {
        let lock: Value = serde_json::from_str(PACKAGE_LOCK).unwrap();
        let packages = lock["packages"].as_object().unwrap();
        let srt = &packages[&format!("node_modules/{SRT_PACKAGE}")];
        assert_eq!(srt["version"], SRT_VERSION);
        for (name, p) in packages.iter().filter(|(n, _)| !n.is_empty()) {
            let integrity = p["integrity"].as_str().unwrap_or("");
            assert!(
                integrity.starts_with("sha512-"),
                "{name} has no sha512 integrity"
            );
        }
        let manifest: Value = serde_json::from_str(PACKAGE_JSON).unwrap();
        assert_eq!(
            manifest["dependencies"][SRT_PACKAGE], SRT_VERSION,
            "exact pin, no range"
        );
    }
}
