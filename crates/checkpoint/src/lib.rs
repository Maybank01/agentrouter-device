//! Checkpoints and undo for a linked folder (docs/LINKER-MCP.md §7.2, issue #47): a **shadow git repository
//! outside the folder** (`<data>/checkpoints/<lnk>.git`, work tree = the folder) takes a snapshot before the first
//! non-read-only call of a turn, and `agentrouter undo` puts the folder back. This is the pattern Claude Code and
//! Cline use.
//!
//! - The person's own `.git` is never touched or read: git runs with `GIT_DIR` = the shadow repository, system and
//!   global config off (`GIT_CONFIG_NOSYSTEM`, an empty `GIT_CONFIG_GLOBAL`, `HOME`/`XDG_CONFIG_HOME` inside the
//!   shadow), `core.hooksPath` pointing at an empty folder of ours and `core.fsmonitor=false`. Nothing configured
//!   in the folder (hooks, fsmonitor, filter or diff drivers) can make the unconfined device run code.
//! - `.gitignore` in the folder is respected; a file over [`FILE_MAX`] is left out; a folder over [`TOTAL_MAX`] gets
//!   no checkpoint (the call still runs; the feed says so).
//! - Undo restores the folder to a checkpoint (files the turn created are removed, deleted ones come back);
//!   before that it snapshots the current state, so an undo can itself be undone.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A file bigger than this is not snapshotted.
pub const FILE_MAX: u64 = 50 << 20;
/// A folder whose snapshot would be bigger than this gets no checkpoint.
pub const TOTAL_MAX: u64 = 2 << 30;
/// Checkpoints kept per link (older ones become unreachable and are pruned).
pub const KEEP: usize = 100;

const TURN: &str = "turn";
const BEFORE_UNDO: &str = "before-undo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Short commit id (what `agentrouter undo <id>` takes).
    pub id: String,
    /// Unix seconds.
    pub at: i64,
    /// `turn` (before a turn's changes) or `before-undo`.
    pub kind: String,
    pub label: String,
}

/// What an undo did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    /// The checkpoint the folder now matches.
    pub restored: Checkpoint,
    /// The state just before the undo (undo this to redo).
    pub saved: Checkpoint,
}

/// The shadow repository of one linked folder.
#[derive(Debug, Clone)]
pub struct Shadow {
    git_dir: PathBuf,
    work_tree: PathBuf,
    git: Option<PathBuf>,
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// `git` from PATH, never from the folder itself.
fn find_git(folder: &Path) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe"] } else { &[""] };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute() && !d.starts_with(folder))
        .find_map(|d| {
            exts.iter()
                .map(|e| d.join(format!("git{e}")))
                .find(|c| c.is_file())
        })
}

impl Shadow {
    /// The shadow repository of link `link` (its folder `folder`, canonical) in the device data folder.
    pub fn new(data_dir: &Path, link: &str, folder: &Path) -> Shadow {
        Shadow {
            git_dir: data_dir.join("checkpoints").join(format!("{link}.git")),
            work_tree: folder.to_path_buf(),
            git: find_git(folder),
        }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// One isolated git command.
    fn git(&self) -> Result<Command, String> {
        let git = self
            .git
            .as_ref()
            .ok_or("这台电脑没有 git（检查点要用它；装上 Git 后再试）")?;
        let mut cmd = Command::new(git);
        // Nothing of the person's git setup applies: no inherited GIT_* variables, no system/global/user config.
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") {
                cmd.env_remove(k);
            }
        }
        cmd.env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("HOME", &self.git_dir)
            .env("XDG_CONFIG_HOME", &self.git_dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .args([
                "-c",
                &format!("core.hooksPath={}", self.git_dir.join("no-hooks").display()),
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.safecrlf=false",
                "-c",
                "core.quotepath=false",
                "-c",
                "core.longpaths=true",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "user.name=AgentRouter",
                "-c",
                "user.email=checkpoint@agentrouter.invalid",
                "-c",
                "safe.directory=*",
            ])
            .current_dir(&self.git_dir)
            .stdin(Stdio::null());
        Ok(cmd)
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self
            .git()?
            .args(args)
            .output()
            .map_err(|e| format!("git 起不来：{e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git {} 没成功：{}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn ensure(&self) -> Result<(), String> {
        if self.git_dir.join("HEAD").is_file() {
            return Ok(());
        }
        std::fs::create_dir_all(self.git_dir.join("no-hooks"))
            .map_err(|e| format!("建不了检查点仓库：{e}"))?;
        self.run(&["init", "-q"])?;
        // Hooks of the shadow repository itself: none, ever.
        let _ = std::fs::remove_dir_all(self.git_dir.join("hooks"));
        Ok(())
    }

    /// Files over [`FILE_MAX`] stay out (in the shadow's own `info/exclude`); the snapshot size, or an error when
    /// it is over [`TOTAL_MAX`].
    fn limit_sizes(&self) -> Result<u64, String> {
        // Only the folder's own .gitignore files: `--exclude-standard` would also apply our info/exclude, and a big
        // file left out last time would then be missed, snapshotted, and removed by the next restore.
        let listed = self.run(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-per-directory=.gitignore",
        ])?;
        let mut total = 0u64;
        let mut big = Vec::new();
        for rel in listed.split('\0').filter(|p| !p.is_empty()) {
            let Ok(meta) = std::fs::symlink_metadata(self.work_tree.join(rel)) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if meta.len() > FILE_MAX {
                big.push(rel.to_string());
            } else {
                total += meta.len();
            }
        }
        let info = self.git_dir.join("info");
        let _ = std::fs::create_dir_all(&info);
        let mut exclude = String::from("# AgentRouter checkpoints: files too big to snapshot\n");
        for rel in &big {
            // Anchored, with glob characters escaped.
            let escaped: String = rel
                .chars()
                .flat_map(|c| match c {
                    '*' | '?' | '[' | '\\' | '!' | '#' => vec!['\\', c],
                    c => vec![c],
                })
                .collect();
            exclude.push('/');
            exclude.push_str(&escaped);
            exclude.push('\n');
        }
        std::fs::write(info.join("exclude"), exclude).map_err(|e| e.to_string())?;
        if total > TOTAL_MAX {
            return Err(format!(
                "文件夹太大（{} MB），这一轮没有检查点",
                total >> 20
            ));
        }
        Ok(total)
    }

    fn commit(&self, kind: &str, label: &str) -> Result<Checkpoint, String> {
        self.ensure()?;
        self.limit_sizes()?;
        // The whole work tree (git runs outside it, so no pathspec); unreadable files are skipped.
        self.run(&["add", "-A", "--ignore-errors"])
            .or_else(|_| self.run(&["add", "-A"]))?;
        let label = label.replace(['\n', '\r'], " ");
        let label: String = label.chars().take(120).collect();
        self.run(&[
            "commit",
            "-q",
            "--allow-empty",
            "--no-verify",
            "-m",
            &format!("{kind}: {label}"),
        ])?;
        self.prune();
        self.list(1)?
            .into_iter()
            .next()
            .ok_or_else(|| "检查点没有写进去".to_string())
    }

    /// Snapshot the folder before a turn's changes.
    pub fn snapshot(&self, label: &str) -> Result<Checkpoint, String> {
        self.commit(TURN, label)
    }

    /// The newest checkpoints first (at most `n`).
    pub fn list(&self, n: usize) -> Result<Vec<Checkpoint>, String> {
        if !self.git_dir.join("HEAD").is_file() {
            return Ok(Vec::new());
        }
        let out = match self.run(&["log", &format!("-{n}"), "--format=%h%x1f%ct%x1f%s"]) {
            Ok(out) => out,
            // No commit yet.
            Err(_) => return Ok(Vec::new()),
        };
        Ok(out
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\u{1f}');
                let id = parts.next()?.to_string();
                let at = parts.next()?.parse().ok()?;
                let subject = parts.next().unwrap_or("");
                let (kind, label) = subject.split_once(": ").unwrap_or((TURN, subject));
                Some(Checkpoint {
                    id,
                    at,
                    kind: kind.to_string(),
                    label: label.to_string(),
                })
            })
            .collect())
    }

    fn tree_of(&self, rev: &str) -> Result<String, String> {
        Ok(self
            .run(&["rev-parse", &format!("{rev}^{{tree}}")])?
            .trim()
            .to_string())
    }

    /// Put the folder back to checkpoint `id`, or (no id) to the newest turn checkpoint that differs from the
    /// folder now — so repeated undos step back one turn at a time.
    pub fn undo(&self, id: Option<&str>) -> Result<Undone, String> {
        let history = self.list(KEEP)?;
        if history.is_empty() {
            return Err("这个文件夹还没有检查点".into());
        }
        let saved = self.commit(BEFORE_UNDO, "撤销前的状态")?;
        let now_tree = self.tree_of("HEAD")?;
        let target = match id {
            Some(id) => history
                .iter()
                .find(|c| c.id.starts_with(id) || id.starts_with(&c.id))
                .cloned()
                .ok_or_else(|| format!("没有这个检查点：{id}"))?,
            None => history
                .iter()
                .filter(|c| c.kind == TURN)
                .find(|c| self.tree_of(&c.id).is_ok_and(|t| t != now_tree))
                .cloned()
                .ok_or("没有可以撤销的改动")?,
        };
        // The index holds the folder as it is now (`saved`): read-tree -u --reset makes the folder match the
        // target, removing files the target does not have and restoring the ones it does. Ignored files stay.
        self.run(&["read-tree", "-u", "--reset", &target.id])?;
        Ok(Undone {
            restored: target,
            saved,
        })
    }

    /// Keep the last [`KEEP`] checkpoints reachable; drop the rest's objects now and then.
    fn prune(&self) {
        let Ok(count) = self.run(&["rev-list", "--count", "HEAD"]) else {
            return;
        };
        let count: usize = count.trim().parse().unwrap_or(0);
        if count <= KEEP * 2 {
            return;
        }
        // Re-root the history at the KEEP-th newest commit (its tree only), then let gc drop the rest.
        let Ok(base) = self.run(&["rev-parse", &format!("HEAD~{}", KEEP - 1)]) else {
            return;
        };
        let base = base.trim();
        let Ok(tree) = self.tree_of(base) else { return };
        let Ok(root) = self.run(&[
            "commit-tree",
            &tree,
            "-m",
            "turn: (older checkpoints pruned)",
        ]) else {
            return;
        };
        let root = root.trim();
        let rebuilt = self
            .run(&["rev-list", "--reverse", &format!("{base}..HEAD")])
            .ok()
            .and_then(|revs| {
                let mut parent = root.to_string();
                for rev in revs.lines().filter(|r| !r.is_empty()) {
                    let tree = self.tree_of(rev).ok()?;
                    let msg = self.run(&["log", "-1", "--format=%B", rev]).ok()?;
                    parent = self
                        .run(&["commit-tree", &tree, "-p", &parent, "-m", msg.trim()])
                        .ok()?
                        .trim()
                        .to_string();
                }
                Some(parent)
            });
        if let Some(head) = rebuilt {
            let _ = self.run(&["update-ref", "HEAD", &head]);
            let _ = self.run(&["reflog", "expire", "--expire=now", "--all"]);
            let _ = self.run(&["gc", "--prune=now", "-q"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shadow_lives_in_the_data_folder_and_git_never_comes_from_the_folder() {
        let s = Shadow::new(Path::new("/data"), "lnk_x", Path::new("/work/mod"));
        assert_eq!(s.git_dir(), Path::new("/data/checkpoints/lnk_x.git"));
        if let Some(git) = &s.git {
            assert!(!git.starts_with("/work/mod"));
        }
    }
}
