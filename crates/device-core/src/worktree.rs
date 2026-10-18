//! Git baseline, worktrees and the per-session cwd (workspace spec §2–4; M1 of the device side).
//!
//! - A project gets a git repository without touching the person's setup: a project that is not a
//!   repository yet is `git init`-ed and gets one baseline commit; an existing repository is used as
//!   it is (its current HEAD is the base commit, no remotes or config are changed).
//! - Each session gets its own worktree `<project>/.worktrees/<session>` on branch `ar/<session>`,
//!   created from the project's base commit. `.worktrees/` is kept out of the repository through the
//!   repository's local `info/exclude` (never a change to the person's files). Worktree create is
//!   idempotent, list/remove/prune are explicit, and a session is mapped to exactly one worktree
//!   (`worktrees.json` in the device data folder).
//! - `mcp_start` may run a server in a project or (the default for a bound session) in the session's
//!   worktree: `{{worktree}}`/`{{project}}` in the definition are replaced, and the server's cwd is
//!   that directory (a `start_process` tool inherits it). Paths are validated against the workspace
//!   registry, never taken from a random absolute path.
//!
//! Git is expected on the machine (the checkpoint feature already uses it). Every git call runs with
//! `-c core.longpaths=true` and a plain display path, so a long Windows path works without changing
//! the person's configuration.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gate::{self, display, inside};
use crate::protocol::DeviceError;
use crate::util::iso_now;
use crate::workspace::{self, Registry};

/// The worktree ledger in the device data folder.
pub const LEDGER_FILE: &str = "worktrees.json";
pub const SCHEMA: u32 = 1;
pub const BRANCH_PREFIX: &str = "ar/";
pub const WORKTREES_SUBDIR: &str = ".worktrees";
/// Stale worktree directories are pruned after this age unless told otherwise.
pub const PRUNE_AGE: Duration = Duration::from_secs(72 * 60 * 60);
const SESSION_MAX: usize = 64;

const GITIGNORE: &str = "# AgentRouter workspace defaults (created with the project)\n\
node_modules/\n\
target/\n\
.venv/\n\
venv/\n\
__pycache__/\n\
dist/\n\
build/\n\
*.log\n\
.env\n\
.env.*\n\
*.pem\n\
*.key\n\
id_rsa*\n\
.DS_Store\n";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Worktree {
    /// Project slug the worktree belongs to.
    pub project: String,
    /// Session id it was made for (the directory name and the branch suffix).
    pub session: String,
    /// The worktree directory as people write it.
    pub path: String,
    pub branch: String,
    /// The project commit this worktree started from.
    pub base_commit: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    pub schema_version: u32,
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
}

impl Ledger {
    fn empty() -> Ledger {
        Ledger {
            schema_version: SCHEMA,
            worktrees: Vec::new(),
        }
    }
}

/// One git call: whether it succeeded and what it printed (trimmed).
struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn git_failed(what: &str, out: &GitOut) -> DeviceError {
    let detail = if out.stderr.is_empty() {
        out.stdout.clone()
    } else {
        out.stderr.clone()
    };
    DeviceError::new(
        "WORKSPACE_GIT_FAILED",
        format!("git {what} 失败：{}", crate::util::clip(detail.trim(), 300)),
    )
}

/// Run git in `dir` (plain display path; long paths through `core.longpaths`). `identity` adds the
/// committer identity through the environment, never through the person's configuration.
fn run_git(dir: &Path, args: &[&str], identity: bool) -> Result<GitOut, DeviceError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(display(dir))
        .args([
            "-c",
            "core.longpaths=true",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    if identity {
        cmd.env("GIT_AUTHOR_NAME", "AgentRouter")
            .env("GIT_AUTHOR_EMAIL", "agentrouter@agentrouter.top")
            .env("GIT_COMMITTER_NAME", "AgentRouter")
            .env("GIT_COMMITTER_EMAIL", "agentrouter@agentrouter.top");
    }
    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            DeviceError::new(
                "WORKSPACE_GIT_MISSING",
                "这台电脑上找不到 git；工作区的基线和 worktree 需要 git",
            )
            .next("请先安装 git（Windows 上通常是 Git for Windows），再试一次。")
        } else {
            DeviceError::new("WORKSPACE_GIT_FAILED", format!("git 起不来：{e}"))
        }
    })?;
    Ok(GitOut {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_string(),
    })
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<String, DeviceError> {
    let out = run_git(dir, args, false)?;
    if out.ok {
        Ok(out.stdout)
    } else {
        Err(git_failed(&args.join(" "), &out))
    }
}

fn git_commit(dir: &Path, args: &[&str]) -> Result<String, DeviceError> {
    let out = run_git(dir, args, true)?;
    if out.ok {
        Ok(out.stdout)
    } else {
        Err(git_failed(&args.join(" "), &out))
    }
}

/// Detection only: `Some(stdout)` when the call succeeded, `None` otherwise (or git is missing).
fn git_probe(dir: &Path, args: &[&str]) -> Option<String> {
    run_git(dir, args, false)
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout)
}

/// The ledger on disk; a missing file is an empty ledger, a broken one is an error.
pub fn load_ledger(data: &Path) -> Result<Ledger, DeviceError> {
    let path = data.join(LEDGER_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Ledger::empty());
    };
    let ledger: Ledger = serde_json::from_str(&text).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_LEDGER_INVALID",
            format!("worktree 登记文件读不出来（{}）：{e}", display(&path)),
        )
    })?;
    if ledger.schema_version != SCHEMA {
        return Err(DeviceError::new(
            "WORKSPACE_LEDGER_INVALID",
            format!("worktree 登记文件版本不认识（{}）", ledger.schema_version),
        ));
    }
    Ok(ledger)
}

pub fn save_ledger(data: &Path, ledger: &Ledger) -> Result<(), DeviceError> {
    std::fs::create_dir_all(data).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("建不了设备数据目录 {}：{e}", display(data)),
        )
    })?;
    let path = data.join(LEDGER_FILE);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|e| DeviceError::new("WORKSPACE_FAILED", format!("登记文件写不出来：{e}")))?;
    std::fs::write(&tmp, bytes).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("worktree 登记写不进去（{}）：{e}", display(&tmp)),
        )
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("worktree 登记换不上去（{}）：{e}", display(&path)),
        )
    })
}

/// The session id becomes a directory name and a branch suffix: letters, digits, `_` and `-`.
pub fn session_id(raw: &str) -> Result<String, DeviceError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > SESSION_MAX {
        return Err(DeviceError::new(
            "WORKSPACE_SESSION_INVALID",
            format!("会话 id 要 1–{SESSION_MAX} 个字符"),
        ));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(DeviceError::new(
            "WORKSPACE_SESSION_INVALID",
            "会话 id 只能用英文字母、数字、_ 和 -（它同时是目录名和分支名）",
        ));
    }
    Ok(raw.to_string())
}

fn same_path(a: &str, b: &Path) -> bool {
    let a = Path::new(a);
    inside(a, b) && inside(b, a)
}

fn project_of(registry: &Registry, raw: &str) -> Result<workspace::Project, DeviceError> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            "要写项目名（workspace_ensure 先建项目）",
        ));
    }
    let slug = workspace::slug(key).unwrap_or_else(|_| key.to_ascii_lowercase());
    registry
        .projects
        .iter()
        .find(|p| p.slug == slug || p.name == key || p.name.eq_ignore_ascii_case(key))
        .cloned()
        .ok_or_else(|| {
            DeviceError::new(
                "WORKSPACE_PROJECT_NOT_FOUND",
                format!("工作区里没有项目 {key}；先用 workspace_ensure 建项目"),
            )
        })
}

/// Per project in-process lock: two requests never create/prune worktrees of one project together.
fn project_lock(project: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>> =
        OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let key = display(project).to_lowercase();
    let mut map = map.lock().unwrap();
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn mark_has_git(data: &Path, slug: &str, has_git: bool) -> Result<(), DeviceError> {
    let mut registry = workspace::load(data)?;
    let Some(project) = registry.projects.iter_mut().find(|p| p.slug == slug) else {
        return Ok(());
    };
    if project.has_git == has_git {
        return Ok(());
    }
    project.has_git = has_git;
    workspace::save(data, &registry)
}

/// Keep the worktrees directory out of the repository through its local `info/exclude`.
fn ensure_exclude(dir: &Path) -> Result<(), DeviceError> {
    let exclude = git_ok(dir, &["rev-parse", "--git-path", "info/exclude"])?;
    let mut path = PathBuf::from(exclude.trim());
    if !path.is_absolute() {
        path = dir.join(path);
    }
    if let Some(parent) = path.parent() {
        workspace::create_dir(parent)?;
    }
    let prefix = git_ok(dir, &["rev-parse", "--show-prefix"]).unwrap_or_default();
    let prefix = prefix.trim_matches(['/', '\\'].as_ref());
    let line = if prefix.is_empty() {
        format!("/{WORKTREES_SUBDIR}/")
    } else {
        format!("/{prefix}/{WORKTREES_SUBDIR}/")
    };
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.lines().any(|l| l.trim() == line) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        std::fs::write(&path, text).map_err(|e| {
            DeviceError::new(
                "WORKSPACE_GIT_FAILED",
                format!("写不进去 git 的 exclude（{}）：{e}", display(&path)),
            )
        })?;
    }
    Ok(())
}

/// The project's base commit and repository.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Base {
    pub repo: String,
    pub commit: String,
    pub created: bool,
}

/// Same location in the plain display form (never compares verbatim `\\?\` with plain paths).
fn same_plain(a: &Path, b: &Path) -> bool {
    let a = PathBuf::from(display(a));
    let b = PathBuf::from(display(b));
    inside(&a, &b) && inside(&b, &a)
}

/// Make sure the project is a git repository with a HEAD, without touching what the person already
/// has: no repository of its own ⇒ `git init` + one baseline commit; a repository at the project
/// directory itself ⇒ its HEAD is used as the base commit and nothing is re-initialized, staged or
/// committed. A project that merely sits inside some unrelated parent repository (a checkout that
/// contains the workspace root, for example) is not that repository's project: it gets its own repo.
pub fn ensure_baseline(data: &Path, project: &workspace::Project) -> Result<Base, DeviceError> {
    let dir = workspace::real(Path::new(&project.path))?;
    let own_repo = git_probe(&dir, &["rev-parse", "--show-toplevel"]).and_then(|top| {
        let top = PathBuf::from(top.trim());
        let top = if top.is_absolute() {
            top
        } else {
            dir.join(top)
        };
        let real_top = workspace::real(&top).ok()?;
        same_plain(&real_top, &dir).then_some(real_top)
    });
    if let Some(repo) = own_repo {
        let head = git_probe(&dir, &["rev-parse", "--verify", "HEAD"])
            .filter(|h| !h.trim().is_empty())
            .map(|h| h.trim().to_string());
        let commit = match head {
            Some(commit) => commit,
            // A repository without commits: one empty commit creates the base without touching files.
            None => {
                git_commit(
                    &dir,
                    &[
                        "commit",
                        "--allow-empty",
                        "--no-gpg-sign",
                        "-m",
                        "baseline: empty project (AgentRouter workspace)",
                    ],
                )?;
                git_ok(&dir, &["rev-parse", "HEAD"])?
            }
        };
        ensure_exclude(&dir)?;
        mark_has_git(data, &project.slug, true)?;
        return Ok(Base {
            repo: display(&repo),
            commit,
            created: false,
        });
    }
    // No repository: initialize one in the project (guarded project directories only).
    git_ok(&dir, &["init"])?;
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE).map_err(|e| {
            DeviceError::new(
                "WORKSPACE_FAILED",
                format!("写不了 .gitignore（{}）：{e}", display(&gitignore)),
            )
        })?;
    }
    let _ = git_ok(&dir, &["add", "-A"]);
    let out = git_commit(
        &dir,
        &[
            "commit",
            "--no-gpg-sign",
            "-m",
            "baseline: project created by AgentRouter workspace",
        ],
    );
    let commit = match out {
        Ok(_) => git_ok(&dir, &["rev-parse", "HEAD"])?,
        Err(_) => {
            git_commit(
                &dir,
                &[
                    "commit",
                    "--allow-empty",
                    "--no-gpg-sign",
                    "-m",
                    "baseline: empty project (AgentRouter workspace)",
                ],
            )?;
            git_ok(&dir, &["rev-parse", "HEAD"])?
        }
    };
    ensure_exclude(&dir)?;
    mark_has_git(data, &project.slug, true)?;
    Ok(Base {
        repo: display(&dir),
        commit,
        created: true,
    })
}

fn view(worktree: &Worktree, reused: bool) -> Value {
    json!({
        "project": worktree.project,
        "session": worktree.session,
        "worktreeDir": worktree.path,
        "branch": worktree.branch,
        "baseCommit": worktree.base_commit,
        "reused": reused,
    })
}

/// Does this `mcp_start` ask for a workspace cwd (an explicit field or a placeholder)?
pub fn wants_workspace(args: &Value) -> bool {
    ["cwd", "project", "worktree", "worktreeSession"]
        .iter()
        .any(|k| args.get(*k).is_some_and(|v| !v.is_null()))
        || wants(args, "{{worktree}}")
        || wants(args, "{{project}}")
}

/// `workspace_baseline`: make sure the project has a git repository and a base commit (idempotent;
/// an existing repository is used read-only).
pub fn baseline(data: &Path, project_raw: &str) -> Result<Value, DeviceError> {
    let registry = workspace::load(data)?;
    let project = project_of(&registry, project_raw)?;
    let dir = workspace::real(Path::new(&project.path))?;
    let lock = project_lock(&dir);
    let _guard = lock.lock().unwrap();
    let base = ensure_baseline(data, &project)?;
    let mut value = serde_json::to_value(&base)
        .map_err(|e| DeviceError::new("WORKSPACE_FAILED", format!("结果写不出来：{e}")))?;
    value["project"] = json!(project.slug);
    Ok(value)
}

/// Worktrees the repository itself knows about (plain display paths).
fn registered_worktrees(repo: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(text) = git_probe(repo, &["worktree", "list", "--porcelain"]) {
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                out.insert(display(Path::new(path.trim())));
            }
        }
    }
    out
}

/// `workspace_worktree_create`: the session's worktree under the project, from its base commit.
/// Idempotent: an existing worktree for the session is returned (a session maps to one worktree).
pub fn create(data: &Path, project_raw: &str, session_raw: &str) -> Result<Value, DeviceError> {
    let session = session_id(session_raw)?;
    let registry = workspace::load(data)?;
    let project = project_of(&registry, project_raw)?;
    let dir = workspace::real(Path::new(&project.path))?;
    let lock = project_lock(&dir);
    let _guard = lock.lock().unwrap();
    let mut ledger = load_ledger(data)?;
    if let Some(existing) = ledger
        .worktrees
        .iter()
        .find(|w| w.session == session)
        .cloned()
    {
        if existing.project != project.slug {
            return Err(DeviceError::new(
                "WORKSPACE_WORKTREE_CONFLICT",
                format!(
                    "会话 {session} 已经属于项目 {} 的 worktree，不能再挂到 {}",
                    existing.project, project.slug
                ),
            ));
        }
        let path = PathBuf::from(&existing.path);
        if path.is_dir() && git_probe(&path, &["rev-parse", "--show-toplevel"]).is_some() {
            return Ok(view(&existing, true));
        }
        // Registered but gone from disk: prune the git entry and rebuild at the same place.
        let _ = git_ok(&dir, &["worktree", "prune"]);
        ledger.worktrees.retain(|w| w.session != session);
    }
    let base = ensure_baseline(data, &project)?;
    let worktrees_dir = dir.join(WORKTREES_SUBDIR);
    workspace::create_dir(&worktrees_dir)?;
    let wt_dir = worktrees_dir.join(&session);
    let wt_plain = PathBuf::from(display(&wt_dir));
    if wt_plain.exists() {
        let empty = std::fs::read_dir(&wt_plain)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(DeviceError::new(
                "WORKSPACE_WORKTREE_CONFLICT",
                format!(
                    "worktree 目录已经存在且不是空的：{}（不是本会话登记的目录）",
                    display(&wt_plain)
                ),
            ));
        }
        let _ = std::fs::remove_dir(&wt_plain);
    }
    let branch = format!("{BRANCH_PREFIX}{session}");
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists =
        git_probe(&dir, &["show-ref", "--verify", "--quiet", &branch_ref]).is_some();
    let shown = display(&wt_plain);
    let result = if branch_exists {
        // The branch survived an earlier worktree (change record retention): reuse it.
        git_ok(&dir, &["worktree", "add", &shown, &branch])
    } else {
        git_ok(
            &dir,
            &["worktree", "add", "-b", &branch, &shown, &base.commit],
        )
    };
    if let Err(e) = result {
        let _ = git_ok(&dir, &["worktree", "prune"]);
        return Err(e);
    }
    let head = git_ok(&wt_plain, &["rev-parse", "HEAD"])?;
    let entry = Worktree {
        project: project.slug.clone(),
        session: session.clone(),
        path: shown,
        branch,
        base_commit: head,
        created_at: iso_now(),
    };
    ledger.worktrees.retain(|w| w.session != session);
    ledger.worktrees.push(entry.clone());
    save_ledger(data, &ledger)?;
    Ok(view(&entry, false))
}

/// `workspace_worktree_list`: ledger entries (optionally one project) with whether the directory exists.
pub fn list(data: &Path, project_raw: Option<&str>) -> Result<Value, DeviceError> {
    let ledger = load_ledger(data)?;
    let project = match project_raw.map(str::trim).filter(|p| !p.is_empty()) {
        Some(raw) => Some(project_of(&workspace::load(data)?, raw)?.slug),
        None => None,
    };
    let mut out = Vec::new();
    for worktree in &ledger.worktrees {
        if project
            .as_deref()
            .is_some_and(|slug| slug != worktree.project)
        {
            continue;
        }
        let exists = Path::new(&worktree.path).is_dir();
        let mut value = serde_json::to_value(worktree)
            .map_err(|e| DeviceError::new("WORKSPACE_FAILED", format!("结果写不出来：{e}")))?;
        value["exists"] = json!(exists);
        out.push(value);
    }
    Ok(json!({"worktrees": out}))
}

/// The worktree a session is bound to, if any.
pub fn for_session(data: &Path, session: &str) -> Result<Option<Worktree>, DeviceError> {
    let session = match session_id(session) {
        Ok(session) => session,
        Err(_) => return Ok(None),
    };
    Ok(load_ledger(data)?
        .worktrees
        .into_iter()
        .find(|w| w.session == session))
}

/// `workspace_worktree_remove`: remove the worktree directory; the branch is kept by default (the
/// change record survives until the person or the cloud deletes it explicitly).
pub fn remove(
    data: &Path,
    project_raw: &str,
    session_raw: &str,
    keep_branch: bool,
) -> Result<Value, DeviceError> {
    let session = session_id(session_raw)?;
    let registry = workspace::load(data)?;
    let project = project_of(&registry, project_raw)?;
    let dir = workspace::real(Path::new(&project.path))?;
    let lock = project_lock(&dir);
    let _guard = lock.lock().unwrap();
    let mut ledger = load_ledger(data)?;
    let Some(entry) = ledger
        .worktrees
        .iter()
        .find(|w| w.session == session)
        .cloned()
    else {
        return Ok(json!({"removed": false, "session": session, "branchKept": keep_branch}));
    };
    let path = PathBuf::from(&entry.path);
    let shown = display(&path);
    let removed = if path.is_dir() {
        git_ok(&dir, &["worktree", "remove", "--force", &shown]).is_ok()
    } else {
        false
    };
    if !removed {
        let _ = git_ok(&dir, &["worktree", "prune"]);
        if path.is_dir()
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            return Err(DeviceError::new(
                "WORKSPACE_WORKTREE_REMOVE_FAILED",
                format!("worktree 目录删不掉（{}）：{e}", shown),
            ));
        }
    }
    let mut branch_kept = true;
    if !keep_branch
        && git_probe(&dir, &["rev-parse", "--verify", "--quiet", &entry.branch]).is_some()
        && git_ok(&dir, &["branch", "-D", &entry.branch]).is_ok()
    {
        branch_kept = false;
    }
    ledger.worktrees.retain(|w| w.session != session);
    save_ledger(data, &ledger)?;
    Ok(json!({
        "removed": true,
        "session": entry.session,
        "project": entry.project,
        "worktreeDir": entry.path,
        "branch": entry.branch,
        "branchKept": branch_kept,
    }))
}

/// `workspace_worktree_prune`: drop stale worktrees (missing directories, directories not registered
/// with the repository or the ledger) older than `older_than_hours` (default 72 h). Branches are kept
/// unless `delete_branches` is true; the base branch is never touched.
pub fn prune(
    data: &Path,
    project_raw: &str,
    older_than_hours: Option<u64>,
    delete_branches: bool,
) -> Result<Value, DeviceError> {
    let registry = workspace::load(data)?;
    let project = project_of(&registry, project_raw)?;
    let dir = workspace::real(Path::new(&project.path))?;
    let lock = project_lock(&dir);
    let _guard = lock.lock().unwrap();
    let _ = git_ok(&dir, &["worktree", "prune"]);
    let registered = registered_worktrees(&dir);
    let threshold = older_than_hours
        .map(|h| Duration::from_secs(h.saturating_mul(60 * 60)))
        .unwrap_or(PRUNE_AGE);
    let mut ledger = load_ledger(data)?;
    let mut removed = Vec::new();
    let mut kept = 0usize;
    let mut branches = Vec::new();
    ledger.worktrees.retain(|worktree| {
        if worktree.project != project.slug {
            return true;
        }
        let path = PathBuf::from(&worktree.path);
        let old_enough = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() >= threshold)
            .unwrap_or(true);
        if path.is_dir() && registered.contains(&display(&path)) && !old_enough {
            kept += 1;
            return true;
        }
        let removed_ok = if path.is_dir() {
            git_ok(&dir, &["worktree", "remove", "--force", &display(&path)]).is_ok()
                || std::fs::remove_dir_all(&path).is_ok()
        } else {
            true
        };
        if removed_ok {
            removed.push(worktree.path.clone());
            branches.push(worktree.branch.clone());
            false
        } else {
            kept += 1;
            true
        }
    });
    // Directories under .worktrees/ that no ledger entry claims: stale leftovers of a crashed run.
    let worktrees_dir = dir.join(WORKTREES_SUBDIR);
    let known: HashSet<String> = ledger
        .worktrees
        .iter()
        .map(|w| display(Path::new(&w.path)))
        .collect();
    if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let shown = display(&path);
            if known.contains(&shown) || registered.contains(&shown) {
                continue;
            }
            let old_enough = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() >= threshold)
                .unwrap_or(false);
            if !old_enough {
                kept += 1;
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                removed.push(shown);
            } else {
                kept += 1;
            }
        }
    }
    let mut deleted_branches = Vec::new();
    if delete_branches {
        for branch in branches {
            let still_used = registered_worktrees(&dir).iter().any(|path| {
                git_probe(Path::new(path), &["rev-parse", "--abbrev-ref", "HEAD"]).as_deref()
                    == Some(branch.as_str())
            });
            if !still_used && git_ok(&dir, &["branch", "-D", &branch]).is_ok() {
                deleted_branches.push(branch);
            }
        }
    }
    save_ledger(data, &ledger)?;
    Ok(json!({
        "removed": removed,
        "kept": kept,
        "deletedBranches": deleted_branches,
        "olderThanHours": threshold.as_secs() / 3600,
    }))
}

/// The cwd a bound MCP server should run in (project or the session's worktree).
#[derive(Debug, Clone)]
pub struct Cwd {
    pub path: PathBuf,
    pub project_slug: String,
    pub worktree: Option<Worktree>,
}

/// `mcp_start` arguments may name the cwd: `cwd` (a registered project/worktree), `project` (project
/// directory) or `worktree: true` (this session's worktree, or `worktreeSession`). Without any of
/// them the old behaviour stays: the first linked folder.
pub fn resolve_cwd(data: &Path, session: &str, args: &Value) -> Result<Option<Cwd>, DeviceError> {
    let registry = workspace::load(data)?;
    let ledger = load_ledger(data)?;
    let explicit = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let project_arg = args
        .get("project")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let wants_worktree = args
        .get("worktree")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let worktree_session = args
        .get("worktreeSession")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(session);
    if let Some(raw) = explicit {
        gate::check_syntax(raw)?;
        let real = workspace::real(Path::new(raw))?;
        let plain = display(&real);
        if let Some(worktree) = ledger
            .worktrees
            .iter()
            .find(|w| same_path(&w.path, &PathBuf::from(display(&real))))
            .cloned()
        {
            return Ok(Some(Cwd {
                path: real,
                project_slug: worktree.project.clone(),
                worktree: Some(worktree),
            }));
        }
        if let Some(project) = registry
            .projects
            .iter()
            .find(|p| same_path(&p.path, &PathBuf::from(display(&real))))
            .cloned()
        {
            return Ok(Some(Cwd {
                path: real,
                project_slug: project.slug,
                worktree: None,
            }));
        }
        return Err(DeviceError::new(
            "WORKSPACE_CWD_INVALID",
            format!("{plain} 不是工作区里登记的项目或 worktree 目录"),
        )
        .next("用 workspace_ensure / workspace_worktree_create 得到路径，不要传任意绝对路径。"));
    }
    if wants_worktree {
        let worktree_session = session_id(worktree_session).map_err(|_| {
            DeviceError::new(
                "WORKSPACE_SESSION_INVALID",
                "worktree: true 需要当前会话 id（或 worktreeSession）",
            )
        })?;
        let Some(worktree) = ledger
            .worktrees
            .into_iter()
            .find(|w| w.session == worktree_session)
        else {
            return Err(DeviceError::new(
                "WORKSPACE_NO_WORKTREE",
                format!("会话 {worktree_session} 还没有 worktree"),
            )
            .next("先调用 workspace_worktree_create {project, session}。"));
        };
        let path = workspace::real(Path::new(&worktree.path))?;
        return Ok(Some(Cwd {
            path,
            project_slug: worktree.project.clone(),
            worktree: Some(worktree),
        }));
    }
    if let Some(raw) = project_arg {
        let project = project_of(&registry, raw)?;
        let path = workspace::real(Path::new(&project.path))?;
        return Ok(Some(Cwd {
            path,
            project_slug: project.slug,
            worktree: None,
        }));
    }
    Ok(None)
}

/// `{{worktree}}`/`{{project}}` for this start, from the cwd resolution (session bound elsewhere).
pub fn placeholder_pairs(
    data: &Path,
    session: &str,
    cwd: Option<&Cwd>,
) -> Result<Vec<(String, String)>, DeviceError> {
    let (worktree_path, project_path) = match cwd {
        Some(cwd) => {
            let worktree = cwd.worktree.as_ref().map(|w| w.path.clone());
            let registry = workspace::load(data)?;
            let project = registry
                .projects
                .iter()
                .find(|p| p.slug == cwd.project_slug)
                .map(|p| p.path.clone());
            (worktree, project)
        }
        None => match for_session(data, session)? {
            Some(worktree) => {
                let registry = workspace::load(data)?;
                let project = registry
                    .projects
                    .iter()
                    .find(|p| p.slug == worktree.project)
                    .map(|p| p.path.clone());
                (Some(worktree.path), project)
            }
            None => (None, None),
        },
    };
    let mut pairs = Vec::new();
    if let Some(path) = worktree_path {
        pairs.push(("{{worktree}}".to_string(), path));
    }
    if let Some(path) = project_path {
        pairs.push(("{{project}}".to_string(), path));
    }
    Ok(pairs)
}

fn strings(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|v| out.extend(strings(v))),
        Value::Object(m) => m.values().for_each(|v| out.extend(strings(v))),
        _ => {}
    }
    out
}

/// Does any string in the definition use this placeholder?
fn wants(args: &Value, placeholder: &str) -> bool {
    strings(args).iter().any(|s| s.contains(placeholder))
}

/// Replace `{{worktree}}`/`{{project}}` in every string of the definition; a placeholder with no
/// value is a clear error (the cloud asked for a worktree the device does not have).
pub fn substitute(args: &mut Value, pairs: &[(String, String)]) -> Result<(), DeviceError> {
    for (placeholder, what) in [
        ("{{worktree}}", "workspace_worktree_create"),
        ("{{project}}", "workspace_ensure"),
    ] {
        if wants(args, placeholder) && !pairs.iter().any(|(p, _)| p == placeholder) {
            return Err(DeviceError::new(
                "WORKSPACE_NO_WORKTREE",
                format!("{placeholder} 没有可替换的值（还没有项目/worktree）"),
            )
            .next(format!("先调用 {what}。")));
        }
    }
    fn walk(value: &mut Value, pairs: &[(String, String)]) {
        match value {
            Value::String(s) => {
                for (placeholder, replacement) in pairs {
                    if s.contains(placeholder.as_str()) {
                        *s = s.replace(placeholder.as_str(), replacement);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|v| walk(v, pairs)),
            Value::Object(m) => m.values_mut().for_each(|v| walk(v, pairs)),
            _ => {}
        }
    }
    walk(args, pairs);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_become_paths_and_branches() {
        assert_eq!(session_id("ags_1234").unwrap(), "ags_1234");
        assert_eq!(session_id("s-1").unwrap(), "s-1");
        for bad in ["", "../x", "a/b", "a b", "a.b", "会话", &"x".repeat(65)] {
            assert_eq!(
                session_id(bad).unwrap_err().code,
                "WORKSPACE_SESSION_INVALID",
                "{bad}"
            );
        }
    }

    #[test]
    fn substitute_replaces_and_refuses_missing_values() {
        let mut args = json!({
            "command": "tool",
            "args": ["--repo", "{{project}}", "--path", "{{worktree}}/src"],
            "env": {"WORKSPACE": "{{project}}"},
        });
        substitute(
            &mut args,
            &[
                ("{{project}}".into(), "C:\\ws\\mod".into()),
                ("{{worktree}}".into(), "C:\\ws\\mod\\.worktrees\\s1".into()),
            ],
        )
        .unwrap();
        assert_eq!(args["args"][1], "C:\\ws\\mod");
        assert_eq!(args["args"][3], "C:\\ws\\mod\\.worktrees\\s1/src");
        assert_eq!(args["env"]["WORKSPACE"], "C:\\ws\\mod");
        let mut missing = json!({"args": ["{{worktree}}"]});
        assert_eq!(
            substitute(&mut missing, &[]).unwrap_err().code,
            "WORKSPACE_NO_WORKTREE"
        );
    }
}
