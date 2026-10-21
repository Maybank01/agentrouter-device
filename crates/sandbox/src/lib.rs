//! The sandbox launcher (docs/HOSTD-MCP.md §6, issue #42): every local MCP server process — and everything it
//! starts — runs inside an OS sandbox whose only writable places are the linked folder(s) and a scratch folder of
//! its own. A write, rename or delete anywhere else fails in the kernel, whatever the server or a command it runs
//! tries (2026-10-18: home dot-folders were deleted on the owner's PC by an unconfined process; #39).
//!
//! Backend: Anthropic `sandbox-runtime` (`srt`, Apache-2.0), pinned to [`SRT_VERSION`] with an npm lockfile that
//! carries the integrity of every package (`runtime/`), installed with `npm ci` into the device data folder and used
//! as a process wrapper (`node cli.js --settings <policy> -- <program> <args>`):
//!
//! - macOS: Seatbelt (`sandbox-exec`) profile; no setup.
//! - Linux: bubblewrap + a network namespace (needs `bwrap`, `socat`, `rg` and unprivileged user namespaces).
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
//! srt's proxy, or the open web for a catalog preset whose list is [`OPEN_WEB`] ([`Launch::open_web`]). There is no
//! unconfined fallback: when the sandbox cannot run, [`Unavailable`] says what to do.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
/// srt needs Node 22.12 or later.
pub const NODE_MIN: (u32, u32) = (22, 12);
/// The loopback ports srt's proxies may use on Windows (the WFP fence permits only these). srt's default is ten
/// ports (two per server) and srt-win accepts at most 65; 64 ports cover far more servers than one link runs.
pub const WINDOWS_PROXY_PORTS: (u16, u16) = (60080, 60143);
/// The error code a call gets when its server cannot be confined (never run unconfined instead).
pub const UNAVAILABLE: &str = "SANDBOX_UNAVAILABLE";

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
    pub message: String,
    pub next: String,
}

impl Unavailable {
    fn new(message: impl Into<String>, next: impl Into<String>) -> Unavailable {
        Unavailable {
            message: message.into(),
            next: next.into(),
        }
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.message, self.next)
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
        let node = node()?;
        let npm = find_program("npm", Some(&node))
            .ok_or_else(|| "找不到 npm（随 Node.js 一起装）".to_string())?;
        let dir = self.dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| format!("清不掉旧的 {}：{e}", shown(&dir)))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("建不了 {}：{e}", shown(&dir)))?;
        std::fs::write(dir.join("package.json"), PACKAGE_JSON).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("package-lock.json"), PACKAGE_LOCK).map_err(|e| e.to_string())?;
        let out = Command::new(&npm)
            .args([
                "ci",
                "--ignore-scripts",
                "--omit=dev",
                "--no-audit",
                "--no-fund",
                "--loglevel=error",
            ])
            .current_dir(&dir)
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

    /// `agentrouter sandbox setup`: install the runtime when it is missing and, on Windows, provision the sandbox
    /// account and its network fence (one UAC prompt, owner Q4). Returns what was done.
    pub fn setup(&self) -> Result<Vec<String>, String> {
        let mut done = Vec::new();
        if !self.installed() {
            self.install()?;
            done.push(format!("装好了 {SRT_PACKAGE}@{SRT_VERSION}"));
        }
        #[cfg(windows)]
        {
            let before = windows_install(&self.srt_win())?;
            if before.sid.is_none() || before.stale {
                let range = format!("{}-{}", WINDOWS_PROXY_PORTS.0, WINDOWS_PROXY_PORTS.1);
                let out = Command::new(self.srt_win())
                    .args(["install", "--proxy-port-range", &range])
                    .output()
                    .map_err(|e| format!("srt-win 起不来：{e}"))?;
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
                done.push(if before.stale {
                    "更新了 Windows 隔离账户和它的网络限制".into()
                } else {
                    "建好了 Windows 隔离账户 srt-sandbox 和它的网络限制".into()
                });
            }
            let after = windows_install(&self.srt_win())?;
            let sid = after
                .sid
                .filter(|_| !after.stale)
                .ok_or_else(|| "隔离账户没建成（srt-win status 仍然没有它）".to_string())?;
            self.windows_runtime_readable(&sid)?;
            // Also a refresh: what was created in the profile since the last run becomes readable.
            let first = windows_profile_state(&self.srt_win())? != "current";
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
        if self.root().exists() {
            std::fs::remove_dir_all(self.root()).map_err(|e| e.to_string())?;
            done.push(format!("删掉了 {}", shown(&self.root())));
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
        let problems = match self.ready() {
            Ok(_) => Vec::new(),
            Err(e) => vec![e],
        };
        Status {
            backend: Runtime::backend(),
            mechanism,
            ready: problems.is_empty(),
            problems,
        }
    }

    fn ready(&self) -> Result<Ready, Unavailable> {
        let node = node().map_err(|m| {
            Unavailable::new(
                m,
                "请用户从 https://nodejs.org 装上 Node.js 22.12 或更新的 LTS 版本，然后运行 agentrouter sandbox setup。",
            )
        })?;
        if !self.installed() {
            return Err(Unavailable::new(
                format!("隔离环境（{}）还没装", Runtime::backend()),
                NEXT_SETUP,
            ));
        }
        #[cfg(target_os = "linux")]
        {
            let missing: Vec<&str> = ["bwrap", "socat", "rg"]
                .into_iter()
                .filter(|p| find_program(p, None).is_none())
                .collect();
            if !missing.is_empty() {
                return Err(Unavailable::new(
                    format!("这台电脑缺少隔离要用的程序：{}", missing.join("、")),
                    "请用户运行 sudo apt-get install bubblewrap socat ripgrep（其他发行版装同名的包）；Ubuntu 24.04 以后还要允许非特权用户命名空间：sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0。",
                ));
            }
        }
        #[cfg(not(windows))]
        self.probe(&node)?;
        #[cfg(windows)]
        {
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
            match windows_profile_state(&self.srt_win()) {
                Ok(state) if state == "current" => {}
                Ok(state) => {
                    return Err(Unavailable::new(
                        format!("隔离账户还不能读你的用户文件夹（{state}）"),
                        NEXT_SETUP,
                    ));
                }
                Err(e) => return Err(Unavailable::new(e, NEXT_SETUP)),
            }
        }
        Ok(Ready {
            node,
            cli: self.cli(),
        })
    }

    /// The sandbox account may read and run the installed runtime (`srt-win.exe` is started as that account; see the
    /// module docs, #66). One inheriting read+execute ACE for the account's SID on the runtime folder, set with the
    /// person's own rights (they own the data folder; no elevation); once per process and folder, as `icacls`
    /// rewrites the whole tree. The data folder above it stays unreadable to the account.
    #[cfg(windows)]
    fn windows_runtime_readable(&self, sid: &str) -> Result<(), String> {
        static DONE: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());
        let dir = self.dir();
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
                let policy = json!({
                    "network": {"allowedDomains": [], "deniedDomains": []},
                    "filesystem": {"denyRead": [], "allowRead": [shown(&self.package_dir())], "allowWrite": [shown(&scratch)], "denyWrite": []},
                });
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
                            Err(Unavailable::new(
                                format!("这台电脑不允许隔离环境建用户命名空间（{clipped}）"),
                                "请用户运行 sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0（长期生效：写进 /etc/sysctl.d/），或者在信任的情况下用 agentrouter link --full。",
                            ))
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
            .chain(std::iter::once(&self.package_dir()))
            .filter(|p| p.exists())
            .map(|p| shown(&plain(p)))
            .collect();
        if cfg!(windows) {
            // Per-user runtimes (nvm/fnm Node, npm's global folder, uv) created after the last profile read grant are
            // not readable through it yet: read+execute grants for this session, removed again when the server ends.
            let mut extra = vec![launch.program.parent().map(Path::to_path_buf)];
            if let Ok(n) = node() {
                extra.push(n.parent().map(Path::to_path_buf));
            }
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
        Ok(policy)
    }

    /// The confined command for one server: stdio, cwd and environment are set; the caller pipes stdio and spawns.
    pub fn command(&self, launch: &Launch) -> Result<Command, Unavailable> {
        let ready = self.ready()?;
        let policy = self.policy(launch)?;
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
        // With HOME in scratch, git still finds the person's identity (read-only).
        let gitconfig = home_dir().join(".gitconfig");
        if !cfg!(windows)
            && gitconfig.is_file()
            && !env.iter().any(|(k, _)| k == "GIT_CONFIG_GLOBAL")
        {
            env.push(("GIT_CONFIG_GLOBAL".to_string(), shown(&gitconfig)));
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
            let spec = json!({
                "program": shown(launch.program),
                "args": launch.args,
                "env": env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect::<serde_json::Map<String, Value>>(),
                "cwd": shown(launch.cwd),
            });
            std::fs::write(&launch_file, spec.to_string())
                .map_err(|e| Unavailable::new(format!("写不了启动文件：{e}"), NEXT_SETUP))?;
            // `cmd /d /s /c "<line>"` inside the sandbox: quoted paths only, nothing cmd would expand.
            let parts = [ready.node.clone(), entry, launch_file];
            for p in &parts {
                let s = shown(p);
                if s.contains(['"', '%', '!', '^', '\r', '\n']) {
                    return Err(Unavailable::new(
                        format!("路径里有 Windows 命令行不能安全传递的字符：{s}"),
                        "请把 Node.js 和 AgentRouter 的数据文件夹放在不含 \" % ! ^ 的路径下。",
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
        cmd.current_dir(launch.cwd);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        Ok(cmd)
    }
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
    if cfg!(windows) {
        dirs.push(PathBuf::from(r"C:\Program Files\nodejs"));
    } else {
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

/// Node 22.12+ (checked once per process).
fn node() -> Result<PathBuf, String> {
    static NODE: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    NODE.get_or_init(|| {
        let node = find_program("node", None).ok_or("这台电脑没有装 Node.js（隔离环境要用）")?;
        let out = Command::new(&node)
            .arg("--version")
            .output()
            .map_err(|e| format!("Node.js 起不来：{e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut parts = text
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u32>().unwrap_or(0));
        let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        if version < NODE_MIN {
            return Err(format!(
                "Node.js 是 {text}，隔离环境要 {}.{} 或更新",
                NODE_MIN.0, NODE_MIN.1
            ));
        }
        Ok(node)
    })
    .clone()
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
}

#[cfg_attr(not(windows), allow(dead_code))]
fn windows_install_of(v: &Value) -> Result<WindowsInstall, String> {
    let user = &v["user"];
    if user["user"]["exists"] != true || user["cred_present"] != true {
        return Ok(WindowsInstall {
            sid: None,
            stale: false,
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
    })
}

#[cfg(windows)]
fn windows_install(srt_win: &Path) -> Result<WindowsInstall, String> {
    if !srt_win.is_file() {
        return Ok(WindowsInstall {
            sid: None,
            stale: false,
        });
    }
    windows_install_of(&srt_win_json(srt_win, &["status"])?)
}

/// `srt-win profile status`'s `state`: `current` is the only one a session can rely on.
#[cfg(windows)]
fn windows_profile_state(srt_win: &Path) -> Result<String, String> {
    let v = srt_win_json(srt_win, &["profile", "status"])?;
    Ok(v["state"].as_str().unwrap_or("unknown").to_string())
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
                stale: false
            }
        );
        assert!(
            windows_install_of(&json!({"user": {"user": {"exists": true}, "cred_present": true}}))
                .is_err()
        );
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
        let _ = std::fs::remove_dir_all(&dir);
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
