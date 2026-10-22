//! The two layers of a checkout as one folder (Cloud PROJECT-SERVICE.md §5.5.2, §5.5.4 "Linux"): a lazy read-only
//! lower layer (one space version, [`Lower`]) and a writable upper layer (a real directory on this host) in **one**
//! FUSE process on `fuser` (MIT) — kernel overlayfs over a FUSE lower is refused in Sysbox's user namespace, and two
//! stacked FUSE hops would double every read's cost.
//!
//! - A file is copied up on its first write (open for writing, truncate, chmod, touch); deletions are whiteouts. The
//!   upper layer *is* the environment's change set: [`SpaceFs::changes`] lists it for the one sync engine, which writes
//!   it into the session's draft, and [`SpaceFs::settle`] drops what the new lower now holds the same.
//! - A whiteout on a directory that exists again in the upper hides the lower's children (opaque).
//! - Renaming a directory that has lower content answers `EXDEV` (as overlayfs without `redirect_dir`); `mv` then
//!   copies, so nothing is moved behind the sync's back.
//! - Every file is shown as the configured owner's (commands run as another user than `hostd`; the sandbox is the
//!   boundary, so no permission check is done here).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::lower::{Entry, Kind, Lower, LowerError, digest_of};

/// How long the kernel may keep a name or attributes before asking again (a version switch shows within this).
const TTL: Duration = Duration::from_secs(1);

/// What has diverged from the lower layer, kept on disk beside the upper directory (survives a `hostd` restart).
#[derive(Default, Serialize, Deserialize)]
pub struct Meta {
    /// Paths deleted from the lower (a directory here that exists in the upper again is opaque).
    pub whiteouts: BTreeSet<String>,
    /// The lower digest each diverged path had when it first diverged (`None`: it did not exist) — the `expected` of
    /// its draft write, so a change the session made meanwhile is a conflict, never overwritten.
    pub base: BTreeMap<String, Option<String>>,
}

/// One change of the upper layer, for the sync engine.
#[derive(Debug, PartialEq, Serialize)]
pub struct Change {
    pub path: String,
    /// `put` (the merged file at `path` is the new content) or `delete`.
    pub op: &'static str,
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

enum Node {
    Upper(std::fs::Metadata),
    Lower(Entry),
    Top,
}

struct Open {
    file: Arc<File>,
    path: String,
    write: bool,
}

#[derive(Default)]
struct Inner {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next_ino: u64,
    meta: Meta,
    handles: HashMap<u64, Open>,
    next_fh: u64,
}

pub struct SpaceFs {
    pub lower: Arc<Lower>,
    upper: PathBuf,
    meta_file: PathBuf,
    owner: (u32, u32),
    mounted_at: SystemTime,
    inner: Mutex<Inner>,
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

fn name_ok(name: &OsStr) -> Option<&str> {
    name.to_str()
        .filter(|n| !n.is_empty() && *n != "." && *n != ".." && !n.contains('/'))
}

fn lower_errno(error: LowerError) -> Errno {
    match error {
        LowerError::NotFound => Errno::ENOENT,
        LowerError::Failed { .. } => Errno::EIO,
    }
}

fn is_under(path: &str, top: &str) -> bool {
    path == top
        || (path.len() > top.len() && path.starts_with(top) && path.as_bytes()[top.len()] == b'/')
}

impl SpaceFs {
    /// `state`: this mount's own directory (`upper/` and `meta.json` in it).
    pub fn new(lower: Arc<Lower>, state: &Path, owner: (u32, u32)) -> std::io::Result<SpaceFs> {
        let upper = state.join("upper");
        std::fs::create_dir_all(&upper)?;
        let meta_file = state.join("meta.json");
        let meta = std::fs::read(&meta_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
            .unwrap_or_default();
        let mut inner = Inner {
            next_ino: 2,
            next_fh: 1,
            meta,
            ..Inner::default()
        };
        inner.by_ino.insert(1, String::new());
        inner.by_path.insert(String::new(), 1);
        Ok(SpaceFs {
            lower,
            upper,
            meta_file,
            owner,
            mounted_at: SystemTime::now(),
            inner: Mutex::new(inner),
        })
    }

    fn up(&self, path: &str) -> PathBuf {
        if path.is_empty() {
            self.upper.clone()
        } else {
            self.upper.join(path)
        }
    }

    fn save(&self, inner: &Inner) {
        let part = self.meta_file.with_extension("json.part");
        if let Ok(bytes) = serde_json::to_vec(&inner.meta)
            && std::fs::write(&part, bytes).is_ok()
        {
            let _ = std::fs::rename(&part, &self.meta_file);
        }
    }

    fn path_of(&self, ino: INodeNo) -> Result<String, Errno> {
        self.inner
            .lock()
            .unwrap()
            .by_ino
            .get(&ino.0)
            .cloned()
            .ok_or(Errno::ENOENT)
    }

    fn ino_of(&self, path: &str) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(ino) = inner.by_path.get(path) {
            return *ino;
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.by_ino.insert(ino, path.to_string());
        inner.by_path.insert(path.to_string(), ino);
        ino
    }

    /// Whether the lower's `path` is hidden by a whiteout on it or on a folder above it.
    fn hidden(&self, path: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        let mut here = path;
        loop {
            if inner.meta.whiteouts.contains(here) {
                return true;
            }
            if here.is_empty() {
                return false;
            }
            here = parent_of(here);
        }
    }

    /// The lower's entry at `path`, whatever the whiteouts say.
    fn lower_entry(&self, path: &str) -> Result<Option<Entry>, Errno> {
        match self.lower.entry(path) {
            Ok(e) => Ok(Some(e)),
            Err(LowerError::NotFound) => Ok(None),
            Err(e) => Err(lower_errno(e)),
        }
    }

    fn lower_digest(&self, path: &str) -> Option<String> {
        self.lower_entry(path)
            .ok()
            .flatten()
            .and_then(|e| match e.kind {
                Kind::Link(target) => Some(format!("link:{target}")),
                Kind::File { .. } => e.digest,
                Kind::Dir => None,
            })
    }

    fn resolve(&self, path: &str) -> Result<Option<Node>, Errno> {
        if let Ok(meta) = std::fs::symlink_metadata(self.up(path)) {
            return Ok(Some(Node::Upper(meta)));
        }
        if path.is_empty() {
            return Ok(Some(Node::Top));
        }
        if self.hidden(path) {
            return Ok(None);
        }
        Ok(self.lower_entry(path)?.map(Node::Lower))
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        let (kind, size, perm, mtime, nlink) = match node {
            Node::Upper(m) => {
                let kind = if m.is_dir() {
                    FileType::Directory
                } else if m.file_type().is_symlink() {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                };
                (
                    kind,
                    m.len(),
                    (m.mode() & 0o7777) as u16,
                    m.modified().unwrap_or(self.mounted_at),
                    if m.is_dir() { 2 } else { 1 },
                )
            }
            Node::Top => (FileType::Directory, 0, 0o755, self.mounted_at, 2),
            Node::Lower(e) => match &e.kind {
                Kind::Dir => (FileType::Directory, 0, 0o755, self.mounted_at, 2),
                Kind::File { executable } => (
                    FileType::RegularFile,
                    e.size,
                    if *executable { 0o755 } else { 0o644 },
                    self.mounted_at,
                    1,
                ),
                Kind::Link(t) => (FileType::Symlink, t.len() as u64, 0o777, self.mounted_at, 1),
            },
        };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink,
            uid: self.owner.0,
            gid: self.owner.1,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The upper folder for `path` and every folder above it (copied up as empty folders: their lower children stay
    /// visible through the merge).
    fn upper_dirs(&self, path: &str) -> Result<(), Errno> {
        let mut here = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            here = join(&here, part);
            let target = self.up(&here);
            match std::fs::symlink_metadata(&target) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(Errno::ENOTDIR),
                Err(_) => {
                    std::fs::create_dir(&target).or_else(|e| {
                        if target.is_dir() {
                            Ok(())
                        } else {
                            Err(Errno::from(e))
                        }
                    })?;
                    let _ =
                        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
                }
            }
        }
        Ok(())
    }

    /// Records `path`'s first divergence from the lower (its digest then), once.
    fn diverge(&self, path: &str) {
        let digest = self.lower_digest(path);
        let mut inner = self.inner.lock().unwrap();
        if !inner.meta.base.contains_key(path) {
            inner.meta.base.insert(path.to_string(), digest);
            self.save(&inner);
        }
    }

    /// The file or link at `path` in the upper layer, copied from the lower on its first write.
    fn copy_up(&self, path: &str) -> Result<(), Errno> {
        if std::fs::symlink_metadata(self.up(path)).is_ok() {
            return Ok(());
        }
        let Some(node) = self.resolve(path)? else {
            return Err(Errno::ENOENT);
        };
        self.upper_dirs(parent_of(path))?;
        match node {
            Node::Upper(_) => {}
            Node::Top => {}
            Node::Lower(e) => match &e.kind {
                Kind::Dir => self.upper_dirs(path)?,
                Kind::Link(target) => {
                    self.diverge(path);
                    std::os::unix::fs::symlink(target, self.up(path)).map_err(Errno::from)?;
                }
                Kind::File { executable } => {
                    let digest = e.digest.clone().ok_or(Errno::EIO)?;
                    let object = self.lower.object(path, &digest).map_err(lower_errno)?;
                    self.diverge(path);
                    let target = self.up(path);
                    let part = target.with_file_name(format!(".{}.copyup", e.name));
                    std::fs::copy(&object, &part).map_err(Errno::from)?;
                    std::fs::set_permissions(
                        &part,
                        std::fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
                    )
                    .map_err(Errno::from)?;
                    std::fs::rename(&part, &target).map_err(Errno::from)?;
                }
            },
        }
        Ok(())
    }

    /// The merged listing of the folder at `path`: upper names first, then the lower's not hidden or shadowed.
    fn merged(&self, path: &str) -> Result<Vec<(String, FileType)>, Errno> {
        let mut out: BTreeMap<String, FileType> = BTreeMap::new();
        if let Ok(read) = std::fs::read_dir(self.up(path)) {
            for e in read.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.ends_with(".copyup") && name.starts_with('.') {
                    continue;
                }
                let kind = match e.file_type() {
                    Ok(t) if t.is_dir() => FileType::Directory,
                    Ok(t) if t.is_symlink() => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                out.insert(name, kind);
            }
        }
        if !self.hidden(path) {
            match self.lower.dir(path) {
                Ok(listing) => {
                    for e in listing.iter() {
                        if out.contains_key(&e.name) || self.hidden(&join(path, &e.name)) {
                            continue;
                        }
                        let kind = match e.kind {
                            Kind::Dir => FileType::Directory,
                            Kind::Link(_) => FileType::Symlink,
                            Kind::File { .. } => FileType::RegularFile,
                        };
                        out.insert(e.name.clone(), kind);
                    }
                }
                Err(LowerError::NotFound) => {}
                Err(e) => return Err(lower_errno(e)),
            }
        }
        Ok(out.into_iter().collect())
    }

    fn handle(&self, file: File, path: &str, write: bool) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let fh = inner.next_fh;
        inner.next_fh += 1;
        inner.handles.insert(
            fh,
            Open {
                file: Arc::new(file),
                path: path.to_string(),
                write,
            },
        );
        fh
    }

    fn file_of(&self, fh: FileHandle) -> Result<Arc<File>, Errno> {
        self.inner
            .lock()
            .unwrap()
            .handles
            .get(&fh.0)
            .map(|h| h.file.clone())
            .ok_or(Errno::EBADF)
    }

    /// Moves the inode names under `from` to `to` (a rename).
    fn rename_inodes(&self, from: &str, to: &str) {
        let mut inner = self.inner.lock().unwrap();
        let moved: Vec<(String, u64)> = inner
            .by_path
            .iter()
            .filter(|(p, _)| is_under(p, from))
            .map(|(p, i)| (p.clone(), *i))
            .collect();
        if let Some(old) = inner.by_path.remove(to) {
            inner.by_ino.remove(&old);
        }
        for (path, ino) in moved {
            let new = format!("{to}{}", &path[from.len()..]);
            inner.by_path.remove(&path);
            inner.by_path.insert(new.clone(), ino);
            inner.by_ino.insert(ino, new);
        }
    }

    fn whiteout(&self, path: &str, on: bool) {
        let mut inner = self.inner.lock().unwrap();
        let changed = if on {
            inner.meta.whiteouts.insert(path.to_string())
        } else {
            inner.meta.whiteouts.remove(path)
        };
        if changed {
            self.save(&inner);
        }
    }

    /// A path stops diverging (created and deleted again): its base goes.
    fn forget_base_if_new(&self, path: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.meta.base.get(path) == Some(&None) {
            inner.meta.base.remove(path);
            self.save(&inner);
        }
    }

    /// What the upper layer holds that the lower does not: puts (upper files and links whose content differs from
    /// their base) and deletes (whiteouts of lower paths, a folder's lower files one by one).
    pub fn changes(&self) -> Result<Vec<Change>, Value> {
        let mut out = Vec::new();
        let (base, whiteouts) = {
            let inner = self.inner.lock().unwrap();
            (inner.meta.base.clone(), inner.meta.whiteouts.clone())
        };
        let mut stack = vec![String::new()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(self.up(&dir)) else {
                continue;
            };
            for e in read.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.starts_with('.') && name.ends_with(".copyup") {
                    continue;
                }
                let path = join(&dir, &name);
                let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path);
                    continue;
                }
                let was = base
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| self.lower_digest(&path));
                if meta.file_type().is_symlink() {
                    let target = std::fs::read_link(e.path())
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if was.as_deref() != Some(&format!("link:{target}")) {
                        out.push(Change {
                            path,
                            op: "put",
                            base: was,
                            digest: None,
                            executable: None,
                            link: Some(target),
                        });
                    }
                    continue;
                }
                let digest = digest_of(&e.path()).map_err(|err| json!({"code": "CHECKOUT_UPPER_UNREADABLE", "message": format!("{path}: {err}")}))?;
                let executable = meta.mode() & 0o111 != 0;
                let lower_exec = matches!(
                    self.lower_entry(&path),
                    Ok(Some(Entry {
                        kind: Kind::File { executable: true },
                        ..
                    }))
                );
                if was.as_deref() != Some(digest.as_str()) || executable != lower_exec {
                    out.push(Change {
                        path,
                        op: "put",
                        base: was,
                        digest: Some(digest),
                        executable: Some(executable),
                        link: None,
                    });
                }
            }
        }
        for gone in &whiteouts {
            let upper = std::fs::symlink_metadata(self.up(gone)).ok();
            match self.lower_entry(gone) {
                Ok(Some(Entry {
                    kind: Kind::Dir, ..
                })) => {
                    // Every lower file under the folder, unless the upper put one back at the same path.
                    let mut todo = vec![gone.clone()];
                    while let Some(dir) = todo.pop() {
                        let listing = self.lower.dir(&dir).map_err(|e| json!({"code": "CHECKOUT_LOWER_UNREADABLE", "message": format!("{dir}: {e:?}")}))?;
                        for e in listing.iter() {
                            let path = join(&dir, &e.name);
                            match e.kind {
                                Kind::Dir => todo.push(path),
                                _ if std::fs::symlink_metadata(self.up(&path)).is_ok() => {}
                                Kind::File { .. } => out.push(Change {
                                    path,
                                    op: "delete",
                                    base: e.digest.clone(),
                                    digest: None,
                                    executable: None,
                                    link: None,
                                }),
                                Kind::Link(ref t) => out.push(Change {
                                    path,
                                    op: "delete",
                                    base: Some(format!("link:{t}")),
                                    digest: None,
                                    executable: None,
                                    link: None,
                                }),
                            }
                        }
                    }
                }
                Ok(Some(e)) if upper.is_none() => {
                    let was = base.get(gone).cloned().unwrap_or(match &e.kind {
                        Kind::Link(t) => Some(format!("link:{t}")),
                        _ => e.digest.clone(),
                    });
                    out.push(Change {
                        path: gone.clone(),
                        op: "delete",
                        base: was,
                        digest: None,
                        executable: None,
                        link: None,
                    });
                }
                _ => {}
            }
        }
        // A file under a deleted folder may also carry its own whiteout (deleted first, then the folder): once.
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        Ok(out)
    }

    /// After a move to a new lower version (the sync wrote the changes into the draft, or main moved): every upper
    /// file the new lower now holds the same is dropped (unless a program has it open for writing), and whiteouts
    /// of paths the new lower no longer has are dropped. Returns how many paths settled.
    pub fn settle(&self) -> usize {
        let mut settled = 0;
        let writing: BTreeSet<String> = self
            .inner
            .lock()
            .unwrap()
            .handles
            .values()
            .filter(|h| h.write)
            .map(|h| h.path.clone())
            .collect();
        let mut dirs = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(self.up(&dir)) else {
                continue;
            };
            for e in read.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let path = join(&dir, &name);
                let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                    continue;
                };
                if meta.is_dir() {
                    stack.push(path.clone());
                    dirs.push(path);
                    continue;
                }
                if writing.contains(&path) || self.hidden(&path) {
                    continue;
                }
                let same = match self.lower_entry(&path) {
                    Ok(Some(Entry {
                        kind: Kind::Link(t),
                        ..
                    })) => {
                        meta.file_type().is_symlink()
                            && std::fs::read_link(e.path())
                                .map(|x| x.to_string_lossy() == t.as_str())
                                .unwrap_or(false)
                    }
                    Ok(Some(Entry {
                        kind: Kind::File { executable },
                        digest: Some(d),
                        ..
                    })) => {
                        !meta.file_type().is_symlink()
                            && (meta.mode() & 0o111 != 0) == executable
                            && digest_of(&e.path()).map(|x| x == d).unwrap_or(false)
                    }
                    _ => false,
                };
                if same && std::fs::remove_file(e.path()).is_ok() {
                    let mut inner = self.inner.lock().unwrap();
                    inner.meta.base.remove(&path);
                    settled += 1;
                }
            }
        }
        // Folders left empty that the lower has (and does not hide) go: the lower shows them.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));
        for dir in dirs {
            if !self.hidden(&dir)
                && matches!(
                    self.lower_entry(&dir),
                    Ok(Some(Entry {
                        kind: Kind::Dir,
                        ..
                    }))
                )
            {
                let _ = std::fs::remove_dir(self.up(&dir));
            }
        }
        let whiteouts = self.inner.lock().unwrap().meta.whiteouts.clone();
        for gone in whiteouts {
            if matches!(self.lower_entry(&gone), Ok(None))
                && std::fs::symlink_metadata(self.up(&gone)).is_err()
            {
                let mut inner = self.inner.lock().unwrap();
                inner.meta.whiteouts.remove(&gone);
                inner.meta.base.remove(&gone);
                settled += 1;
            }
        }
        let inner = self.inner.lock().unwrap();
        self.save(&inner);
        settled
    }

    /// Fetches these files of the lower into this host's cache ahead of their first read (a task's declared inputs, a
    /// version's hot set). Returns how many were fetched now.
    pub fn prefetch(&self, paths: &[String]) -> usize {
        let mut fetched = 0;
        for path in paths {
            if let Ok(Some(Entry {
                kind: Kind::File { .. },
                digest: Some(d),
                ..
            })) = self.lower_entry(path)
                && !self.lower.object_path(&d).is_file()
                && self.lower.object(path, &d).is_ok()
            {
                fetched += 1;
            }
        }
        fetched
    }
}

/// The FUSE side: `fuser` takes the file system by value; the mount registry keeps the same [`SpaceFs`].
pub struct Shared(pub Arc<SpaceFs>);

impl Filesystem for Shared {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(name).ok_or(Errno::ENOENT)?;
            let path = join(&fs.path_of(parent)?, name);
            let node = fs.resolve(&path)?.ok_or(Errno::ENOENT)?;
            Ok::<_, Errno>(fs.attr(fs.ino_of(&path), &node))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let fs = &self.0;
        let result = fs
            .path_of(ino)
            .and_then(|p| fs.resolve(&p)?.ok_or(Errno::ENOENT));
        match result {
            Ok(node) => reply.attr(&TTL, &fs.attr(ino.0, &node)),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let fs = &self.0;
        let result = (|| {
            let path = fs.path_of(ino)?;
            if mode.is_some() || size.is_some() || atime.is_some() || mtime.is_some() {
                fs.copy_up(&path)?;
            }
            let target = fs.up(&path);
            if let Some(size) = size {
                match fh.map(|h| fs.file_of(h)) {
                    Some(Ok(file)) => file.set_len(size).map_err(Errno::from)?,
                    _ => OpenOptions::new()
                        .write(true)
                        .open(&target)
                        .and_then(|f| f.set_len(size))
                        .map_err(Errno::from)?,
                }
            }
            if let Some(mode) = mode {
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(Errno::from)?;
            }
            if atime.is_some() || mtime.is_some() {
                let at = |t: Option<TimeOrNow>| match t {
                    Some(TimeOrNow::SpecificTime(t)) => Some(t),
                    Some(TimeOrNow::Now) => Some(SystemTime::now()),
                    None => None,
                };
                let mut times = std::fs::FileTimes::new();
                if let Some(t) = at(atime) {
                    times = times.set_accessed(t);
                }
                if let Some(t) = at(mtime) {
                    times = times.set_modified(t);
                }
                if !std::fs::symlink_metadata(&target)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(true)
                {
                    File::options()
                        .write(true)
                        .open(&target)
                        .and_then(|f| f.set_times(times))
                        .or_else(|_| File::open(&target).and_then(|f| f.set_times(times)))
                        .map_err(Errno::from)?;
                }
            }
            let node = fs.resolve(&path)?.ok_or(Errno::ENOENT)?;
            Ok::<_, Errno>(fs.attr(ino.0, &node))
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let fs = &self.0;
        let result = fs.path_of(ino).and_then(|p| match fs.resolve(&p)? {
            Some(Node::Upper(_)) => std::fs::read_link(fs.up(&p))
                .map(|t| t.as_os_str().as_encoded_bytes().to_vec())
                .map_err(Errno::from),
            Some(Node::Lower(Entry {
                kind: Kind::Link(t),
                ..
            })) => Ok(t.into_bytes()),
            Some(_) => Err(Errno::EINVAL),
            None => Err(Errno::ENOENT),
        });
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(name).ok_or(Errno::EINVAL)?;
            let parent = fs.path_of(parent)?;
            let path = join(&parent, name);
            if fs.resolve(&path)?.is_some() {
                return Err(Errno::EEXIST);
            }
            fs.upper_dirs(&parent)?;
            std::fs::create_dir(fs.up(&path)).map_err(Errno::from)?;
            let _ = std::fs::set_permissions(
                fs.up(&path),
                std::fs::Permissions::from_mode(mode & !umask & 0o7777),
            );
            let node = fs.resolve(&path)?.ok_or(Errno::EIO)?;
            Ok(fs.attr(fs.ino_of(&path), &node))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(name).ok_or(Errno::ENOENT)?;
            let path = join(&fs.path_of(parent)?, name);
            match fs.resolve(&path)? {
                None => return Err(Errno::ENOENT),
                Some(Node::Upper(m)) if m.is_dir() => return Err(Errno::EISDIR),
                Some(Node::Lower(Entry {
                    kind: Kind::Dir, ..
                }))
                | Some(Node::Top) => return Err(Errno::EISDIR),
                _ => {}
            }
            let lower_has = fs.lower_entry(&path)?.is_some();
            if lower_has {
                fs.diverge(&path);
            }
            match std::fs::remove_file(fs.up(&path)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Errno::from(e)),
            }
            if lower_has {
                fs.whiteout(&path, true);
            } else {
                fs.forget_base_if_new(&path);
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(name).ok_or(Errno::ENOENT)?;
            let path = join(&fs.path_of(parent)?, name);
            match fs.resolve(&path)? {
                None => return Err(Errno::ENOENT),
                Some(Node::Upper(m)) if !m.is_dir() => return Err(Errno::ENOTDIR),
                Some(Node::Lower(Entry {
                    kind: Kind::File { .. } | Kind::Link(_),
                    ..
                })) => return Err(Errno::ENOTDIR),
                _ => {}
            }
            if !fs.merged(&path)?.is_empty() {
                return Err(Errno::ENOTEMPTY);
            }
            match std::fs::remove_dir_all(fs.up(&path)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Errno::from(e)),
            }
            if matches!(
                fs.lower_entry(&path)?,
                Some(Entry {
                    kind: Kind::Dir,
                    ..
                })
            ) {
                fs.whiteout(&path, true);
            }
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(link_name).ok_or(Errno::EINVAL)?;
            let parent = fs.path_of(parent)?;
            let path = join(&parent, name);
            if fs.resolve(&path)?.is_some() {
                return Err(Errno::EEXIST);
            }
            fs.upper_dirs(&parent)?;
            fs.diverge(&path);
            std::os::unix::fs::symlink(target, fs.up(&path)).map_err(Errno::from)?;
            fs.whiteout(&path, false);
            let node = fs.resolve(&path)?.ok_or(Errno::EIO)?;
            Ok(fs.attr(fs.ino_of(&path), &node))
        })();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let fs = &self.0;
        let result = (|| {
            if flags.contains(RenameFlags::RENAME_EXCHANGE)
                || flags.contains(RenameFlags::RENAME_WHITEOUT)
            {
                return Err(Errno::EINVAL);
            }
            let from = join(&fs.path_of(parent)?, name_ok(name).ok_or(Errno::ENOENT)?);
            let to_parent = fs.path_of(newparent)?;
            let to = join(&to_parent, name_ok(newname).ok_or(Errno::EINVAL)?);
            if from == to {
                return Ok(());
            }
            let source = fs.resolve(&from)?.ok_or(Errno::ENOENT)?;
            let dest = fs.resolve(&to)?;
            if dest.is_some() && flags.contains(RenameFlags::RENAME_NOREPLACE) {
                return Err(Errno::EEXIST);
            }
            let source_dir = match &source {
                Node::Upper(m) => m.is_dir(),
                Node::Lower(e) => e.kind == Kind::Dir,
                Node::Top => return Err(Errno::EBUSY),
            };
            if source_dir {
                // A folder with lower content cannot move inside the upper alone (EXDEV: the caller copies).
                if !fs.hidden(&from) && fs.lower.dir(&from).is_ok() {
                    return Err(Errno::EXDEV);
                }
                if fs.lower_entry(&to)?.is_some() && !fs.hidden(&to) {
                    return Err(Errno::EXDEV);
                }
            } else {
                fs.copy_up(&from)?;
            }
            if let Some(node) = &dest {
                let dest_dir = match node {
                    Node::Upper(m) => m.is_dir(),
                    Node::Lower(e) => e.kind == Kind::Dir,
                    Node::Top => true,
                };
                if dest_dir != source_dir {
                    return Err(if dest_dir {
                        Errno::EISDIR
                    } else {
                        Errno::ENOTDIR
                    });
                }
                if dest_dir && !fs.merged(&to)?.is_empty() {
                    return Err(Errno::ENOTEMPTY);
                }
            }
            fs.upper_dirs(&to_parent)?;
            if !source_dir {
                fs.diverge(&to);
            }
            std::fs::rename(fs.up(&from), fs.up(&to)).map_err(Errno::from)?;
            if !source_dir {
                fs.whiteout(&to, false);
            }
            // Bases of files created inside a moved upper-only folder move with them.
            {
                let mut inner = fs.inner.lock().unwrap();
                let moved: Vec<(String, Option<String>)> = inner
                    .meta
                    .base
                    .iter()
                    .filter(|(p, _)| source_dir && is_under(p, &from))
                    .map(|(p, b)| (p.clone(), b.clone()))
                    .collect();
                for (p, b) in moved {
                    inner.meta.base.remove(&p);
                    inner
                        .meta
                        .base
                        .insert(format!("{to}{}", &p[from.len()..]), b);
                }
                fs.save(&inner);
            }
            if fs.lower_entry(&from)?.is_some() && !fs.hidden(&from) {
                fs.diverge(&from);
                fs.whiteout(&from, true);
            } else {
                fs.forget_base_if_new(&from);
            }
            fs.rename_inodes(&from, &to);
            Ok(())
        })();
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let fs = &self.0;
        let result = (|| {
            let path = fs.path_of(ino)?;
            let acc = flags.0 & libc::O_ACCMODE;
            let write =
                acc == libc::O_WRONLY || acc == libc::O_RDWR || flags.0 & libc::O_TRUNC != 0;
            let node = fs.resolve(&path)?.ok_or(Errno::ENOENT)?;
            let file = if write {
                fs.copy_up(&path)?;
                let mut o = OpenOptions::new();
                o.read(true).write(true);
                if flags.0 & libc::O_TRUNC != 0 {
                    o.truncate(true);
                }
                if flags.0 & libc::O_APPEND != 0 {
                    o.append(true);
                }
                o.open(fs.up(&path)).map_err(Errno::from)?
            } else {
                match node {
                    Node::Upper(_) => File::open(fs.up(&path)).map_err(Errno::from)?,
                    Node::Lower(Entry {
                        kind: Kind::File { .. },
                        digest: Some(d),
                        ..
                    }) => {
                        let object = fs.lower.object(&path, &d).map_err(lower_errno)?;
                        File::open(object).map_err(Errno::from)?
                    }
                    _ => return Err(Errno::EISDIR),
                }
            };
            Ok::<_, Errno>(fs.handle(file, &path, write))
        })();
        match result {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = self.0.file_of(fh).and_then(|file| {
            let mut buf = vec![0u8; size as usize];
            let mut got = 0;
            while got < buf.len() {
                match file.read_at(&mut buf[got..], offset + got as u64) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(Errno::from(e)),
                }
            }
            buf.truncate(got);
            Ok(buf)
        });
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _wf: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = self
            .0
            .file_of(fh)
            .and_then(|file| file.write_all_at(data, offset).map_err(Errno::from));
        match result {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.0.inner.lock().unwrap().handles.remove(&fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.0.file_of(fh).and_then(|f| {
            if datasync {
                f.sync_data()
            } else {
                f.sync_all()
            }
            .map_err(Errno::from)
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let fs = &self.0;
        let result = (|| {
            let path = fs.path_of(ino)?;
            let parent_ino = if path.is_empty() {
                1
            } else {
                fs.ino_of(parent_of(&path))
            };
            let mut all = vec![
                (ino.0, FileType::Directory, ".".to_string()),
                (parent_ino, FileType::Directory, "..".to_string()),
            ];
            for (name, kind) in fs.merged(&path)? {
                all.push((fs.ino_of(&join(&path, &name)), kind, name));
            }
            Ok::<_, Errno>(all)
        })();
        match result {
            Ok(all) => {
                for (i, (ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
                    if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let path =
            std::ffi::CString::new(self.0.upper.as_os_str().as_encoded_bytes()).unwrap_or_default();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is a NUL-terminated string and `st` a properly sized out-parameter.
        if unsafe { libc::statvfs(path.as_ptr(), &mut st) } == 0 {
            reply.statfs(
                st.f_blocks,
                st.f_bfree,
                st.f_bavail,
                st.f_files,
                st.f_ffree,
                st.f_bsize as u32,
                255,
                st.f_frsize as u32,
            );
        } else {
            reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let fs = &self.0;
        let result = (|| {
            let name = name_ok(name).ok_or(Errno::EINVAL)?;
            let parent = fs.path_of(parent)?;
            let path = join(&parent, name);
            let exists = fs.resolve(&path)?;
            if exists.is_some() && flags & libc::O_EXCL != 0 {
                return Err(Errno::EEXIST);
            }
            if exists.is_some() {
                fs.copy_up(&path)?;
            } else {
                fs.upper_dirs(&parent)?;
                fs.diverge(&path);
            }
            let mut o = OpenOptions::new();
            o.read(true)
                .write(true)
                .create(true)
                .mode(mode & !umask & 0o7777);
            if flags & libc::O_TRUNC != 0 {
                o.truncate(true);
            }
            if flags & libc::O_APPEND != 0 {
                o.append(true);
            }
            let file = o.open(fs.up(&path)).map_err(Errno::from)?;
            fs.whiteout(&path, false);
            let node = fs.resolve(&path)?.ok_or(Errno::EIO)?;
            let ino = fs.ino_of(&path);
            Ok::<_, Errno>((fs.attr(ino, &node), fs.handle(file, &path, true)))
        })();
        match result {
            Ok((attr, fh)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }
}
