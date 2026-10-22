//! The task step (Cloud #2836, PROJECT-SERVICE §4.7.2): the host half of a declared space task — one task contract,
//! the same on a cloud sandbox and on the person's computer (Windows, macOS, Linux). Control drives it; Agents never
//! see it (`_task`: the aggregator does not list hostd's own tools; Control calls it as `host___task`).
//!
//! A task runs in **its own folder in this environment's output folder** (`<out>/tasks/<space>/<task>`, never inside
//! the person's folders): its declared inputs copied there, its command a hostd job in it (`job_start`), its declared
//! outputs and deliverables read back from it. The command itself (with `${deps.<as>.path}` / `.version` already
//! resolved by Control from what this host reports) is an ordinary `job_start`; this tool does the file work around it:
//!
//! - `probe`: os, arch, GPU, and each required tool on the person's PATH (`path`: directories searched first) with the
//!   version its `--version` prints; a wanted version (`node@22`) must match it (`22` or `22.x`).
//! - `stage`: the task folder made afresh — the 作品's declared `inputs` (globs; none = every 作品 file but generated
//!   ones) copied from `work` and hashed, or for a check exactly a stored version's `manifest` (each file copied when the
//!   作品 copy has the same bytes, else answered `missing` for Control to write from the store); the outputs of the task
//!   it waits for (`carry`) added; the 作品's `node_modules` copied, never linked (a task's `npm ci` cannot reach the
//!   作品's install).
//! - `collect`: the declared `outputs` (globs), hashed.
//! - `pick`: the `from` files (globs), hashed, each with its place under the commit path (the pattern's fixed folder
//!   stripped).
//!
//! Globs are relative to the folder they select in: `**` crosses folders, `*` and `?` stay within one. Paths are
//! answered with `/` on every OS. Every answer is in the call's result; nothing travels on a command line.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Fail, Host, arg_str, io_fail, shown, tool};

/// Folders a task's default inputs never take (generated, installed or tool state).
const IGNORED: &[&str] = &[
    "node_modules",
    ".pnpm-store",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    ".parcel-cache",
    "coverage",
    ".nyc_output",
    "__pycache__",
    ".venv",
    "venv",
    ".pytest_cache",
    ".mypy_cache",
    "target",
    ".git",
    ".space",
];
/// Where task folders live inside the output folder.
const TASKS: &str = "tasks";
/// How long one tool's `--version` may take.
const VERSION_WAIT: Duration = Duration::from_secs(15);

pub(super) fn tools() -> Vec<Value> {
    let list = json!({"type": "array", "items": {"type": "string"}, "maxItems": 5000});
    vec![tool(
        "_task",
        "Platform use: the file work of a declared space task in this environment's output folder (tasks/<space>/<task>). op probe: os, arch, gpu, required tools and their versions. op stage: the task folder made afresh from the 作品 folder (work) — declared inputs, or a stored version's manifest — plus the waited-for task's outputs (carry). op collect: the declared outputs, hashed. op pick: the from: files, hashed, with their place under the commit path. The command itself runs with job_start in the answered folder.",
        json!({"type": "object", "properties": {
            "op": {"type": "string", "enum": ["probe", "stage", "collect", "pick"]},
            "tools": list, "path": list,
            "work": {"type": "string", "description": "the 作品 folder (absolute)"},
            "folder": {"type": "string", "description": "the task folder: tasks/<space>/<task>, relative to the output folder (or that folder's absolute path)"},
            "inputs": list, "outputs": list, "from": list,
            "manifest": {"type": "array", "items": {"type": "object", "properties": {
                "path": {"type": "string"}, "digest": {"type": "string"}, "executable": {"type": "boolean"}}}},
            "carry": {"type": "array", "items": {"type": "object", "properties": {
                "folder": {"type": "string"}, "files": list}}},
        }, "required": ["op"]}),
        false,
    )]
}

fn invalid(op: &str, message: String) -> Fail {
    Fail::coded(
        "HOSTD_ARGS_INVALID",
        message.clone(),
        json!({"component": "hostd", "operation": format!("_task {op}"), "cause": {"message": message},
               "retryable": false, "next": "pass the task step's arguments as Control's task runs build them"}),
    )
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `path` relative to `root`, with `/` on every OS.
fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// Every plain file under `root` (sorted; symbolic links are not followed), relative; `skip`: generated folders and
/// files left out.
fn walk(root: &Path, skip: bool) -> Vec<String> {
    fn go(root: &Path, dir: &Path, skip: bool, out: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        let (mut dirs, mut files) = (Vec::new(), Vec::new());
        for e in entries {
            let Ok(kind) = e.file_type() else { continue };
            let name = e.file_name().to_string_lossy().to_string();
            if kind.is_dir() {
                if !(skip && IGNORED.contains(&name.as_str())) {
                    dirs.push(e.path());
                }
            } else if kind.is_file() {
                if skip && (name == ".DS_Store" || name.ends_with(".pyc") || name.ends_with(".log"))
                {
                    continue;
                }
                files.push(e.path());
            }
        }
        for f in files {
            out.push(rel_of(root, &f));
        }
        for d in dirs {
            go(root, &d, skip, out);
        }
    }
    let mut out = Vec::new();
    go(root, root, skip, &mut out);
    out.sort();
    out
}

fn wild(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

fn clean(pat: &str) -> String {
    let mut p = pat.trim();
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    if p.ends_with('/') {
        format!("{p}**")
    } else {
        p.to_string()
    }
}

/// Whether `text` matches the glob `pat`: `**/` any folders (or none), `**` anything, `*` and `?` within one folder;
/// every other character (`[` included) stands for itself.
fn matches(pat: &[char], text: &[char]) -> bool {
    if pat.starts_with(&['*', '*', '/']) {
        let rest = &pat[3..];
        return matches(rest, text)
            || (1..=text.len()).any(|i| text[i - 1] == '/' && matches(rest, &text[i..]));
    }
    if pat.starts_with(&['*', '*']) {
        let rest = &pat[2..];
        return (0..=text.len()).any(|i| matches(rest, &text[i..]));
    }
    match pat.first() {
        None => text.is_empty(),
        Some('*') => {
            let stop = text.iter().position(|c| *c == '/').unwrap_or(text.len());
            (0..=stop).any(|i| matches(&pat[1..], &text[i..]))
        }
        Some('?') => text.first().is_some_and(|c| *c != '/') && matches(&pat[1..], &text[1..]),
        Some(c) => text.first() == Some(c) && matches(&pat[1..], &text[1..]),
    }
}

/// The pattern's fixed leading folder (before its first wildcard part; a pattern without one: its folder).
fn fixed(pat: &str) -> String {
    let parts: Vec<&str> = pat.split('/').collect();
    let mut head = Vec::new();
    for p in &parts[..parts.len() - 1] {
        if wild(p) {
            return head.join("/");
        }
        head.push(*p);
    }
    head.join("/")
}

fn dirname(rel: &str) -> &str {
    rel.rfind('/').map_or("", |i| &rel[..i])
}

/// The files of `root` the patterns select (sorted, relative).
fn expand(root: &Path, pats: &[String], skip: bool) -> Vec<String> {
    let mut found = BTreeSet::new();
    for raw in pats {
        let pat = clean(raw);
        if pat.is_empty()
            || pat.starts_with('/')
            || pat.split('/').any(|s| s == "..")
            || Path::new(&pat).is_absolute()
        {
            continue;
        }
        if !wild(&pat) {
            let whole = root.join(&pat);
            if is_plain_file(&whole) {
                found.insert(pat.clone());
            } else if whole.is_dir() {
                found.extend(walk(&whole, skip).into_iter().map(|r| format!("{pat}/{r}")));
            }
            continue;
        }
        let pre = fixed(&pat);
        let start = if pre.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&pre)
        };
        if !start.is_dir() {
            continue;
        }
        let chars: Vec<char> = pat.chars().collect();
        let skip_here = skip && !pre.split('/').any(|s| IGNORED.contains(&s));
        for r in walk(&start, skip_here) {
            let rel = if pre.is_empty() {
                r
            } else {
                format!("{pre}/{r}")
            };
            if matches(&chars, &rel.chars().collect::<Vec<_>>()) {
                found.insert(rel);
            }
        }
    }
    found.into_iter().collect()
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_executable(path: &Path, yes: bool) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(if yes { 0o755 } else { 0o644 }),
    );
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _yes: bool) {}

/// A file's entry: path, SHA-256, size, executable.
fn entry(root: &Path, rel: &str) -> std::io::Result<Value> {
    let path = root.join(rel);
    let mut file = File::open(&path)?;
    let meta = file.metadata()?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(json!({"path": rel, "sha256": hex, "size": meta.len(), "executable": executable(&meta)}))
}

fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::symlink_metadata(dst).is_ok() {
        std::fs::remove_file(dst)?;
    }
    std::fs::copy(src, dst).map(|_| ())
}

/// The 作品's `node_modules` as the task folder's own (#2747): a copy, never a link, so a task running `npm ci` or
/// rewriting a cached file cannot reach the 作品's install. Copy-on-write where Linux clones (`cp --reflink=auto`).
fn owned_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let cloned = Command::new("cp")
            .arg("-a")
            .arg("--reflink=auto")
            .arg(src)
            .arg(dst)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if cloned.is_ok_and(|s| s.success()) {
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(dst);
    }
    copy_tree(src, dst).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(dst);
    })
}

/// A folder copied entry by entry; symbolic links stay links (Unix; on Windows they are left out: a link there needs
/// a right the person may not have).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let kind = e.file_type()?;
        let to = dst.join(e.file_name());
        if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &to)?;
        } else if kind.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// The first line a tool prints for its version (`--version`, `version`, `-v`), waited for at most VERSION_WAIT.
fn version_line(exe: &Path, path_env: Option<&std::ffi::OsStr>) -> String {
    for flag in ["--version", "version", "-v"] {
        let mut cmd = Command::new(exe);
        cmd.arg(flag)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = agentrouter_device::shell_env::for_command() {
            cmd.env_clear().envs(env);
        }
        if let Some(p) = path_env {
            cmd.env("PATH", p);
        }
        let Ok(mut child) = cmd.spawn() else { continue };
        let until = Instant::now() + VERSION_WAIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let Some(status) = status else { continue };
        let mut out = String::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        if out.trim().is_empty()
            && let Some(mut s) = child.stderr.take()
        {
            let _ = s.read_to_string(&mut out);
        }
        let line = out.trim().lines().next().unwrap_or("").trim();
        if status.success() && !line.is_empty() {
            return line.chars().take(200).collect();
        }
    }
    String::new()
}

/// The first dotted number of a version line (`v22.11.0` → `22.11.0`).
fn version_of(line: &str) -> String {
    let start = line.find(|c: char| c.is_ascii_digit());
    let Some(start) = start else {
        return String::new();
    };
    let tail = &line[start..];
    let mut end = 0;
    let bytes = tail.as_bytes();
    while end < bytes.len() {
        let dot = bytes[end] == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
        if !(bytes[end].is_ascii_digit() || dot) {
            break;
        }
        end += 1;
    }
    tail[..end].to_string()
}

/// A program on `path_env` (Windows: `.exe`, `.cmd`, `.bat`, `.com` too).
fn find(name: &str, path_env: &std::ffi::OsStr) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat", ".com"]
    } else {
        &[""]
    };
    std::env::split_paths(path_env).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let p = dir.join(format!("{name}{ext}"));
            p.is_file().then_some(p)
        })
    })
}

impl Host {
    pub(super) fn task(&self, args: &Value) -> Result<Value, Fail> {
        let op = arg_str(args, "op").unwrap_or("");
        match op {
            "probe" => Ok(self.task_probe(args)),
            "stage" => self.task_stage(args),
            "collect" => {
                let folder = self.task_folder(op, args, "folder")?;
                let outputs = expand(&folder, &strings(args, "outputs"), false)
                    .iter()
                    .map(|rel| entry(&folder, rel))
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(|e| {
                        io_fail(
                            "_task collect",
                            "读不了任务的产出：",
                            &folder,
                            &e,
                            &self.folders,
                        )
                    })?;
                Ok(json!({"folder": shown(&folder), "outputs": outputs}))
            }
            "pick" => self.task_pick(args),
            other => Err(invalid(
                op,
                format!("_task op {other:?}: use probe, stage, collect or pick"),
            )),
        }
    }

    /// A task folder named in `key`: `tasks/…` relative to the output folder, or its absolute path; never outside it.
    fn task_folder(&self, op: &str, args: &Value, key: &str) -> Result<PathBuf, Fail> {
        let raw = arg_str(args, key).unwrap_or("").trim().to_string();
        self.task_folder_of(op, &raw)
    }

    fn task_folder_of(&self, op: &str, raw: &str) -> Result<PathBuf, Fail> {
        let tasks = self.out.root().join(TASKS);
        let path = PathBuf::from(raw);
        let inside = if path.is_absolute() {
            path.strip_prefix(&tasks)
                .ok()
                .map(Path::to_path_buf)
                .or_else(|| {
                    // The output folder as the caller saw it before (canonical vs not, `\\?\` on Windows).
                    let real = std::fs::canonicalize(&tasks).ok()?;
                    path.strip_prefix(shown(&real)).ok().map(Path::to_path_buf)
                })
        } else {
            Path::new(raw)
                .strip_prefix(TASKS)
                .ok()
                .map(Path::to_path_buf)
        };
        match inside {
            Some(rest)
                if rest.components().count() > 0
                    && rest.components().all(|c| matches!(c, Component::Normal(_))) =>
            {
                Ok(tasks.join(rest))
            }
            _ => Err(invalid(
                op,
                format!(
                    "{raw:?} is not a task folder: name one as tasks/<space>/<task> (inside {})",
                    shown(&tasks)
                ),
            )),
        }
    }

    fn task_probe(&self, args: &Value) -> Value {
        let base = agentrouter_device::shell_env::var_os("PATH").unwrap_or_default();
        let first: Vec<PathBuf> = strings(args, "path")
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let path_env = if first.is_empty() {
            base.clone()
        } else {
            std::env::join_paths(first.iter().cloned().chain(std::env::split_paths(&base)))
                .unwrap_or(base.clone())
        };
        let mut found = serde_json::Map::new();
        let mut missing = Vec::new();
        for t in strings(args, "tools") {
            let (name, want) = t.split_once('@').unwrap_or((&t, ""));
            let Some(exe) = find(name, &path_env) else {
                missing.push(format!("{t} (not installed)"));
                continue;
            };
            let line = version_line(&exe, (!first.is_empty()).then_some(path_env.as_os_str()));
            let ver = version_of(&line);
            if !want.is_empty() && !(ver == want || ver.starts_with(&format!("{want}."))) {
                let seen = if ver.is_empty() {
                    "an unknown version".to_string()
                } else {
                    ver
                };
                missing.push(format!("{t} (found {seen})"));
                continue;
            }
            let shown_line = if !line.is_empty() {
                line
            } else if !ver.is_empty() {
                ver
            } else {
                "present".into()
            };
            found.insert(name.to_string(), json!(shown_line));
        }
        json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "gpu": find("nvidia-smi", &path_env).is_some(),
            "tools": found,
            "missing": missing,
        })
    }

    fn task_stage(&self, args: &Value) -> Result<Value, Fail> {
        let op = "stage";
        let folder = self.task_folder(op, args, "folder")?;
        let work = match arg_str(args, "work").map(str::trim) {
            Some(w) if !w.is_empty() => self.absolute(w),
            _ => {
                return Err(invalid(
                    op,
                    "_task stage needs work (the 作品 folder)".into(),
                ));
            }
        };
        let io = |step: &str, path: &Path, e: std::io::Error| {
            io_fail("_task stage", step, path, &e, &self.folders)
        };
        // Made afresh: whatever an earlier run left goes.
        match std::fs::symlink_metadata(&folder) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&folder)
                .map_err(|e| io("清不掉上次的任务文件夹：", &folder, e))?,
            Ok(_) => std::fs::remove_file(&folder)
                .map_err(|e| io("清不掉上次的任务文件夹：", &folder, e))?,
            Err(_) => {}
        }
        std::fs::create_dir_all(&folder).map_err(|e| io("建不了任务文件夹：", &folder, e))?;
        let mut files: BTreeMap<String, Value> = BTreeMap::new();
        let mut missing = Vec::new();
        let carry: Vec<(PathBuf, Vec<String>)> = args
            .get("carry")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| {
                let dir =
                    self.task_folder_of(op, c.get("folder").and_then(Value::as_str).unwrap_or(""))?;
                Ok((dir, strings(c, "files")))
            })
            .collect::<Result<_, Fail>>()?;
        if let Some(manifest) = args.get("manifest").and_then(Value::as_array) {
            let carried: BTreeSet<&str> = carry
                .iter()
                .flat_map(|(_, f)| f.iter().map(String::as_str))
                .collect();
            for m in manifest {
                let rel = m.get("path").and_then(Value::as_str).unwrap_or("");
                if rel.is_empty() || carried.contains(rel) {
                    continue;
                }
                if rel.split('/').any(|s| s == ".." || s.is_empty()) || Path::new(rel).is_absolute()
                {
                    return Err(invalid(
                        op,
                        format!("manifest path {rel:?} leaves the 作品"),
                    ));
                }
                let digest = m.get("digest").and_then(Value::as_str).unwrap_or("");
                let (sha, size) = digest.split_once('-').unwrap_or((digest, ""));
                let src = work.join(rel);
                let same = is_plain_file(&src)
                    && std::fs::metadata(&src).is_ok_and(|meta| meta.len().to_string() == size)
                    && entry(&work, rel).is_ok_and(|e| e["sha256"] == sha);
                if same {
                    let dst = folder.join(rel);
                    copy_file(&src, &dst).map_err(|e| io("复制不了输入文件：", &src, e))?;
                    set_executable(
                        &dst,
                        m.get("executable").and_then(Value::as_bool) == Some(true),
                    );
                    files.insert(
                        rel.to_string(),
                        entry(&folder, rel).map_err(|e| io("读不了输入文件：", &dst, e))?,
                    );
                } else {
                    missing.push(m.clone());
                }
            }
        } else {
            let pats = strings(args, "inputs");
            let rels = if pats.is_empty() {
                walk(&work, true)
            } else {
                expand(&work, &pats, true)
            };
            for rel in rels {
                let (src, dst) = (work.join(&rel), folder.join(&rel));
                copy_file(&src, &dst).map_err(|e| io("复制不了输入文件：", &src, e))?;
                files.insert(
                    rel.clone(),
                    entry(&folder, &rel).map_err(|e| io("读不了输入文件：", &dst, e))?,
                );
            }
        }
        for (dir, rels) in &carry {
            for rel in rels {
                let src = dir.join(rel);
                if rel.split('/').any(|s| s == "..") || !src.is_file() {
                    continue;
                }
                let dst = folder.join(rel);
                copy_file(&src, &dst).map_err(|e| io("复制不了前一个任务的产出：", &src, e))?;
                files.insert(
                    rel.clone(),
                    entry(&folder, rel).map_err(|e| io("读不了输入文件：", &dst, e))?,
                );
            }
        }
        let deps = work.join("node_modules");
        let own = folder.join("node_modules");
        if deps.is_dir() && std::fs::symlink_metadata(&own).is_err() {
            owned_copy(&deps, &own).map_err(|e| {
                let cause = e.to_string();
                Fail::coded(
                    "TASK_ENVIRONMENT_FAILED",
                    format!("task stage: copying the 作品's node_modules ({}) into the task folder {} failed: {cause}; free space in the environment, then run the task again", shown(&deps), shown(&own)),
                    json!({"component": "hostd", "operation": "_task stage", "path": shown(&own), "cause": {"message": cause},
                           "retryable": true, "next": "free space in the environment, then run the task again"}),
                )
            })?;
        }
        Ok(
            json!({"folder": shown(&folder), "inputs": files.into_values().collect::<Vec<_>>(), "missing": missing}),
        )
    }

    fn task_pick(&self, args: &Value) -> Result<Value, Fail> {
        let folder = self.task_folder("pick", args, "folder")?;
        let mut picked: BTreeMap<String, Value> = BTreeMap::new();
        for raw in strings(args, "from") {
            let pat = clean(&raw);
            let pre = if wild(&pat) {
                fixed(&pat)
            } else {
                dirname(&pat).to_string()
            };
            for rel in expand(&folder, std::slice::from_ref(&raw), false) {
                let mut e = entry(&folder, &rel).map_err(|e| {
                    io_fail(
                        "_task pick",
                        "读不了要交付的文件：",
                        &folder.join(&rel),
                        &e,
                        &self.folders,
                    )
                })?;
                let to = if pre.is_empty() {
                    rel.clone()
                } else {
                    rel.strip_prefix(&format!("{pre}/"))
                        .unwrap_or(&rel)
                        .to_string()
                };
                e["to"] = json!(to);
                picked.insert(to, e);
            }
        }
        Ok(json!({"folder": shown(&folder), "files": picked.into_values().collect::<Vec<_>>()}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};
    use super::*;

    fn glob(pat: &str, text: &str) -> bool {
        matches(
            &clean(pat).chars().collect::<Vec<_>>(),
            &text.chars().collect::<Vec<_>>(),
        )
    }

    #[test]
    fn globs_cross_folders_only_with_two_stars() {
        assert!(glob("**/*.js", "a.js"));
        assert!(glob("**/*.js", "src/deep/a.js"));
        assert!(glob("src/*.js", "src/a.js"));
        assert!(!glob("src/*.js", "src/deep/a.js"));
        assert!(glob("src/**", "src/deep/a.js"));
        assert!(glob("dist/", "dist/x/y.css"));
        assert!(glob("a?.txt", "ab.txt"));
        assert!(!glob("a?.txt", "a/.txt"));
        assert!(glob("[x].txt", "[x].txt"));
        assert!(!glob("[x].txt", "x.txt"));
        assert_eq!(fixed("dist/**/*.css"), "dist");
        assert_eq!(fixed("a/b/c.txt"), "a/b");
        assert_eq!(fixed("*.txt"), "");
        assert_eq!(version_of("v22.11.0 (lts)"), "22.11.0");
        assert_eq!(version_of("git version 2.43.0."), "2.43.0");
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn stage_collect_and_pick_follow_the_task_contract() {
        let dir = temp("task");
        let mut host = host_in(&dir);
        host.out = std::sync::Arc::new(super::super::out::Out::at(dir.join("out"), u64::MAX, 0));
        let work = dir.join("work");
        write(&work.join("src/a.txt"), "a");
        write(&work.join("src/deep/b.txt"), "b");
        write(&work.join("README.md"), "r");
        write(&work.join("dist/old.js"), "generated");
        write(&work.join("node_modules/pkg/index.js"), "dep");
        let folder = "tasks/wrk_00000000/build";

        // Default inputs: every 作品 file but generated ones; node_modules is the task's own copy.
        let all = host
            .task(&json!({"op": "stage", "work": work.to_string_lossy(), "folder": folder}))
            .unwrap();
        let paths: Vec<&str> = all["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["README.md", "src/a.txt", "src/deep/b.txt"]);
        let abs = PathBuf::from(all["folder"].as_str().unwrap());
        assert!(abs.join("node_modules/pkg/index.js").is_file());
        assert_eq!(
            all["inputs"][1]["sha256"],
            "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
        );

        // Declared inputs, staged afresh; the earlier task's outputs carried in.
        write(&abs.join("out/x.bin"), "x");
        let prev = "tasks/wrk_00000000/prev";
        write(&host.out.root().join(prev).join("gen/c.txt"), "c");
        let some = host
            .task(&json!({"op": "stage", "work": work.to_string_lossy(), "folder": abs.to_string_lossy(),
                          "inputs": ["src/*.txt"], "carry": [{"folder": prev, "files": ["gen/c.txt"]}]}))
            .unwrap();
        let paths: Vec<&str> = some["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, ["gen/c.txt", "src/a.txt"]);
        assert!(!abs.join("out/x.bin").exists());

        // A stored version's manifest: same bytes copied, others answered missing.
        let a = entry(&work, "src/a.txt").unwrap();
        let manifest = json!([
            {"path": "src/a.txt", "digest": format!("{}-1", a["sha256"].as_str().unwrap()), "executable": true},
            {"path": "src/z.txt", "digest": "00-1", "executable": false},
        ]);
        let checked = host
            .task(&json!({"op": "stage", "work": work.to_string_lossy(), "folder": folder, "manifest": manifest}))
            .unwrap();
        assert_eq!(checked["inputs"].as_array().unwrap().len(), 1);
        assert_eq!(checked["missing"][0]["path"], "src/z.txt");
        assert_eq!(checked["inputs"][0]["executable"], cfg!(unix));

        // Outputs and deliverables.
        write(&abs.join("dist/site/index.html"), "<p>");
        write(&abs.join("dist/site/css/a.css"), "x");
        let outs = host
            .task(&json!({"op": "collect", "folder": folder, "outputs": ["dist/**"]}))
            .unwrap();
        assert_eq!(outs["outputs"].as_array().unwrap().len(), 2);
        let picked = host
            .task(&json!({"op": "pick", "folder": folder, "from": ["dist/site/**/*.css", "dist/site/index.html"]}))
            .unwrap();
        let to: Vec<&str> = picked["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["to"].as_str().unwrap())
            .collect();
        assert_eq!(to, ["css/a.css", "index.html"]);

        // Never outside the task folders.
        for bad in ["work", "tasks", "tasks/../x", dir.to_str().unwrap()] {
            let err = host
                .task(&json!({"op": "collect", "folder": bad, "outputs": ["*"]}))
                .unwrap_err();
            assert_eq!(err.code, Some("HOSTD_ARGS_INVALID"), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_names_the_os_and_what_is_missing() {
        let dir = temp("task-probe");
        let host = host_in(&dir);
        let p = host
            .task(&json!({"op": "probe", "tools": ["surely-not-a-tool-2836", "git@1"]}))
            .unwrap();
        assert_eq!(p["os"], std::env::consts::OS);
        assert_eq!(p["missing"][0], "surely-not-a-tool-2836 (not installed)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
