//! Packages of local MCP servers (DEVICE-PROTOCOL.md §18.1–§18.3): pinned catalog packages are checked here before
//! they run, a broken npx cache entry is cleared, and a server's own data folder is kept out of the person's git.
//!
//! - Pinned packages: the cloud's catalog records each package's registry digest (npm `integrity`, PyPI wheel
//!   sha256). The device never downloads a package itself (it talks only to its gateway, docs/AV-HYGIENE.md; the one
//!   exception is its own pinned Node.js and uv, `agentrouter_sandbox::runtimes`, #116): the cloud sends the package
//!   file with `mcp_package`, the device checks its digest and keeps it in its data folder,
//!   and `mcp_start` runs exactly that file (`{{package}}` in the arguments). A file that does not match is refused.
//!   A preset that comes with its lock (Cloud #2740) is installed from the lock instead, every package checksummed
//!   (`agentrouter_sandbox::locks`).
//! - npx cache: an install killed midway (a crash, a stop during the first download) leaves `_npx/<hash>` half
//!   written, and every later start fails on a missing module. The host clears only that entry and starts once more.
//! - Keep-out folders: a server that keeps its data in the project (serena's `.serena`) gets the folder created with
//!   a `.gitignore` of `*` first, so git never sees it; an existing folder is left as it is.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use crate::protocol::DeviceError;
use crate::util::{b64_decode, hex};

/// A package file is at most this big (the request frame is 8 MiB, base64 included).
pub const PACKAGE_MAX: usize = 6 << 20;

/// The argument the device fills with the checked package file.
pub const PACKAGE_ARG: &str = "{{package}}";

fn error(code: &'static str, message: impl Into<String>) -> DeviceError {
    DeviceError::new(code, message)
}

/// What the catalog pinned: the package file and its registry digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub file: String,
    pub digest: String,
    /// The package's name (npm or PyPI), for messages and for finding its npx cache entry.
    pub package: String,
    pub version: String,
}

fn valid_file(file: &str) -> bool {
    (5..=160).contains(&file.len())
        && file
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && file
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        && (file.ends_with(".tgz") || file.ends_with(".whl"))
        && !file.contains("..")
}

/// The expected bytes of a digest: `sha512-<base64>` (npm) or `sha256:<hex>` (PyPI).
fn expected(digest: &str) -> Option<(&'static str, Vec<u8>)> {
    if let Some(b64) = digest.strip_prefix("sha512-") {
        let bytes = b64_decode(b64)?;
        return (bytes.len() == 64).then_some(("sha512", bytes));
    }
    if let Some(h) = digest.strip_prefix("sha256:") {
        if h.len() != 64 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let bytes = (0..32)
            .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?;
        return Some(("sha256", bytes));
    }
    None
}

/// The digest of some bytes, written the way `digest` is (to show what was found instead).
fn actual(kind: &str, bytes: &[u8]) -> (Vec<u8>, String) {
    if kind == "sha512" {
        let d = Sha512::digest(bytes).to_vec();
        let shown = format!("sha512-{}", crate::util::b64(&d));
        (d, shown)
    } else {
        let d = Sha256::digest(bytes).to_vec();
        let shown = format!("sha256:{}", hex(&d));
        (d, shown)
    }
}

impl Pin {
    pub fn parse(args: &Value) -> Result<Option<Pin>, DeviceError> {
        let Some(pin) = args.get("pin").filter(|p| !p.is_null()) else {
            return Ok(None);
        };
        let s = |k: &str| {
            pin.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let (file, digest) = (s("file"), s("digest"));
        if !valid_file(&file) {
            return Err(error(
                crate::errors::HOSTD_ARGS_INVALID,
                format!("pin.file 要是一个 .tgz 或 .whl 文件名，收到 {file:?}"),
            ));
        }
        if expected(&digest).is_none() {
            return Err(error(
                crate::errors::HOSTD_ARGS_INVALID,
                format!("pin.digest 要是 sha512-… 或 sha256:…，收到 {digest:?}"),
            ));
        }
        Ok(Some(Pin {
            file,
            digest,
            package: crate::util::clip(&s("package"), 160),
            version: crate::util::clip(&s("version"), 60),
        }))
    }

    /// Whether these bytes are the pinned package; otherwise what they are instead.
    pub fn check(&self, bytes: &[u8]) -> Result<(), String> {
        let Some((kind, want)) = expected(&self.digest) else {
            return Err("unknown digest".into());
        };
        let (got, shown) = actual(kind, bytes);
        if got == want { Ok(()) } else { Err(shown) }
    }

    fn mismatch(&self, found: &str) -> DeviceError {
        error(
            "MCP_DIGEST_MISMATCH",
            format!(
                "{} {} 的包和目录里审过的不一样（应为 {}，实际 {found}），没有启动",
                self.package, self.version, self.digest
            ),
        )
        .next(
            "不要重试，也不要换别的方式装它：告诉用户这个本机工具的安装包没通过校验，平台已记录。",
        )
    }
}

/// Where checked packages are kept (in the helper's data folder, never a temporary folder).
pub struct Packages {
    dir: PathBuf,
}

impl Packages {
    pub fn new(dir: PathBuf) -> Packages {
        Packages { dir }
    }

    /// The checked file for a pin: the stored one when it still matches, else `MCP_PACKAGE_NEEDED` (the cloud sends it).
    pub fn file(&self, pin: &Pin) -> Result<PathBuf, DeviceError> {
        let path = self.dir.join(&pin.file);
        let needed = || {
            error(
                "MCP_PACKAGE_NEEDED",
                format!("这台电脑上还没有 {}", pin.file),
            )
            .next("控制面先用 mcp_package 发来这个包，再启动。")
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Err(needed());
        };
        match pin.check(&bytes) {
            Ok(()) => Ok(path),
            // Changed on disk since it was checked: it goes, and the cloud sends it again.
            Err(_) => {
                let _ = std::fs::remove_file(&path);
                Err(needed())
            }
        }
    }

    /// `mcp_package`: keep a package the cloud sent, only when it is exactly the pinned one.
    pub fn store(&self, args: &Value) -> Result<Value, DeviceError> {
        let Some(pin) = Pin::parse(args)? else {
            return Err(error(crate::errors::HOSTD_ARGS_INVALID, "pin 不能为空"));
        };
        let data = args.get("data").and_then(Value::as_str).unwrap_or("");
        if data.len() > PACKAGE_MAX / 3 * 4 + 4 {
            return Err(error("TOO_LARGE", "包太大"));
        }
        let Some(bytes) = b64_decode(data) else {
            return Err(error(crate::errors::HOSTD_ARGS_INVALID, "data 要是 base64"));
        };
        if let Err(found) = pin.check(&bytes) {
            return Err(pin.mismatch(&found));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            error(
                crate::errors::HOSTD_FAULT,
                format!("没能建好包目录 {}：{e}", crate::gate::display(&self.dir)),
            )
        })?;
        let path = self.dir.join(&pin.file);
        let part = self.dir.join(format!("{}.part", pin.file));
        std::fs::write(&part, &bytes)
            .and_then(|_| std::fs::rename(&part, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&part);
                error(
                    crate::errors::HOSTD_FAULT,
                    format!("没能保存 {}：{e}", crate::gate::display(&path)),
                )
            })?;
        Ok(serde_json::json!({"file": pin.file, "bytes": bytes.len(), "digest": pin.digest}))
    }

    /// Check a pinned launch: the stored file, and `{{package}}` in the arguments replaced by it.
    pub fn apply(&self, pin: Option<&Pin>, args: &mut [String]) -> Result<(), DeviceError> {
        let wants = args.iter().any(|a| a == PACKAGE_ARG);
        let Some(pin) = pin else {
            return if wants {
                Err(error(
                    crate::errors::HOSTD_ARGS_INVALID,
                    "{{package}} 要和 pin 一起给",
                ))
            } else {
                Ok(())
            };
        };
        if !wants {
            return Err(error(
                crate::errors::HOSTD_ARGS_INVALID,
                "pin 要配一个 {{package}} 参数",
            ));
        }
        let file = crate::gate::display(&self.file(pin)?);
        for a in args.iter_mut().filter(|a| *a == PACKAGE_ARG) {
            *a = file.clone();
        }
        Ok(())
    }
}

// ---- npx cache ----

/// Lines that say a package's files are missing: a half-written install, not a bug of the server.
const BROKEN_SIGNS: &[&str] = &[
    "Cannot find module",
    "MODULE_NOT_FOUND",
    "ERR_MODULE_NOT_FOUND",
    "could not determine executable to run",
    "is not recognized as an internal or external command",
];

/// npm's cache folder: `npm_config_cache` (the server's environment, then ours), else npm's default.
pub fn npm_cache(env: &[(String, String)]) -> Option<PathBuf> {
    let from =
        |k: &str, v: &str| k.eq_ignore_ascii_case("npm_config_cache") && !v.trim().is_empty();
    if let Some((_, v)) = env.iter().find(|(k, v)| from(k, v)) {
        return Some(PathBuf::from(v.trim()));
    }
    for (k, v) in std::env::vars() {
        if from(&k, &v) {
            return Some(PathBuf::from(v.trim()));
        }
    }
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("npm-cache"))
    } else {
        Some(crate::util::home_dir().join(".npm"))
    }
}

/// The `_npx/<hash>` entry a log line names (`…\_npx\8c4e7a05d0d4c8f3\node_modules\…`).
fn entry_in(line: &str) -> Option<(String, String)> {
    let at = line.find("_npx")?;
    let rest = &line[at + 4..];
    let sep = rest.chars().next().filter(|c| *c == '\\' || *c == '/')?;
    let hash: String = rest[1..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if !(8..=64).contains(&hash.len()) {
        return None;
    }
    // The cache folder is what comes before `_npx` on that line (after the last space or quote).
    let before = &line[..at];
    let start = before
        .rfind([' ', '\'', '"', '(', '\t'])
        .map(|i| i + 1)
        .unwrap_or(0);
    let cache = before[start..].trim_end_matches(sep).to_string();
    Some((cache, hash))
}

/// Whether an `_npx` entry is the one for this server: its manifest lists one of our package specs, or (an install
/// cut before the manifest was written) its node_modules holds our package.
fn entry_is_ours(entry: &Path, specs: &[String], package: &str) -> bool {
    if let Ok(text) = std::fs::read_to_string(entry.join("package.json"))
        && let Ok(manifest) = serde_json::from_str::<Value>(&text)
    {
        let listed = manifest
            .pointer("/_npx/packages")
            .and_then(Value::as_array)
            .is_some_and(|l| {
                l.iter()
                    .filter_map(Value::as_str)
                    .any(|p| specs.iter().any(|s| s == p))
            });
        let depends = !package.is_empty()
            && manifest
                .get("dependencies")
                .and_then(Value::as_object)
                .is_some_and(|d| d.contains_key(package));
        return listed || depends;
    }
    !package.is_empty() && entry.join("node_modules").join(package).is_dir()
}

/// After an npx server failed to start: the broken `_npx/<hash>` entry of its package, if the log says files are
/// missing. `log` is this start's output; `specs` the package arguments it was run with.
pub fn broken_npx_entry(
    log: &[String],
    env: &[(String, String)],
    specs: &[String],
    package: &str,
) -> Option<PathBuf> {
    if !log
        .iter()
        .any(|l| BROKEN_SIGNS.iter().any(|sign| l.contains(sign)))
    {
        return None;
    }
    let cache = npm_cache(env);
    // The entry the error names, when it is ours.
    for line in log {
        if let Some((dir, hash)) = entry_in(line) {
            let base = if dir.is_empty() {
                cache.clone()?
            } else {
                PathBuf::from(dir)
            };
            let entry = base.join("_npx").join(&hash);
            if entry.is_dir() && entry_is_ours(&entry, specs, package) {
                return Some(entry);
            }
        }
    }
    // Otherwise ours in the cache (a missing program names no path).
    let root = cache?.join("_npx");
    let mut found = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && entry_is_ours(p, specs, package));
    let first = found.next()?;
    // Two entries for one package: not sure which, leave both.
    found.next().is_none().then_some(first)
}

// ---- keep-out folders ----

const KEEP_OUT_NOTE: &str = "# 由 AgentRouter 创建：这个文件夹是本机工具的数据，不进 git（删掉这个文件就会显示出来）。\n*\n";

pub fn valid_keep_out(name: &str) -> bool {
    (2..=40).contains(&name.len())
        && name.starts_with('.')
        && name != ".."
        && name != ".git"
        && name[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Create `<folder>/<name>` with a `.gitignore` of `*` when it does not exist yet (an existing one is the person's).
pub fn keep_out(folder: &Path, name: &str) -> bool {
    let dir = folder.join(name);
    if dir.exists() || !folder.is_dir() {
        return false;
    }
    std::fs::create_dir(&dir).is_ok()
        && std::fs::write(dir.join(".gitignore"), KEEP_OUT_NOTE).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pin(bytes: &[u8]) -> Pin {
        Pin {
            file: "pkg-1.0.0.tgz".into(),
            digest: format!("sha512-{}", crate::util::b64(&Sha512::digest(bytes))),
            package: "pkg".into(),
            version: "1.0.0".into(),
        }
    }

    #[test]
    fn digests_are_npm_integrity_or_wheel_sha256() {
        let p = pin(b"abc");
        assert!(p.check(b"abc").is_ok());
        assert!(p.check(b"abd").unwrap_err().starts_with("sha512-"));
        let wheel = Pin {
            digest: format!("sha256:{}", hex(&Sha256::digest(b"whl"))),
            file: "a-1-py3-none-any.whl".into(),
            ..p
        };
        assert!(wheel.check(b"whl").is_ok());
        assert!(wheel.check(b"other").unwrap_err().starts_with("sha256:"));
        assert!(expected("md5:abc").is_none());
        assert!(expected("sha256:xyz").is_none());
    }

    #[test]
    fn file_names_are_plain() {
        assert!(valid_file("server-filesystem-1.0.0.tgz"));
        assert!(valid_file("serena_agent-2.0.0-py3-none-any.whl"));
        for bad in [
            "../x.tgz", "a/b.tgz", "a\\b.whl", "x.exe", ".tgz", "a..b.tgz",
        ] {
            assert!(!valid_file(bad), "{bad}");
        }
    }

    #[test]
    fn a_package_is_kept_only_when_it_matches_and_fills_the_launch() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-mcp-pkg");
        let _ = std::fs::remove_dir_all(&root);
        let packages = Packages::new(root.clone());
        let p = pin(b"package bytes");
        let args = |data: &[u8]| json!({"pin": {"file": p.file, "digest": p.digest, "package": "pkg", "version": "1.0.0"}, "data": crate::util::b64(data)});
        // Not there yet: the cloud is asked for it.
        let mut launch = vec![
            "-y".to_string(),
            "-p".into(),
            PACKAGE_ARG.into(),
            "bin".into(),
        ];
        assert_eq!(
            packages.apply(Some(&p), &mut launch).unwrap_err().code,
            "MCP_PACKAGE_NEEDED"
        );
        // Different bytes: refused, nothing kept.
        assert_eq!(
            packages.store(&args(b"tampered")).unwrap_err().code,
            "MCP_DIGEST_MISMATCH"
        );
        assert!(!root.join(&p.file).exists());
        packages.store(&args(b"package bytes")).unwrap();
        packages.apply(Some(&p), &mut launch).unwrap();
        assert!(launch[2].ends_with("pkg-1.0.0.tgz") && launch[2] != PACKAGE_ARG);
        // Changed on disk afterwards: dropped, asked for again.
        std::fs::write(root.join(&p.file), b"changed").unwrap();
        let mut again = vec![PACKAGE_ARG.to_string()];
        assert_eq!(
            packages.apply(Some(&p), &mut again).unwrap_err().code,
            "MCP_PACKAGE_NEEDED"
        );
        assert!(!root.join(&p.file).exists());
        // {{package}} only with a pin, a pin only with {{package}}.
        assert!(
            packages
                .apply(None, &mut [PACKAGE_ARG.to_string()])
                .is_err()
        );
        assert!(packages.apply(Some(&p), &mut ["x".to_string()]).is_err());
        assert!(packages.apply(None, &mut ["x".to_string()]).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_half_written_npx_entry_is_found_from_the_error_or_the_cache() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-mcp-npx");
        let _ = std::fs::remove_dir_all(&root);
        let cache = root.join("npm-cache");
        let ours = cache.join("_npx").join("8c4e7a05d0d4c8f3");
        let other = cache.join("_npx").join("0123456789abcdef");
        std::fs::create_dir_all(ours.join("node_modules").join("@scope").join("srv")).unwrap();
        std::fs::create_dir_all(other.join("node_modules").join("left-pad")).unwrap();
        std::fs::write(
            other.join("package.json"),
            r#"{"dependencies":{"left-pad":"1"},"_npx":{"packages":["left-pad@1"]}}"#,
        )
        .unwrap();
        let env = vec![("npm_config_cache".to_string(), cache.display().to_string())];
        let specs = vec!["@scope/srv@1.0.0".to_string()];
        // A crash without the signs of missing files: not a cache problem.
        assert!(
            broken_npx_entry(
                &["TypeError: x is not a function".into()],
                &env,
                &specs,
                "@scope/srv"
            )
            .is_none()
        );
        // Named in the error.
        let line = format!(
            "Error: Cannot find module '../helpers/x'\nRequire stack:\n- {}",
            ours.join("node_modules").join("a.js").display()
        );
        assert_eq!(
            broken_npx_entry(&[line], &env, &specs, "@scope/srv"),
            Some(ours.clone())
        );
        // A missing program names no path: found by its package.
        let missing =
            vec!["'srv' is not recognized as an internal or external command,".to_string()];
        assert_eq!(
            broken_npx_entry(&missing, &env, &specs, "@scope/srv"),
            Some(ours.clone())
        );
        // Never another package's entry.
        let theirs = format!(
            "Cannot find module 'x' {}",
            other.join("node_modules").display()
        );
        assert_eq!(
            broken_npx_entry(&[theirs], &env, &specs, "@scope/srv"),
            Some(ours.clone())
        );
        assert!(broken_npx_entry(&missing, &env, &["nothing@1".into()], "nothing").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_out_folders_are_ignored_by_git_and_never_overwritten() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-mcp-keep-out");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(keep_out(&root, ".serena"));
        assert!(
            std::fs::read_to_string(root.join(".serena").join(".gitignore"))
                .unwrap()
                .contains("\n*\n")
        );
        // The person's own (or an earlier) folder stays as it is.
        std::fs::write(root.join(".serena").join(".gitignore"), "/cache\n").unwrap();
        assert!(!keep_out(&root, ".serena"));
        assert_eq!(
            std::fs::read_to_string(root.join(".serena").join(".gitignore")).unwrap(),
            "/cache\n"
        );
        for bad in [".git", "..", "serena", ".a/b", "."] {
            assert!(!valid_keep_out(bad), "{bad}");
        }
        assert!(valid_keep_out(".serena"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
