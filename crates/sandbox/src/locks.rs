//! Catalog locks (Cloud #2740): a pinned catalog MCP server is installed from the lock of everything below it, not
//! resolved by `npx`/`uvx` at every start. The cloud sends the lock with `link__server_add` (`lock`, in the signed
//! request body, never on a command line); the device installs it once into the shared packages folder and runs the
//! installed program.
//!
//! - **Standard tools, every file checked:** npm → `npm ci --omit=dev --ignore-scripts` (npm checks every package's
//!   `integrity` against `package-lock.json`); Python → `uv venv` on a uv-managed Python, then
//!   `uv pip sync --require-hashes` (every wheel's sha256 against `requirements.txt`), wheels only except the names the
//!   entry lists in `build`. No package code runs while installing. A file that does not match is
//!   `MCP_DIGEST_MISMATCH`; nothing is installed from it.
//! - **Sources (owner rule 2026-10-22):** the order the cloud sends (`indexes`): the official registry first, then its
//!   mirrors, each as one whole attempt with that source in the environment (npm swaps the registry host of every
//!   `resolved` URL; uv takes `UV_DEFAULT_INDEX`). The checksums make any source safe. Where an official host already
//!   proved slow or unreachable on this computer ([`crate::runtimes::prefer_mirrors`]), the mirrors go first; an
//!   official source that fails or runs past [`OFFICIAL_BOUND`] sets that signal. The managed Python comes from uv's
//!   own host first, then [`PYTHON_MIRROR`] (uv checks it against the SHA-256 built into uv either way). When every
//!   source fails the answer is `MCP_INSTALL_FAILED`, naming each source and what it said.
//! - **Where:** `<packages>/locks/<key>/` ([`crate::Runtime::packages_dir`], readable to every sandbox), `<key>` a
//!   digest of the lock; the managed Python under `<packages>/python`. A folder counts only with its marker, written
//!   last; a half-written one is removed and installed again. Installs on this computer run one at a time.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Runtime, Unavailable, runtimes};

/// A package's file did not match the lock's checksum (Cloud error catalog).
pub const DIGEST_MISMATCH: &str = "MCP_DIGEST_MISMATCH";
/// No source delivered the locked packages (Cloud error catalog).
pub const INSTALL_FAILED: &str = "MCP_INSTALL_FAILED";
/// npmmirror's copy of the python-build-standalone releases uv installs (measured 14.7 MB/s from China, Cloud #2716).
pub const PYTHON_MIRROR: &str = "https://registry.npmmirror.com/-/binary/python-build-standalone";
/// An attempt from a source that is not the last one gives up after this long (the next source is tried).
pub const OFFICIAL_BOUND: Duration = Duration::from_secs(180);
/// Written last into an installed lock folder: its key, then where it came from.
const MARKER: &str = ".agentrouter-lock";
const FILE_MAX: usize = 1 << 20;
const FILES_MAX: usize = 2 << 20;

static INSTALLING: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Npm,
    Uv,
}

impl Kind {
    fn files(self) -> &'static [&'static str] {
        match self {
            Kind::Npm => &["package.json", "package-lock.json"],
            Kind::Uv => &["requirements.txt", ".python-version"],
        }
    }
}

/// What `link__server_add` carries for a pinned catalog entry (`lock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    pub kind: Kind,
    /// File name → content, the names of [`Kind::files`] exactly.
    pub files: Vec<(String, String)>,
    /// Package sources in order (registry roots for npm, simple indexes for uv).
    pub indexes: Vec<String>,
    pub package: String,
    pub version: String,
    /// The program in the package the server runs.
    pub bin: String,
    /// The server's own arguments (`{{folders}}`/`{{folder}}` filled in by the host).
    pub args: Vec<String>,
    /// Python only: the names allowed to build from a source file (everything else wheels only).
    pub build: Vec<String>,
}

/// An installed lock: what to run.
#[derive(Debug)]
pub struct Installed {
    pub program: PathBuf,
    /// Before the server's own arguments (the script, for npm: `node <script>`).
    pub args: Vec<String>,
    /// The source it was installed from now; `None` when it was already there.
    pub from: Option<String>,
    pub took: Duration,
}

fn invalid(message: String) -> String {
    message
}

fn plain_name(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '/' | '+'))
        && !s.contains("..")
        && !s.starts_with('/')
}

/// PEP 503: lower case, runs of `-_.` as one `-`.
fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

impl Lock {
    /// The `lock` argument, if there is one. Errors name the field and what it must be (`HOSTD_ARGS_INVALID`).
    pub fn parse(args: &Value) -> Result<Option<Lock>, String> {
        let Some(lock) = args.get("lock").filter(|l| !l.is_null()) else {
            return Ok(None);
        };
        let s = |k: &str| {
            lock.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let list = |k: &str, max: usize| -> Result<Vec<String>, String> {
            let items = lock
                .get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.len() > max {
                return Err(invalid(format!(
                    "lock.{k} 最多 {max} 个，给了 {} 个",
                    items.len()
                )));
            }
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .filter(|s| s.len() <= 4000)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            invalid(format!(
                                "lock.{k} 每个都要是不超过 4000 字节的字符串，收到 {v}"
                            ))
                        })
                })
                .collect()
        };
        let kind = match s("kind").as_str() {
            "npm" => Kind::Npm,
            "uv" => Kind::Uv,
            other => return Err(invalid(format!("lock.kind 要是 npm 或 uv，收到 {other:?}"))),
        };
        let mut files = Vec::new();
        let mut total = 0;
        let given = lock
            .get("files")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for name in kind.files() {
            let Some(text) = given.get(*name).and_then(Value::as_str) else {
                return Err(invalid(format!(
                    "lock.files 缺 {name}（{} 锁要有 {}）",
                    s("kind"),
                    kind.files().join("、")
                )));
            };
            if text.len() > FILE_MAX {
                return Err(invalid(format!(
                    "lock.files.{name} 有 {} 字节，最多 {FILE_MAX}",
                    text.len()
                )));
            }
            total += text.len();
            files.push((name.to_string(), text.to_string()));
        }
        if let Some(extra) = given.keys().find(|k| !kind.files().contains(&k.as_str())) {
            return Err(invalid(format!(
                "lock.files 里不该有 {extra:?}（只收 {}）",
                kind.files().join("、")
            )));
        }
        if total > FILES_MAX {
            return Err(invalid(format!(
                "lock.files 一共 {total} 字节，最多 {FILES_MAX}"
            )));
        }
        let indexes = list("indexes", 6)?;
        if indexes.is_empty()
            || !indexes.iter().all(|i| {
                i.starts_with("https://")
                    && i.len() <= 200
                    && !i
                        .chars()
                        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
            })
        {
            return Err(invalid(format!(
                "lock.indexes 要是 1–6 个 https:// 地址，收到 {indexes:?}"
            )));
        }
        let (package, version, bin) = (s("package"), s("version"), s("bin"));
        if !plain_name(&package, 214)
            || !plain_name(&version, 60)
            || !plain_name(&bin, 100)
            || bin.contains('/')
        {
            return Err(invalid(format!(
                "lock.package / version / bin 要是包名、版本和程序名，收到 {package:?} {version:?} {bin:?}"
            )));
        }
        let build = list("build", 16)?;
        if (kind == Kind::Npm && !build.is_empty())
            || !build.iter().all(|b| plain_name(b, 100) && !b.contains('/'))
        {
            return Err(invalid(format!(
                "lock.build 只给 Python 锁，要是包名，收到 {build:?}"
            )));
        }
        let lock = Lock {
            kind,
            files,
            indexes,
            package,
            version,
            bin,
            args: list("args", 64)?,
            build,
        };
        if kind == Kind::Uv {
            let v = lock.file(".python-version").trim();
            let ok = (3..=10).contains(&v.len())
                && v.split('.').count() <= 3
                && v.split('.')
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
            if !ok {
                return Err(invalid(format!(
                    "lock.files..python-version 要是 3.12 这样的版本，收到 {v:?}"
                )));
            }
        }
        Ok(Some(lock))
    }

    fn file(&self, name: &str) -> &str {
        self.files
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.as_str())
            .unwrap_or("")
    }

    /// The folder name of this lock: a digest of what decides what gets installed.
    pub fn key(&self) -> String {
        let mut h = Sha256::new();
        h.update(match self.kind {
            Kind::Npm => b"npm\0".as_slice(),
            Kind::Uv => b"uv\0".as_slice(),
        });
        for (name, text) in &self.files {
            h.update(name.as_bytes());
            h.update([0]);
            h.update((text.len() as u64).to_le_bytes());
            h.update(text.as_bytes());
        }
        for b in &self.build {
            h.update(b.as_bytes());
            h.update([0]);
        }
        h.finalize()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The lock installs exactly the pinned package: its own entry in the lock has the pinned version and the
    /// catalog's digest (npm `integrity` / the wheel's sha256). A lock for something else is refused.
    pub fn pins(&self, digest: &str) -> Result<(), String> {
        match self.kind {
            Kind::Npm => {
                let tree: Value = serde_json::from_str(self.file("package-lock.json"))
                    .map_err(|e| format!("package-lock.json 不是 JSON：{e}"))?;
                let own = &tree["packages"][format!("node_modules/{}", self.package)];
                if own["version"] != self.version.as_str() || own["integrity"] != digest {
                    return Err(format!(
                        "package-lock.json 里的 {} 是 {} {}，目录钉的是 {} {digest}",
                        self.package, own["version"], own["integrity"], self.version
                    ));
                }
            }
            Kind::Uv => {
                let want = normalize(&self.package);
                let hex = digest.strip_prefix("sha256:").unwrap_or("");
                let mut block = String::new();
                let mut found = false;
                for line in self.file("requirements.txt").lines() {
                    let t = line.trim();
                    if !line.starts_with(' ') && !t.is_empty() && !t.starts_with('#') {
                        if found {
                            break;
                        }
                        let (name, version) = t.split_once("==").unwrap_or(("", ""));
                        let version = version.split([' ', '\\', ';']).next().unwrap_or("");
                        found = normalize(name.trim()) == want && version == self.version;
                    }
                    if found {
                        block.push_str(t);
                        block.push('\n');
                    }
                }
                if !found || hex.len() != 64 || !block.contains(&format!("--hash=sha256:{hex}")) {
                    return Err(format!(
                        "requirements.txt 里没有 {}=={} 带目录钉的 {digest}",
                        self.package, self.version
                    ));
                }
            }
        }
        Ok(())
    }
}

/// One failed attempt.
struct Attempt {
    source: String,
    checksum: bool,
    said: String,
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| {
            !l.trim().is_empty()
                && !l.contains("npm notice")
                && !l.contains("complete log of this run")
        })
        .collect();
    let start = lines.len().saturating_sub(12);
    lines[start..].join(" | ")
}

/// Run a command with its output in `log`; past `bound` it is stopped.
fn run(mut cmd: Command, log: &Path, bound: Option<Duration>) -> Result<(bool, String), String> {
    let out = std::fs::File::create(log).map_err(|e| format!("建不了 {}：{e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    let mut child = cmd.spawn().map_err(|e| format!("起不来：{e}"))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(status);
        }
        if bound.is_some_and(|b| started.elapsed() > b) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let said = tail(&std::fs::read_to_string(log).unwrap_or_default());
    match status {
        Some(s) => Ok((s.success(), said)),
        None => Ok((
            false,
            format!(
                "{} 秒内没装完，换下一个源（{said}）",
                bound.unwrap_or_default().as_secs()
            ),
        )),
    }
}

/// What the tools say when a file's checksum is not the lock's.
fn checksum_failed(said: &str) -> bool {
    said.contains("EINTEGRITY")
        || said.contains("Hash mismatch")
        || said.contains("integrity checksum failed")
}

/// The variables that would point npm or uv somewhere else than the source of an attempt.
const SOURCE_ENV: &[&str] = &[
    "npm_config_registry",
    "UV_DEFAULT_INDEX",
    "UV_INDEX_URL",
    "UV_INDEX",
    "UV_EXTRA_INDEX_URL",
    "UV_FIND_LINKS",
    "UV_NO_VERIFY_HASHES",
    "UV_INSECURE_HOST",
    "UV_PYTHON_INSTALL_MIRROR",
    "UV_PYTHON_DOWNLOADS",
];

impl Runtime {
    /// The folder a lock installs into.
    pub fn lock_dir(&self, lock: &Lock) -> PathBuf {
        self.packages_dir().join("locks").join(lock.key())
    }

    fn python_dir(&self) -> PathBuf {
        self.packages_dir().join("python")
    }

    /// The sources of a lock in the order to try them (see the module docs).
    pub fn lock_sources(&self, lock: &Lock) -> Vec<String> {
        let mut sources = lock.indexes.clone();
        if runtimes::prefer_mirrors(&self.runtimes_dir()) && sources.len() > 1 {
            sources.rotate_left(1);
        }
        sources
    }

    /// Install a lock (once; later calls find it) and say what to run. Blocking: minutes on a first install.
    pub fn install_lock(&self, lock: &Lock) -> Result<Installed, Unavailable> {
        let _one = INSTALLING.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let dir = self.lock_dir(lock);
        if let Some(program) = self.installed_lock(lock, &dir) {
            return Ok(Installed {
                program: program.0,
                args: program.1,
                from: None,
                took: started.elapsed(),
            });
        }
        let fault = |what: String| Unavailable {
            code: INSTALL_FAILED,
            message: what,
            next: "这台电脑上的文件夹写不进去，重试帮不了：告诉用户原因里写的路径和错误。".into(),
        };
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| fault(format!("清不掉没装完的 {}：{e}", dir.display())))?;
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| fault(format!("建不了 {}：{e}", dir.display())))?;
        for (name, text) in &lock.files {
            std::fs::write(dir.join(name), text)
                .map_err(|e| fault(format!("写不了 {name}：{e}")))?;
        }
        let from = match lock.kind {
            Kind::Npm => self.npm_install(lock, &dir)?,
            Kind::Uv => self.uv_install(lock, &dir)?,
        };
        let Some((program, args)) = self.lock_program(lock, &dir) else {
            return Err(fault(format!(
                "装完了，但 {} 里没有程序 {}",
                lock.package, lock.bin
            )));
        };
        std::fs::write(dir.join(MARKER), format!("{}\n{from}\n", lock.key()))
            .map_err(|e| fault(format!("写不了 {MARKER}：{e}")))?;
        Ok(Installed {
            program,
            args,
            from: Some(from),
            took: started.elapsed(),
        })
    }

    fn installed_lock(&self, lock: &Lock, dir: &Path) -> Option<(PathBuf, Vec<String>)> {
        let marker = std::fs::read_to_string(dir.join(MARKER)).ok()?;
        (marker.lines().next() == Some(lock.key().as_str())).then_some(())?;
        self.lock_program(lock, dir)
    }

    /// The installed program: npm → the device's Node with the package's `bin` script; uv → the venv's program.
    fn lock_program(&self, lock: &Lock, dir: &Path) -> Option<(PathBuf, Vec<String>)> {
        match lock.kind {
            Kind::Npm => {
                let pkg = dir.join("node_modules").join(&lock.package);
                let manifest: Value =
                    serde_json::from_str(&std::fs::read_to_string(pkg.join("package.json")).ok()?)
                        .ok()?;
                let unscoped = lock.package.rsplit('/').next().unwrap_or(&lock.package);
                let rel = match &manifest["bin"] {
                    Value::String(s) if lock.bin == unscoped => s.clone(),
                    Value::Object(m) => m.get(&lock.bin)?.as_str()?.to_string(),
                    _ => return None,
                };
                if rel.split(['/', '\\']).any(|p| p == "..") {
                    return None;
                }
                let script = pkg.join(rel.trim_start_matches("./"));
                script.is_file().then_some(())?;
                Some((self.node().ok()?, vec![crate::shown(&script)]))
            }
            Kind::Uv => {
                let venv = dir.join(".venv");
                let program = if cfg!(windows) {
                    venv.join("Scripts").join(format!("{}.exe", lock.bin))
                } else {
                    venv.join("bin").join(&lock.bin)
                };
                program.is_file().then_some((program, Vec::new()))
            }
        }
    }

    fn tool_command(&self, program: &Path, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(dir).env(
            "PATH",
            self.path_with_runtimes(std::env::var_os("PATH").as_deref()),
        );
        for k in SOURCE_ENV {
            cmd.env_remove(k);
            cmd.env_remove(k.to_ascii_uppercase());
        }
        cmd
    }

    /// Each source in turn until one works; every failure kept for the error.
    fn attempts(
        &self,
        what: &str,
        sources: &[String],
        mut one: impl FnMut(&str, Option<Duration>) -> Result<(bool, String), String>,
    ) -> Result<String, Unavailable> {
        let mut failed: Vec<Attempt> = Vec::new();
        for (i, source) in sources.iter().enumerate() {
            let last = i + 1 == sources.len();
            let (ok, said) = one(source, (!last).then_some(OFFICIAL_BOUND)).map_err(|e| Unavailable {
                code: INSTALL_FAILED,
                message: format!("{what}：{e}"),
                next: "AgentRouter 自己的 Node.js 或 uv 起不来：请用户运行 agentrouter sandbox setup 后再装。".into(),
            })?;
            if ok {
                return Ok(source.clone());
            }
            let checksum = checksum_failed(&said);
            if !checksum
                && i == 0
                && sources.len() > 1
                && !runtimes::prefer_mirrors(&self.runtimes_dir())
            {
                // The official source failed or was slow here: mirrors go first from now on (as for the runtimes).
                runtimes::set_prefer_mirrors(&self.runtimes_dir());
            }
            failed.push(Attempt {
                source: source.clone(),
                checksum,
                said,
            });
        }
        let each = failed
            .iter()
            .map(|a| format!("{}：{}", a.source, a.said))
            .collect::<Vec<_>>()
            .join("；");
        Err(if let Some(bad) = failed.iter().find(|a| a.checksum) {
            Unavailable {
                code: DIGEST_MISMATCH,
                message: format!(
                    "{what}：{} 发来的文件和目录锁里审过的校验值不一样，没有安装（{each}）",
                    bad.source
                ),
                next: "不要重试，也不要换别的方式装它：告诉用户这个本机工具的安装包没通过校验，平台已记录。".into(),
            }
        } else {
            Unavailable {
                code: INSTALL_FAILED,
                message: format!("{what}：{} 个源都没装成（{each}）", failed.len()),
                next: "过一分钟再装一次；还不行就告诉用户这台电脑连不上原因里的这些软件源（网络、代理或防火墙）。不要换别的方式装。".into(),
            }
        })
    }

    fn npm_install(&self, lock: &Lock, dir: &Path) -> Result<String, Unavailable> {
        let node = self.node().map_err(|e| Unavailable {
            code: INSTALL_FAILED,
            message: e,
            next: "请用户运行 agentrouter sandbox setup 后再装。".into(),
        })?;
        let npm = self.npm_cli();
        let log = dir.join("install.log");
        let what = format!("npm ci 装 {}@{} 的锁", lock.package, lock.version);
        self.attempts(&what, &self.lock_sources(lock), |registry, bound| {
            let mut cmd = self.tool_command(&node, dir);
            cmd.arg(&npm)
                .args([
                    "ci",
                    "--omit=dev",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                    "--loglevel=error",
                ])
                .env("npm_config_registry", registry)
                .env("npm_config_update_notifier", "false");
            run(cmd, &log, bound)
        })
    }

    fn uv_install(&self, lock: &Lock, dir: &Path) -> Result<String, Unavailable> {
        let uv = runtimes::UV
            .program(&self.runtimes_dir(), "uv")
            .ok_or_else(|| Unavailable {
                code: INSTALL_FAILED,
                message: format!("装 {}@{} 要用的 uv 还没准备好", lock.package, lock.version),
                next: "请用户运行 agentrouter sandbox setup 后再装。".into(),
            })?;
        let python = lock.file(".python-version").trim().to_string();
        let log = dir.join("install.log");
        let pythons = self.python_dir();
        let base = |cmd: &mut Command| {
            cmd.env("UV_PYTHON_INSTALL_DIR", &pythons)
                .env("UV_MANAGED_PYTHON", "1")
                .env("UV_LINK_MODE", "copy")
                .env("UV_NO_PROGRESS", "1");
        };
        // The managed Python (shared by every Python lock): uv's own host first, then the mirror.
        let mut hosts = vec![
            "https://github.com/astral-sh/python-build-standalone".to_string(),
            PYTHON_MIRROR.to_string(),
        ];
        if runtimes::prefer_mirrors(&self.runtimes_dir()) {
            hosts.rotate_left(1);
        }
        self.attempts(&format!("uv 装 Python {python}"), &hosts, |host, bound| {
            let mut cmd = self.tool_command(&uv, dir);
            base(&mut cmd);
            cmd.args(["python", "install", &python]);
            if host == PYTHON_MIRROR {
                cmd.env("UV_PYTHON_INSTALL_MIRROR", host);
            }
            run(cmd, &log, bound)
        })?;
        let mut venv = self.tool_command(&uv, dir);
        base(&mut venv);
        venv.env("UV_PYTHON_DOWNLOADS", "never")
            .args(["venv", "--clear", "--quiet", "--python", &python, ".venv"]);
        match run(venv, &log, None) {
            Ok((true, _)) => {}
            Ok((false, said)) | Err(said) => {
                return Err(Unavailable {
                    code: INSTALL_FAILED,
                    message: format!("uv venv（Python {python}）没建成：{said}"),
                    next: "请用户运行 agentrouter sandbox setup 后再装；还不行就把原因告诉用户。"
                        .into(),
                });
            }
        }
        // Wheels only, except what the entry builds (pure-Python source files such as proxy-tools).
        let only_binary = if lock.build.is_empty() {
            "\":all:\"".to_string()
        } else {
            let build: Vec<String> = lock.build.iter().map(|b| normalize(b)).collect();
            lock.file("requirements.txt")
                .lines()
                .filter(|l| !l.starts_with(' ') && !l.trim_start().starts_with('#'))
                .filter_map(|l| l.split_once("==").map(|(n, _)| n.trim().to_string()))
                .filter(|n| !build.contains(&normalize(n)))
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        std::fs::write(
            dir.join("uv.toml"),
            format!("[pip]\nonly-binary = [{only_binary}]\n"),
        )
        .map_err(|e| Unavailable {
            code: INSTALL_FAILED,
            message: format!("写不了 uv.toml：{e}"),
            next: "这台电脑上的文件夹写不进去，重试帮不了：告诉用户原因里写的路径和错误。".into(),
        })?;
        let venv_python = if cfg!(windows) {
            dir.join(".venv").join("Scripts").join("python.exe")
        } else {
            dir.join(".venv").join("bin").join("python")
        };
        let what = format!("uv pip sync 装 {}=={} 的锁", lock.package, lock.version);
        self.attempts(&what, &self.lock_sources(lock), |index, bound| {
            let mut cmd = self.tool_command(&uv, dir);
            base(&mut cmd);
            cmd.env("UV_DEFAULT_INDEX", index)
                .env("UV_PYTHON_DOWNLOADS", "never")
                .args([
                    "pip",
                    "sync",
                    "--require-hashes",
                    "--config-file",
                    "uv.toml",
                    "--python",
                ])
                .arg(&venv_python)
                .arg("requirements.txt");
            run(cmd, &log, bound)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn npm_lock(integrity: &str) -> Value {
        let tree = json!({"name": "x", "lockfileVersion": 3, "packages": {
            "": {"dependencies": {"@scope/srv": "1.0.0"}},
            "node_modules/@scope/srv": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/@scope/srv/-/srv-1.0.0.tgz", "integrity": integrity}
        }});
        json!({"lock": {"kind": "npm", "files": {"package.json": "{}", "package-lock.json": tree.to_string()},
            "indexes": ["https://registry.npmjs.org/", "https://registry.npmmirror.com/"],
            "package": "@scope/srv", "version": "1.0.0", "bin": "srv", "args": ["{{folders}}"]}})
    }

    #[test]
    fn a_lock_is_parsed_checked_against_the_pin_and_keyed_by_its_content() {
        let lock = Lock::parse(&npm_lock("sha512-abc")).unwrap().unwrap();
        assert_eq!(lock.kind, Kind::Npm);
        assert_eq!(lock.args, ["{{folders}}"]);
        assert!(lock.pins("sha512-abc").is_ok());
        assert!(
            lock.pins("sha512-other")
                .unwrap_err()
                .contains("sha512-other")
        );
        let other = Lock::parse(&npm_lock("sha512-abd")).unwrap().unwrap();
        assert_ne!(lock.key(), other.key());
        assert_eq!(lock.key().len(), 32);
        assert!(Lock::parse(&json!({})).unwrap().is_none());
    }

    #[test]
    fn a_python_lock_names_the_pinned_wheel() {
        let hex = "a".repeat(64);
        let req = format!(
            "# header\nanyio==4.0 \\\n    --hash=sha256:{}\nserena-agent==2.0.0 \\\n    --hash=sha256:{hex}\n    # via x\n",
            "b".repeat(64)
        );
        let args = json!({"lock": {"kind": "uv", "files": {"requirements.txt": req, ".python-version": "3.12\n"},
            "indexes": ["https://pypi.org/simple"], "package": "serena_agent", "version": "2.0.0", "bin": "serena",
            "build": ["proxy-tools"]}});
        let lock = Lock::parse(&args).unwrap().unwrap();
        assert!(lock.pins(&format!("sha256:{hex}")).is_ok());
        assert!(lock.pins(&format!("sha256:{}", "b".repeat(64))).is_err());
        assert_eq!(normalize("Proxy_Tools"), "proxy-tools");
    }

    #[test]
    fn bad_locks_are_refused_with_the_field_named() {
        let mut bad = npm_lock("sha512-abc");
        bad["lock"]["indexes"] = json!(["http://registry.npmjs.org/"]);
        assert!(Lock::parse(&bad).unwrap_err().contains("lock.indexes"));
        let mut bad = npm_lock("sha512-abc");
        bad["lock"]["files"]["evil.sh"] = json!("x");
        assert!(Lock::parse(&bad).unwrap_err().contains("evil.sh"));
        let mut bad = npm_lock("sha512-abc");
        bad["lock"]["kind"] = json!("pip");
        assert!(Lock::parse(&bad).unwrap_err().contains("lock.kind"));
        let mut bad = npm_lock("sha512-abc");
        bad["lock"]["bin"] = json!("../x");
        assert!(Lock::parse(&bad).is_err());
        let mut bad = npm_lock("sha512-abc");
        bad["lock"]["build"] = json!(["x"]);
        assert!(Lock::parse(&bad).unwrap_err().contains("lock.build"));
    }

    #[test]
    fn checksum_failures_are_told_apart_from_network_ones() {
        assert!(checksum_failed("npm error code EINTEGRITY"));
        assert!(checksum_failed(
            "cause: Hash mismatch for `annotated-types==0.8.0`"
        ));
        assert!(!checksum_failed("npm error code ETIMEDOUT"));
    }
}
