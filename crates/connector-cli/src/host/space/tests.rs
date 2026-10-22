//! A checkout mounted through FUSE over an in-memory version (skipped where this machine cannot mount FUSE: no
//! `/dev/fuse`, or not root and no `fusermount3`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentrouter_device::util::hex;
use sha2::{Digest as _, Sha256};

use super::fs::{Change, Shared, SpaceFs};
use super::lower::{Entry, Kind, Lower, LowerError, Source, Ticket};

/// One version: path → file content (`None` = a directory).
type Version = HashMap<String, Option<Vec<u8>>>;

/// Versions by root: path → file content (`None` content = a directory); a link is `@target`.
#[derive(Default)]
struct MapSource {
    versions: Mutex<HashMap<String, Version>>,
    reads: AtomicUsize,
}

fn digest(bytes: &[u8]) -> String {
    format!("{}-{}", hex(&Sha256::digest(bytes)), bytes.len())
}

impl MapSource {
    fn add(&self, root: &str, files: &[(&str, &str)]) {
        let mut all: Version = HashMap::new();
        all.insert("work".into(), None);
        for (path, content) in files {
            let full = format!("work/{path}");
            let mut here = full.as_str();
            while let Some((parent, _)) = here.rsplit_once('/') {
                all.entry(parent.to_string()).or_insert(None);
                here = parent;
            }
            all.insert(full, Some(content.as_bytes().to_vec()));
        }
        self.versions.lock().unwrap().insert(root.into(), all);
    }
}

impl Source for MapSource {
    fn list(&self, ticket: &Ticket, path: &str) -> Result<Vec<Entry>, LowerError> {
        let versions = self.versions.lock().unwrap();
        let all = versions.get(&ticket.root).ok_or(LowerError::NotFound)?;
        if all.get(path) != Some(&None) {
            return Err(LowerError::NotFound);
        }
        let mut out: Vec<Entry> = all
            .iter()
            .filter_map(|(p, c)| {
                let name = p.strip_prefix(&format!("{path}/"))?;
                if name.contains('/') {
                    return None;
                }
                Some(match c {
                    None => {
                        // A directory's digest: its children's, so an unchanged folder keeps it across versions.
                        let inner: Vec<String> = all
                            .iter()
                            .filter(|(q, _)| q.starts_with(&format!("{p}/")))
                            .map(|(q, c)| format!("{q}{c:?}"))
                            .collect();
                        let mut inner = inner;
                        inner.sort();
                        Entry {
                            name: name.into(),
                            kind: Kind::Dir,
                            size: 0,
                            digest: Some(digest(inner.join("\n").as_bytes())),
                        }
                    }
                    Some(bytes) if bytes.starts_with(b"@") => Entry {
                        name: name.into(),
                        kind: Kind::Link(String::from_utf8_lossy(&bytes[1..]).into()),
                        size: 0,
                        digest: None,
                    },
                    Some(bytes) => Entry {
                        name: name.into(),
                        kind: Kind::File {
                            executable: name.ends_with(".sh"),
                        },
                        size: bytes.len() as u64,
                        digest: Some(digest(bytes)),
                    },
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read(&self, ticket: &Ticket, path: &str) -> Result<(Vec<u8>, String), LowerError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let versions = self.versions.lock().unwrap();
        match versions.get(&ticket.root).and_then(|v| v.get(path)) {
            Some(Some(bytes)) => Ok((bytes.clone(), digest(bytes))),
            _ => Err(LowerError::NotFound),
        }
    }
}

struct SharedSource(Arc<MapSource>);
impl Source for SharedSource {
    fn list(&self, t: &Ticket, p: &str) -> Result<Vec<Entry>, LowerError> {
        self.0.list(t, p)
    }
    fn read(&self, t: &Ticket, p: &str) -> Result<(Vec<u8>, String), LowerError> {
        self.0.read(t, p)
    }
}

fn ticket(root: &str) -> Ticket {
    Ticket {
        origin: "http://node".into(),
        root: root.into(),
        exp: u64::MAX,
        t: "t".into(),
        prefix: "work".into(),
    }
}

fn can_mount() -> bool {
    let root = unsafe { libc::getuid() } == 0;
    let helper = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("fusermount3").is_file()))
        .unwrap_or(false);
    Path::new("/dev/fuse").exists() && (root || helper)
}

fn temp(name: &str) -> PathBuf {
    crate::host::tests::temp(&format!("space-{name}"))
}

fn puts(changes: &[Change]) -> Vec<(&str, &str)> {
    changes.iter().map(|c| (c.path.as_str(), c.op)).collect()
}

#[test]
fn a_checkout_reads_on_demand_copies_up_on_write_and_settles_on_the_next_version() {
    if !can_mount() {
        eprintln!("skipped: this machine cannot mount FUSE");
        return;
    }
    let source = Arc::new(MapSource::default());
    source.add(
        "v1",
        &[
            ("api/index.js", "api v1"),
            ("api/run.sh", "#!/bin/sh\necho hi\n"),
            ("web/a.txt", "a"),
            ("web/b.txt", "b"),
            ("big/never.bin", "never read"),
        ],
    );
    let base = temp("mount");
    let mnt = base.join("work");
    let lower = Arc::new(
        Lower::new(
            Box::new(SharedSource(source.clone())),
            ticket("v1"),
            base.join("objects"),
        )
        .unwrap(),
    );
    let fs = Arc::new(
        SpaceFs::new(
            lower.clone(),
            &base.join("state"),
            (unsafe { libc::getuid() }, unsafe { libc::getgid() }),
        )
        .unwrap(),
    );
    std::fs::create_dir_all(&mnt).unwrap();
    let mut config = fuser::Config::default();
    config.n_threads = Some(4);
    let session = fuser::spawn_mount(Shared(fs.clone()), &mnt, &config).unwrap();

    // Listing needs no file content; a dependency's file appears on its first read, nothing else is fetched.
    let mut names: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["api", "big", "web"]);
    assert_eq!(source.reads.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read_to_string(mnt.join("api/index.js")).unwrap(),
        "api v1"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("api/index.js")).unwrap(),
        "api v1"
    );
    assert_eq!(
        source.reads.load(Ordering::Relaxed),
        1,
        "read once, then from this host's cache"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(mnt.join("api/run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );

    // Writes: copy-up, a new file, a delete, a rename; nothing written reaches the lower.
    std::fs::write(mnt.join("web/a.txt"), "a2").unwrap();
    std::fs::write(mnt.join("web/new.txt"), "new").unwrap();
    std::fs::remove_file(mnt.join("web/b.txt")).unwrap();
    std::fs::rename(mnt.join("api/index.js"), mnt.join("api/main.js")).unwrap();
    std::fs::create_dir(mnt.join("web/sub")).unwrap();
    std::fs::write(mnt.join("web/sub/x.txt"), "x").unwrap();
    assert!(!mnt.join("web/b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(mnt.join("api/main.js")).unwrap(),
        "api v1"
    );
    assert_eq!(
        std::fs::rename(mnt.join("web"), mnt.join("web2"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EXDEV),
        "a folder with lower content is copied, never moved in the upper alone"
    );
    let changes = fs.changes().unwrap();
    assert_eq!(
        puts(&changes),
        [
            ("api/index.js", "delete"),
            ("api/main.js", "put"),
            ("web/a.txt", "put"),
            ("web/b.txt", "delete"),
            ("web/new.txt", "put"),
            ("web/sub/x.txt", "put")
        ]
    );
    let a = changes.iter().find(|c| c.path == "web/a.txt").unwrap();
    assert_eq!(
        a.base.as_deref(),
        Some(digest(b"a").as_str()),
        "the put names the version digest it replaces"
    );
    assert_eq!(
        changes
            .iter()
            .find(|c| c.path == "web/new.txt")
            .unwrap()
            .base,
        None
    );

    // A folder deleted whole: each of its lower files is a delete.
    std::fs::remove_file(mnt.join("big/never.bin")).unwrap();
    std::fs::remove_dir(mnt.join("big")).unwrap();
    assert!(puts(&fs.changes().unwrap()).contains(&("big/never.bin", "delete")));

    // The sync wrote them into the draft: the next version holds them, so the upper settles to nothing.
    source.add(
        "v2",
        &[
            ("api/main.js", "api v1"),
            ("api/run.sh", "#!/bin/sh\necho hi\n"),
            ("web/a.txt", "a2"),
            ("web/new.txt", "new"),
            ("web/sub/x.txt", "x"),
        ],
    );
    lower.switch(ticket("v2"));
    assert!(fs.settle() >= 5);
    assert_eq!(fs.changes().unwrap(), Vec::<Change>::new());
    assert_eq!(
        std::fs::read_to_string(mnt.join("web/a.txt")).unwrap(),
        "a2"
    );
    assert!(!mnt.join("big").exists());

    // A change made while main moved stays (a conflict is the sync's to report, never lost here).
    std::fs::write(mnt.join("web/a.txt"), "mine").unwrap();
    source.add("v3", &[("api/main.js", "api v1"), ("web/a.txt", "theirs")]);
    lower.switch(ticket("v3"));
    fs.settle();
    assert_eq!(
        std::fs::read_to_string(mnt.join("web/a.txt")).unwrap(),
        "mine"
    );
    let left = fs.changes().unwrap();
    assert_eq!(
        left.iter()
            .find(|c| c.path == "web/a.txt")
            .unwrap()
            .base
            .as_deref(),
        Some(digest(b"a2").as_str())
    );

    drop(session);
    let _ = std::fs::remove_dir_all(&base);
}
