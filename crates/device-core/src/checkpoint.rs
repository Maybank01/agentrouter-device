//! Checkpoints and undo (DEVICE-PROTOCOL.md §5.7). The default level does not ask before changes, so
//! every turn's first change to a linked folder is preceded by a checkpoint, and `agentrouter undo`
//! puts the folder back.
//!
//! - Git repositories: the whole work tree (`.gitignore` respected, untracked files included) becomes a
//!   commit under the hidden ref `refs/agentrouter/cp/<id>`, built with a temporary index, so the
//!   person's branch, staging area and files are never touched.
//! - Other folders: files the file tools change are copied first; before a command the folder is
//!   copied if it is small enough. Beyond the caps the checkpoint is `partial`.
//!
//! Checkpoints live in the helper's data folder (never reachable through the file tools); the last 50
//! are kept.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::gate::display;
use crate::util::{iso_now, short_id};

const KEEP: usize = 50;
/// Without a turn id, a pause this long starts a new turn.
const TURN_GAP: Duration = Duration::from_secs(300);
const UNTRACKED_FILE_MAX: u64 = 50 << 20;
const UNTRACKED_TOTAL_MAX: u64 = 200 << 20;
const BACKUP_MAX: u64 = 100 << 20;
const COPY_MAX_BYTES: u64 = 50 << 20;
const COPY_MAX_FILES: usize = 5000;
/// Regenerable folders a folder copy skips.
const SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "dist",
    "build",
    "__pycache__",
    ".next",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub session: String,
    pub turn: String,
    pub folder: String,
    /// "git" or "files".
    pub kind: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub copied: bool,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Backup {
    path: String,
    /// The copy's file name inside the checkpoint, or none when the file did not exist.
    copy: Option<String>,
}

/// What is about to change.
pub enum Change<'a> {
    Files(&'a [PathBuf]),
    Command,
}

struct Active {
    meta: Meta,
    backed: HashSet<PathBuf>,
    backup_bytes: u64,
}

#[derive(Default)]
struct State {
    active: HashMap<(String, String, PathBuf), Active>,
    /// Per conversation without turn ids: generation and last change.
    auto: HashMap<String, (u64, Instant)>,
}

pub struct Checkpoints {
    dir: PathBuf,
    state: Mutex<State>,
}

fn git(repo: &Path, args: &[&str], index: Option<&Path>) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotepath=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "AgentRouter")
        .env("GIT_AUTHOR_EMAIL", "checkpoint@agentrouter.top")
        .env("GIT_COMMITTER_NAME", "AgentRouter")
        .env("GIT_COMMITTER_EMAIL", "checkpoint@agentrouter.top")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Is this folder in a git work tree (and not ignored by it)? Git then works from the folder itself,
/// with `.` as the pathspec, so a linked subfolder never snapshots or restores anything outside it.
pub fn repo_of(folder: &Path) -> Option<PathBuf> {
    if git(folder, &["rev-parse", "--is-inside-work-tree"], None)? != "true" {
        return None;
    }
    let prefix = git(folder, &["rev-parse", "--show-prefix"], None)?;
    let prefix = prefix.trim_end_matches('/');
    if !prefix.is_empty() {
        let top = git(folder, &["rev-parse", "--show-toplevel"], None)?;
        if git(Path::new(&top), &["check-ignore", "-q", "--", prefix], None).is_some() {
            return None;
        }
    }
    Some(folder.to_path_buf())
}

/// Snapshot the work tree into a commit (not referenced yet). Returns the commit and whether large
/// untracked files were left out.
fn snapshot(repo: &Path, scratch: &Path) -> Option<(String, bool)> {
    let index = scratch.join(format!("index-{}", short_id("")));
    let head = git(
        repo,
        &["rev-parse", "--verify", "-q", "HEAD^{commit}"],
        None,
    );
    if head.is_some() {
        git(repo, &["read-tree", "HEAD"], Some(&index))?;
    }
    let untracked = git(
        repo,
        &["ls-files", "-o", "--exclude-standard", "-z", "--", "."],
        None,
    )?;
    let mut partial = false;
    let mut total = 0u64;
    let mut keep: Vec<&str> = Vec::new();
    for name in untracked.split('\0').filter(|n| !n.is_empty()) {
        let size = std::fs::metadata(repo.join(name))
            .map(|m| m.len())
            .unwrap_or(0);
        if size > UNTRACKED_FILE_MAX || total + size > UNTRACKED_TOTAL_MAX {
            partial = true;
            continue;
        }
        total += size;
        keep.push(name);
    }
    let added = if partial {
        let list = scratch.join(format!("paths-{}", short_id("")));
        let _ = std::fs::write(&list, keep.join("\0"));
        let tracked = git(repo, &["add", "-u", "--", "."], Some(&index));
        let rest = keep.is_empty()
            || git(
                repo,
                &[
                    "add",
                    &format!("--pathspec-from-file={}", list.display()),
                    "--pathspec-file-nul",
                ],
                Some(&index),
            )
            .is_some();
        let _ = std::fs::remove_file(&list);
        tracked.is_some() && rest
    } else {
        git(repo, &["add", "-A", "--", "."], Some(&index)).is_some()
    };
    let tree = if added {
        git(repo, &["write-tree"], Some(&index))
    } else {
        None
    };
    let _ = std::fs::remove_file(&index);
    let tree = tree?;
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "AgentRouter checkpoint"];
    if let Some(h) = head.as_deref() {
        args.extend(["-p", h]);
    }
    let commit = git(repo, &args, None)?;
    Some((commit, partial))
}

fn copy_tree(from: &Path, to: &Path, budget: &mut (u64, usize)) -> bool {
    let Ok(entries) = std::fs::read_dir(from) else {
        return true;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let name = entry.file_name();
        if meta.is_dir() {
            if SKIP.iter().any(|s| name.eq_ignore_ascii_case(s)) {
                continue;
            }
            if !copy_tree(&path, &to.join(&name), budget) {
                return false;
            }
        } else if meta.is_file() {
            budget.0 += meta.len();
            budget.1 += 1;
            if budget.0 > COPY_MAX_BYTES || budget.1 > COPY_MAX_FILES {
                return false;
            }
            let _ = std::fs::create_dir_all(to);
            if std::fs::copy(&path, to.join(&name)).is_err() {
                return false;
            }
        }
    }
    true
}

/// Files under `root` a folder copy covers (relative, skipping the regenerable folders).
fn tree_files(root: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if !SKIP
                .iter()
                .any(|s| entry.file_name().eq_ignore_ascii_case(s))
            {
                tree_files(&path, base, out);
            }
        } else if meta.is_file()
            && let Ok(rel) = path.strip_prefix(base)
        {
            out.push(rel.to_path_buf());
        }
    }
}

impl Checkpoints {
    pub fn new(data_dir: &Path) -> Checkpoints {
        Checkpoints {
            dir: data_dir.join("checkpoints"),
            state: Mutex::new(State::default()),
        }
    }

    fn save_meta(&self, meta: &Meta) {
        let dir = self.dir.join(&meta.id);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(meta).unwrap_or_default(),
        );
    }

    /// Before a change in `folder` (a linked folder, real path): the checkpoint of this turn, made now
    /// if this is the turn's first change there. Returns its id.
    pub fn before(
        &self,
        session: &str,
        turn: Option<&str>,
        folder: &Path,
        change: Change<'_>,
    ) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        let turn_key = match turn.filter(|t| !t.is_empty()) {
            Some(t) => crate::util::clip(t, 64),
            None => {
                let now = Instant::now();
                let entry = state.auto.entry(session.to_string()).or_insert((0, now));
                if now.duration_since(entry.1) > TURN_GAP {
                    entry.0 += 1;
                }
                entry.1 = now;
                format!("auto-{}", entry.0)
            }
        };
        let key = (session.to_string(), turn_key.clone(), folder.to_path_buf());
        if !state.active.contains_key(&key) {
            let id = short_id("cp_");
            let _ = std::fs::create_dir_all(self.dir.join(&id));
            let mut meta = Meta {
                id: id.clone(),
                session: session.to_string(),
                turn: turn_key,
                folder: display(folder),
                kind: "files".into(),
                repo: None,
                commit: None,
                partial: false,
                copied: false,
                at: iso_now(),
            };
            if let Some(repo) = repo_of(Path::new(&display(folder)))
                && let Some((commit, partial)) = snapshot(&repo, &self.dir.join(&id))
                && git(
                    &repo,
                    &["update-ref", &format!("refs/agentrouter/cp/{id}"), &commit],
                    None,
                )
                .is_some()
            {
                meta.kind = "git".into();
                meta.repo = Some(display(&repo));
                meta.commit = Some(commit);
                meta.partial = partial;
            }
            self.save_meta(&meta);
            state.active.insert(
                key.clone(),
                Active {
                    meta,
                    backed: HashSet::new(),
                    backup_bytes: 0,
                },
            );
            drop(state);
            self.prune();
            state = self.state.lock().unwrap();
        }
        let active = state.active.get_mut(&key)?;
        if active.meta.kind == "files" {
            let dir = self.dir.join(&active.meta.id);
            match change {
                Change::Files(paths) => {
                    for path in paths {
                        if active.backed.contains(path) {
                            continue;
                        }
                        active.backed.insert(path.clone());
                        let size = std::fs::metadata(path).map(|m| m.len()).ok();
                        let copy = match size {
                            Some(size) if active.backup_bytes + size <= BACKUP_MAX => {
                                let name = format!("{}.bak", active.backed.len());
                                let _ = std::fs::create_dir_all(dir.join("files"));
                                std::fs::copy(path, dir.join("files").join(&name))
                                    .ok()
                                    .map(|_| {
                                        active.backup_bytes += size;
                                        name
                                    })
                            }
                            Some(_) => {
                                active.meta.partial = true;
                                continue;
                            }
                            None => None,
                        };
                        let line = serde_json::to_string(&Backup {
                            path: display(path),
                            copy,
                        })
                        .unwrap_or_default();
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join("files.jsonl"))
                        {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                }
                Change::Command => {
                    if !active.meta.copied && !active.meta.partial {
                        let mut budget = (0, 0);
                        if copy_tree(folder, &dir.join("tree"), &mut budget) {
                            active.meta.copied = true;
                        } else {
                            let _ = std::fs::remove_dir_all(dir.join("tree"));
                            active.meta.partial = true;
                        }
                    }
                }
            }
            let meta = active.meta.clone();
            self.save_meta(&meta);
        }
        Some(active.meta.id.clone())
    }

    /// Keep the newest checkpoints only.
    fn prune(&self) {
        let mut all = list(self.dir.parent().unwrap_or(&self.dir));
        if all.len() <= KEEP {
            return;
        }
        all.sort_by(|a, b| b.at.cmp(&a.at));
        for old in all.into_iter().skip(KEEP) {
            if let Some(repo) = &old.repo {
                let _ = git(
                    Path::new(repo),
                    &[
                        "update-ref",
                        "-d",
                        &format!("refs/agentrouter/cp/{}", old.id),
                    ],
                    None,
                );
            }
            let _ = std::fs::remove_dir_all(self.dir.join(&old.id));
        }
    }
}

/// The checkpoints kept under a data folder, newest first.
pub fn list(data_dir: &Path) -> Vec<Meta> {
    let dir = data_dir.join("checkpoints");
    let mut out: Vec<Meta> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read(e.path().join("meta.json")).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    out.sort_by(|a, b| b.at.cmp(&a.at));
    out
}

/// What undoing a checkpoint changes: (what, path) pairs, plus the snapshot of now (git) to keep.
pub struct Plan {
    pub meta: Meta,
    pub changes: Vec<(String, String)>,
    now_commit: Option<String>,
}

/// Work out what `undo` would change, without changing anything.
pub fn plan(data_dir: &Path, id: &str) -> Result<Plan, String> {
    let meta = list(data_dir)
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("没有这个检查点：{id}"))?;
    let scratch = data_dir.join("checkpoints").join(&meta.id);
    match (meta.kind.as_str(), &meta.repo, &meta.commit) {
        ("git", Some(repo), Some(commit)) => {
            let repo = Path::new(repo);
            let (now, _) = snapshot(repo, &scratch).ok_or("现在的状态没法快照（git 出错）")?;
            let diff = git(
                repo,
                &[
                    "diff",
                    "--name-status",
                    "--no-renames",
                    "--relative",
                    "-z",
                    &now,
                    commit,
                    "--",
                    ".",
                ],
                None,
            )
            .ok_or("git diff 出错")?;
            let parts: Vec<&str> = diff.split('\0').filter(|p| !p.is_empty()).collect();
            let changes = parts
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| {
                    let what = match c[0] {
                        "A" => "恢复",
                        "D" => "删除",
                        _ => "改回",
                    };
                    (what.to_string(), c[1].to_string())
                })
                .collect();
            Ok(Plan {
                meta,
                changes,
                now_commit: Some(now),
            })
        }
        _ => {
            let mut changes = Vec::new();
            let mut seen = HashSet::new();
            for backup in backups(&scratch) {
                if seen.insert(backup.path.clone()) {
                    let what = if backup.copy.is_some() {
                        "改回"
                    } else {
                        "删除"
                    };
                    changes.push((what.to_string(), backup.path));
                }
            }
            if meta.copied {
                let folder = PathBuf::from(&meta.folder);
                let mut now = Vec::new();
                tree_files(&folder, &folder, &mut now);
                let mut then = Vec::new();
                let tree = scratch.join("tree");
                tree_files(&tree, &tree, &mut then);
                let then_set: HashSet<&PathBuf> = then.iter().collect();
                for rel in &now {
                    if !then_set.contains(rel) {
                        changes.push(("删除".into(), display(&folder.join(rel))));
                    }
                }
                for rel in &then {
                    let current = std::fs::read(folder.join(rel)).ok();
                    let old = std::fs::read(tree.join(rel)).ok();
                    if current != old {
                        changes.push(("改回".into(), display(&folder.join(rel))));
                    }
                }
            }
            Ok(Plan {
                meta,
                changes,
                now_commit: None,
            })
        }
    }
}

fn backups(scratch: &Path) -> Vec<Backup> {
    std::fs::read_to_string(scratch.join("files.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Put the folder back to the checkpoint. A git undo first keeps the current state as a new
/// checkpoint, so the undo itself can be undone; returns its id.
pub fn undo(data_dir: &Path, plan: &Plan) -> Result<Option<String>, String> {
    let meta = &plan.meta;
    let scratch = data_dir.join("checkpoints").join(&meta.id);
    if let (Some(repo), Some(commit), Some(now)) = (&meta.repo, &meta.commit, &plan.now_commit) {
        let repo = Path::new(repo);
        let keep = short_id("cp_");
        git(
            repo,
            &["update-ref", &format!("refs/agentrouter/cp/{keep}"), now],
            None,
        )
        .ok_or("没能保存现在的状态")?;
        let saved = Meta {
            id: keep.clone(),
            session: "undo".into(),
            turn: format!("before undo of {}", meta.id),
            folder: meta.folder.clone(),
            kind: "git".into(),
            repo: meta.repo.clone(),
            commit: Some(now.clone()),
            partial: false,
            copied: false,
            at: iso_now(),
        };
        let dir = data_dir.join("checkpoints").join(&keep);
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            dir.join("meta.json"),
            serde_json::to_vec_pretty(&saved).unwrap_or_default(),
        );
        let index = scratch.join(format!("index-{}", short_id("")));
        git(repo, &["read-tree", commit], Some(&index)).ok_or("git read-tree 出错")?;
        for (what, path) in &plan.changes {
            if what == "删除" {
                let _ = std::fs::remove_file(repo.join(path));
            }
        }
        let restore: Vec<&str> = plan
            .changes
            .iter()
            .filter(|(what, _)| what != "删除")
            .map(|(_, p)| p.as_str())
            .collect();
        let mut ok = true;
        for chunk in restore.chunks(100) {
            let mut args = vec!["checkout-index", "-f", "--"];
            args.extend(chunk);
            ok &= git(repo, &args, Some(&index)).is_some();
        }
        let _ = std::fs::remove_file(&index);
        if !ok {
            return Err("有的文件没能恢复（git checkout-index 出错）".into());
        }
        return Ok(Some(keep));
    }
    let mut done = HashSet::new();
    for backup in backups(&scratch) {
        if !done.insert(backup.path.clone()) {
            continue;
        }
        let path = PathBuf::from(&backup.path);
        match &backup.copy {
            Some(copy) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::copy(scratch.join("files").join(copy), &path)
                    .map_err(|e| format!("{}：{e}", backup.path))?;
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    if meta.copied {
        let folder = PathBuf::from(&meta.folder);
        let tree = scratch.join("tree");
        let mut now = Vec::new();
        tree_files(&folder, &folder, &mut now);
        let mut then = Vec::new();
        tree_files(&tree, &tree, &mut then);
        let then_set: HashSet<&PathBuf> = then.iter().collect();
        for rel in &now {
            if !then_set.contains(rel) {
                let _ = std::fs::remove_file(folder.join(rel));
            }
        }
        for rel in &then {
            let target = folder.join(rel);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(tree.join(rel), &target)
                .map_err(|e| format!("{}：{e}", display(&target)))?;
        }
    }
    Ok(None)
}

/// A checkpoint as the protocol shows it.
pub fn summary(meta: &Meta) -> serde_json::Value {
    json!({"checkpoint": meta.id, "folder": meta.folder, "kind": meta.kind, "partial": meta.partial, "at": meta.at})
}
