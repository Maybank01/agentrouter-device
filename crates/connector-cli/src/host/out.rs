//! The environment's output folder (Cloud #2435, device #100): what a command or a tool result produced beyond what
//! its caller takes inline stays **here, where it was produced**, and only a path to it travels (owner 2026-10-21
//! 「等会，怎么在往服务器上传文件？」, 「是的，返回结果留在空间/环境中。这里应该是环境吧。这样沙箱回收的时候，会顺手给清理掉了」).
//! The Agent opens the path with its own `read`, which this same host answers.
//!
//! One folder per link under the temp folder (`<temp>/agentrouter-out/<pid>/`; in folder mode the temp folder is the
//! link's scratch folder), never inside the person's folders, one sub-folder per scope (the cloud session that uses
//! this environment). It goes away when the scope is released (`out_release`: the environment ended or the Agent moved
//! away), when the link ends, and at the latest through the caps: files older than [`AGE`] and the oldest files past
//! [`TOTAL_MAX`] are deleted whenever a new file starts, and folders of links that are gone are swept at start.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use super::{Host, arg_str, shown, tool};

/// Files older than this are deleted (a safety net; release and the link's end delete them first).
const AGE: Duration = Duration::from_secs(24 * 3600);
/// All output files of this computer together stay under this size; the oldest go first.
const TOTAL_MAX: u64 = 1 << 30;
/// One `spill_write` carries at most this much (longer text: several calls with `path`).
const WRITE_MAX: usize = 4 << 20;

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "spill_write",
            "Save text in this environment's output folder (outside the person's folders; removed when the scope is released or the link ends) and return its path. With path (a file this tool returned), the text is appended.",
            json!({"type": "object", "properties": {
                "scope": {"type": "string", "maxLength": 64},
                "name": {"type": "string", "maxLength": 80},
                "data": {"type": "string"},
                "path": {"type": "string"},
            }, "required": ["data"]}),
            false,
        ),
        tool(
            "out_release",
            "Delete a scope's output files (the environment of that session ended).",
            json!({"type": "object", "properties": {"scope": {"type": "string", "maxLength": 64}}, "required": ["scope"]}),
            false,
        ),
    ]
}

/// A scope or file name as one safe path segment.
fn segment(raw: &str, fallback: &str, max: usize) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(max)
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

pub(super) struct Out {
    root: PathBuf,
    next: AtomicU64,
    /// Serialises pruning.
    prune: Mutex<()>,
}

impl Default for Out {
    fn default() -> Out {
        // The temp folder as the link's commands see it (folder mode points it at the link's scratch folder). Only data
        // is written there; nothing runs from it (docs/AV-HYGIENE.md).
        let temp = ["TMPDIR", "TEMP", "TMP"]
            .iter()
            .find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    std::env::current_dir().unwrap_or_default()
                } else {
                    PathBuf::from("/tmp")
                }
            });
        Out::at(temp.join("agentrouter-out"))
    }
}

impl Out {
    /// The folder of this link under `base`; folders of other links untouched for [`AGE`] are swept.
    pub(super) fn at(base: PathBuf) -> Out {
        if let Ok(rows) = std::fs::read_dir(&base) {
            for row in rows.flatten() {
                if untouched(&row.path(), AGE) {
                    let _ = std::fs::remove_dir_all(row.path());
                }
            }
        }
        Out {
            root: base.join(std::process::id().to_string()),
            next: AtomicU64::new(0),
            prune: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    fn scope_dir(&self, scope: &str) -> Result<PathBuf, String> {
        let dir = self.root.join(segment(scope, "shared", 64));
        std::fs::create_dir_all(&dir).map_err(|e| format!("输出文件夹建不了：{e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [&self.root, &dir] {
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            }
        }
        Ok(dir)
    }

    /// A new file of `scope` named after `name` (the caps are applied first).
    pub(super) fn create(&self, scope: &str, name: &str) -> Result<(PathBuf, File), String> {
        self.apply_caps();
        let dir = self.scope_dir(scope)?;
        let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let path = dir.join(format!("{n}-{}", segment(name, "output.txt", 80)));
        let file = File::create(&path).map_err(|e| format!("输出文件建不了：{e}"))?;
        Ok((path, file))
    }

    /// Whether `path` is one of this link's output files.
    fn owns(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
            && !path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
    }

    pub(super) fn release(&self, scope: &str) -> bool {
        std::fs::remove_dir_all(self.root.join(segment(scope, "shared", 64))).is_ok()
    }

    /// The link ended: all of it goes.
    pub(super) fn remove_all(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }

    /// Delete files older than [`AGE`], then the oldest until all are under [`TOTAL_MAX`].
    pub(super) fn apply_caps(&self) {
        self.caps(AGE, TOTAL_MAX);
    }

    fn caps(&self, age: Duration, total_max: u64) {
        let _held = self.prune.lock().unwrap();
        let mut files = Vec::new();
        let Ok(scopes) = std::fs::read_dir(&self.root) else {
            return;
        };
        for scope in scopes.flatten() {
            let Ok(rows) = std::fs::read_dir(scope.path()) else {
                continue;
            };
            for row in rows.flatten() {
                let Ok(meta) = row.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let at = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((at, meta.len(), row.path()));
            }
        }
        files.sort();
        let now = SystemTime::now();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        for (at, len, path) in files {
            let old = now.duration_since(at).unwrap_or_default() > age;
            if (old || total > total_max) && std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

fn untouched(path: &Path, age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d > age)
}

/// A stream's complete copy in the output folder: every byte from offset 0 up to `max`.
pub(super) struct Spill {
    pub(super) path: PathBuf,
    file: File,
    pub(super) written: u64,
    max: u64,
    pub(super) full: bool,
}

impl Spill {
    pub(super) fn new(path: PathBuf, file: File, max: u64) -> Spill {
        Spill {
            path,
            file,
            written: 0,
            max,
            full: false,
        }
    }

    pub(super) fn write(&mut self, bytes: &[u8]) {
        if self.full {
            return;
        }
        let room = self.max.saturating_sub(self.written) as usize;
        let take = bytes.len().min(room);
        if self.file.write_all(&bytes[..take]).is_err() {
            self.full = true;
            return;
        }
        self.written += take as u64;
        if take < bytes.len() {
            self.full = true;
        }
    }

    /// Bytes `[offset, offset + max)` of the copy (fewer at its end).
    pub(super) fn read_at(&self, offset: u64, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let Ok(mut f) = File::open(&self.path) else {
            return out;
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return out;
        }
        let want = (self.written.saturating_sub(offset) as usize).min(max);
        let _ = f.take(want as u64).read_to_end(&mut out);
        out
    }

    pub(super) fn shown(&self) -> String {
        shown(&self.path)
    }
}

impl Host {
    pub(super) fn spill_write(&self, args: &Value) -> Result<Value, String> {
        let data = arg_str(args, "data").unwrap_or("");
        if data.len() > WRITE_MAX {
            return Err(format!(
                "一次最多写 {WRITE_MAX} 字节；更长的分几次，带上 path 追加"
            ));
        }
        let (path, mut file) = match arg_str(args, "path").filter(|p| !p.is_empty()) {
            Some(p) => {
                let path = PathBuf::from(p);
                if !self.out.owns(&path) {
                    return Err("path 只能是 spill_write 返回的文件".into());
                }
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .map_err(|e| format!("输出文件打不开：{e}"))?;
                (path, file)
            }
            None => self.out.create(
                arg_str(args, "scope").unwrap_or(""),
                arg_str(args, "name").unwrap_or("output.txt"),
            )?,
        };
        file.write_all(data.as_bytes())
            .map_err(|e| format!("输出文件写不进去：{e}"))?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(json!({"path": shown(&path), "bytes": bytes}))
    }

    pub(super) fn out_release(&self, args: &Value) -> Result<Value, String> {
        let scope = arg_str(args, "scope").unwrap_or("");
        if scope.is_empty() {
            return Err("缺少 scope".into());
        }
        Ok(json!({"released": self.out.release(scope)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_with_their_scope_and_the_caps_delete_the_oldest() {
        let base = super::super::tests::temp("out");
        let out = Out::at(base.clone());
        let (a, mut fa) = out.create("ses/1", "../x.txt").unwrap();
        assert!(
            a.starts_with(out.root())
                && a.file_name().unwrap().to_string_lossy().ends_with("x.txt")
        );
        fa.write_all(&[b'a'; 100]).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let (b, mut fb) = out.create("ses-2", "y.txt").unwrap();
        fb.write_all(&[b'b'; 100]).unwrap();
        // Windows deletes no open file (a running job's copy stays until it ends).
        drop((fa, fb));
        out.caps(AGE, 150);
        assert!(!a.exists() && b.exists(), "the oldest goes first");
        assert!(out.release("ses-2") && !b.exists());
        out.remove_all();
        assert!(!out.root().exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
