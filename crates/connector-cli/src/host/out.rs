//! The environment's output folder (Cloud #2435, device #100, #102): what a command or a tool result produced beyond
//! what its caller takes inline stays **here, where it was produced**, and only a path to it travels (owner 2026-10-21
//! 「等会，怎么在往服务器上传文件？」, 「是的，返回结果留在空间/环境中。这里应该是环境吧。这样沙箱回收的时候，会顺手给清理掉了」).
//! The Agent opens the path with its own `read`, which this same host answers.
//!
//! One folder under the temp folder (`<temp>/agentrouter-out/`; in folder mode the temp folder is the link's scratch
//! folder), never inside the person's folders, one sub-folder per scope (the cloud session that uses this environment).
//! It does not depend on this process: a reconnect or a reboot keeps every path the Agent was given.
//!
//! **Nothing here is deleted automatically** — not when the link drops, not when the Agent moves to another environment,
//! not by age or size (owner 2026-10-21 「然后事件清理，这个应该是像CC一样，发现空间不够了，通知Agent可以清理了。我认为自动清理这个
//! 还是不要做，因为很可能误删」). hostd keeps the accounting (each scope's size, files and last use) and, when the folder
//! grows past `AGENTROUTER_OUT_WARN` (default 1 GiB) or the disk's free space falls under `AGENTROUTER_OUT_MIN_FREE`
//! (default 2 GiB), the next answer that adds output carries `storage_low` once (the runner turns it into the
//! environment's `environment.storage_low` event). The Agent decides what goes: `out_release` (a whole scope) or its
//! ordinary file tools. `out_status` answers the accounting at any time. (A cloud sandbox is discarded whole when it is
//! reclaimed: that is the environment ending, not a cleanup decision.)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use super::{Host, arg_str, shown, tool};

/// The folder's size past which the Agent is told (`AGENTROUTER_OUT_WARN`).
const WARN_DEFAULT: u64 = 1 << 30;
/// The disk's free space under which the Agent is told (`AGENTROUTER_OUT_MIN_FREE`).
const MIN_FREE_DEFAULT: u64 = 2 << 30;
/// One `spill_write` carries at most this much (longer text: several calls with `path`).
const WRITE_MAX: usize = 4 << 20;
/// A scope's last use (its mtime); not an output file.
const LAST: &str = ".last";
/// Without a new file, the accounting is looked at again at most this often.
const RECHECK: Duration = Duration::from_secs(60);
/// The notice names at most this many scopes (the largest).
const SCOPES_SHOWN: usize = 20;

/// A size: a number with `K`, `M`, `G` or `T` (powers of 1024; bare = bytes).
pub(super) fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (num, unit) = match raw.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&raw[..i], raw[i..].trim().trim_end_matches(['B', 'b', 'i'])),
        None => (raw, ""),
    };
    let n: u64 = num.parse().ok()?;
    let shift = match unit.to_ascii_uppercase().as_str() {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return None,
    };
    n.checked_mul(1u64 << shift)
}

fn size_env(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| parse_size(&v))
        .unwrap_or(default)
}

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "spill_write",
            "Save text in this environment's output folder (outside the person's folders; kept until the Agent removes it) and return its path. With path (a file this tool returned), the text is appended.",
            json!({"type": "object", "properties": {
                "scope": {"type": "string", "maxLength": 64},
                "name": {"type": "string", "maxLength": 80},
                "data": {"type": "string"},
                "path": {"type": "string"},
            }, "required": ["data"]}),
            false,
        ),
        tool(
            "out_status",
            "The output folder's accounting: where it is, its total size, the disk's free space, the thresholds, and each scope's size, files and last use (largest first).",
            json!({"type": "object", "properties": {}}),
            true,
        ),
        tool(
            "out_release",
            "Delete one scope's output files (the Agent decided they are no longer needed).",
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

/// Whether the Agent was told, and when the accounting was last looked at.
struct Watch {
    changed: bool,
    checked: Option<Instant>,
    told: bool,
}

pub(super) struct Out {
    root: PathBuf,
    warn: u64,
    min_free: u64,
    next: AtomicU64,
    watch: Mutex<Watch>,
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
        Out::at(
            temp.join("agentrouter-out"),
            size_env("AGENTROUTER_OUT_WARN", WARN_DEFAULT),
            size_env("AGENTROUTER_OUT_MIN_FREE", MIN_FREE_DEFAULT),
        )
    }
}

impl Out {
    pub(super) fn at(root: PathBuf, warn: u64, min_free: u64) -> Out {
        Out {
            root,
            warn,
            min_free,
            next: AtomicU64::new(0),
            watch: Mutex::new(Watch {
                changed: true,
                checked: None,
                told: false,
            }),
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

    /// A new file of `scope` named after `name`.
    pub(super) fn create(&self, scope: &str, name: &str) -> Result<(PathBuf, File), String> {
        let dir = self.scope_dir(scope)?;
        touch(&dir);
        self.watch.lock().unwrap().changed = true;
        let name = segment(name, "output.txt", 80);
        // The folder outlives this process: a number an earlier one used is skipped, never overwritten.
        loop {
            let n = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            let path = dir.join(format!("{n}-{name}"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("输出文件建不了：{e}")),
            }
        }
    }

    /// The scope folder `path` is in, if it is inside the output folder.
    fn scope_of(&self, path: &Path) -> Option<PathBuf> {
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return None;
        }
        let first = path.strip_prefix(&self.root).ok()?.components().next()?;
        Some(self.root.join(first.as_os_str()))
    }

    /// Whether `path` is one of the output files.
    fn owns(&self, path: &Path) -> bool {
        self.scope_of(path).is_some_and(|s| s != *path)
    }

    /// A call named `path`: if it is in a scope, that scope was used now (its last use in the accounting).
    pub(super) fn used(&self, path: &Path) {
        if let Some(scope) = self.scope_of(path).filter(|s| s.is_dir()) {
            touch(&scope);
        }
    }

    /// Only when the Agent asks (`out_release`).
    pub(super) fn release(&self, scope: &str) -> bool {
        let gone = std::fs::remove_dir_all(self.root.join(segment(scope, "shared", 64))).is_ok();
        self.watch.lock().unwrap().changed = true;
        gone
    }

    /// The accounting: total, free space, thresholds, each scope (largest first), and whether a threshold is crossed.
    pub(super) fn status(&self) -> Value {
        let mut scopes = Vec::new();
        if let Ok(rows) = std::fs::read_dir(&self.root) {
            for row in rows.flatten() {
                let dir = row.path();
                if !dir.is_dir() {
                    continue;
                }
                let (bytes, files) = tally(&dir);
                let last = std::fs::metadata(dir.join(LAST))
                    .or_else(|_| std::fs::metadata(&dir))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64);
                scopes.push((
                    bytes,
                    row.file_name().to_string_lossy().to_string(),
                    files,
                    last,
                ));
            }
        }
        scopes.sort_by_key(|a| std::cmp::Reverse(a.0));
        let total: u64 = scopes.iter().map(|s| s.0).sum();
        let free = free_space(if self.root.exists() {
            &self.root
        } else {
            self.root.parent().unwrap_or(&self.root)
        });
        let low = total >= self.warn || free.is_some_and(|f| f < self.min_free);
        json!({
            "folder": shown(&self.root),
            "total_bytes": total,
            "free_bytes": free,
            "warn_bytes": self.warn,
            "min_free_bytes": self.min_free,
            "low": low,
            "scopes": scopes.iter().take(SCOPES_SHOWN).map(|(bytes, scope, files, last)| json!({
                "scope": scope, "bytes": bytes, "files": files, "last_used_ms": last,
                "path": shown(&self.root.join(scope)),
            })).collect::<Vec<_>>(),
            "scope_count": scopes.len(),
        })
    }

    /// The accounting once a threshold is crossed, the first time only (again after it was below); looked at after a new
    /// file or a release, else at most every [`RECHECK`].
    pub(super) fn notice(&self) -> Option<Value> {
        let mut w = self.watch.lock().unwrap();
        if !w.changed && w.checked.is_some_and(|t| t.elapsed() < RECHECK) {
            return None;
        }
        w.changed = false;
        w.checked = Some(Instant::now());
        let status = self.status();
        let low = status["low"] == true;
        let tell = low && !w.told;
        w.told = low;
        tell.then_some(status)
    }
}

/// The size and file count of one scope (its marker left out).
fn tally(dir: &Path) -> (u64, u64) {
    let (mut bytes, mut files) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rows) = std::fs::read_dir(&d) else {
            continue;
        };
        for row in rows.flatten() {
            let Ok(meta) = row.metadata() else { continue };
            if meta.is_dir() {
                stack.push(row.path());
            } else if meta.is_file() && !(d == dir && row.file_name() == LAST) {
                bytes += meta.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// Marks a scope used now.
fn touch(dir: &Path) {
    let _ = File::create(dir.join(LAST));
}

/// The free space (for this user) of the disk `path` is on.
#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a properly sized out-parameter.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

#[cfg(windows)]
fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    // SAFETY: `wide` is NUL-terminated; the out-parameters may be null except the first.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

#[cfg(not(any(unix, windows)))]
fn free_space(_path: &Path) -> Option<u64> {
    None
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

    pub(super) fn out_status(&self) -> Result<Value, String> {
        Ok(self.out.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_deleted_and_the_crossing_is_told_once() {
        let base = super::super::tests::temp("out");
        let out = Out::at(base.clone(), 150, 0);
        let (a, mut fa) = out.create("ses/1", "../x.txt").unwrap();
        assert!(
            a.starts_with(out.root())
                && a.file_name().unwrap().to_string_lossy().ends_with("x.txt")
        );
        fa.write_all(&[b'a'; 100]).unwrap();
        drop(fa);
        assert!(out.notice().is_none(), "under the threshold");
        let (b, mut fb) = out.create("ses-2", "y.txt").unwrap();
        fb.write_all(&[b'b'; 100]).unwrap();
        drop(fb);
        let told = out.notice().expect("past the threshold");
        assert_eq!(told["total_bytes"], 200);
        assert_eq!(told["scopes"].as_array().unwrap().len(), 2);
        assert!(a.exists() && b.exists(), "nothing deleted by itself");
        let (_, fc) = out.create("ses-2", "z.txt").unwrap();
        drop(fc);
        assert!(out.notice().is_none(), "told once");
        // The Agent releases a scope: below again, and the next crossing is told again.
        assert!(out.release("ses-1") && !a.exists() && b.exists());
        assert!(out.notice().is_none());
        let (_, mut fd) = out.create("ses-3", "w.txt").unwrap();
        fd.write_all(&[b'c'; 100]).unwrap();
        drop(fd);
        assert!(out.notice().is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_new_process_keeps_the_folder_and_never_overwrites_a_file() {
        let base = super::super::tests::temp("out-restart");
        let (a, mut fa) = Out::at(base.clone(), WARN_DEFAULT, 0)
            .create("ses", "x.txt")
            .unwrap();
        fa.write_all(b"first").unwrap();
        drop(fa);
        // A reconnect or reboot: a new hostd, the same folder.
        let (b, _) = Out::at(base.clone(), WARN_DEFAULT, 0)
            .create("ses", "x.txt")
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"first");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn sizes_read_units() {
        assert_eq!(parse_size("500M"), Some(500 << 20));
        assert_eq!(parse_size("1G"), Some(1 << 30));
        assert_eq!(parse_size("2GiB"), Some(2 << 30));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("lots"), None);
    }
}
