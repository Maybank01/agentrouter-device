//! Checkouts: a space version as an ordinary folder on this host, fetched on first read, copy-on-write (Cloud
//! PROJECT-SERVICE.md §5.5, #2767). The OS adapter of read path (b) — commands, tasks, runtimes and editors see normal
//! files; the Agent's own file tools keep reading through the cache API on the platform side.
//!
//! hostd's own tools (`_…`: the aggregator never lists them, so Agents never see them; Control calls them as
//! `host___space_*`):
//!
//! - `_space_mount`: mounts (or moves to another version / renews the ticket of) the checkout at a folder. The lower
//!   layer reads the node daemon's content routes with a checkout ticket (`POST /v1/view/checkout` on the platform
//!   side); the upper layer is this host's own folder under the device home. `prefetch` fetches named files ahead of
//!   their first read (a task's declared inputs); `settle` drops upper files the new version holds the same.
//! - `_space_prefetch`: fetches now the files a task's declared `inputs` select in a folder of a checkout (selected by
//!   the node daemon, `POST /v1/content/select`), several at once, within a byte budget — a task's stage then copies
//!   them from this host's cache instead of fetching them one by one on first read.
//! - `_space_changes`: the upper layer as changes (puts with the lower digest they replace, deletes), for the one sync
//!   engine, plus this checkout's cache counters.
//! - `_space_unmount`: ends the checkout (the upper layer stays on disk for the next mount at the same folder).
//!
//! Linux only for now (`fuser`, MIT, on `/dev/fuse`); Windows (Cloud Files API / ProjFS) and macOS (NFS loopback) are
//! later steps of #2767 — there these tools answer `CHECKOUT_UNSUPPORTED`, and the platform keeps a full copy.

#[cfg(target_os = "linux")]
mod fs;
#[cfg(target_os = "linux")]
mod lower;
#[cfg(all(test, target_os = "linux"))]
mod tests;

use serde_json::{Value, json};

use super::tool;
#[cfg(not(target_os = "linux"))]
use super::{Fail, Host};

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "_space_mount",
            "Platform use: mount a space version (a snapshot root) at a folder of this host as an on-demand checkout — files are fetched from the node cache on first read, writes go copy-on-write into this host's upper layer. Mounting again at the same folder moves it to the given version (settle: drop upper files the new version holds the same). prefetch: files to fetch now. Linux only.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "the folder (absolute) the checkout appears at"},
                "origin": {"type": "string", "description": "the node daemon, http(s)://host[:port]"},
                "root": {"type": "string"},
                "exp": {"type": "integer"},
                "t": {"type": "string"},
                "prefix": {"type": "string", "description": "the snapshot path shown at the folder (e.g. work)"},
                "owner": {"type": "object", "properties": {"uid": {"type": "integer"}, "gid": {"type": "integer"}}},
                "settle": {"type": "boolean"},
                "prefetch": {"type": "array", "items": {"type": "string"}, "maxItems": 5000},
            }, "required": ["path", "origin", "root", "exp", "t"]}),
            false,
        ),
        tool(
            "_space_prefetch",
            "Platform use: fetch now the files a task's declared inputs select in a folder of a mounted checkout (several at once, at most 1 GiB; the rest stay on demand). Linux only.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "a folder inside a mounted checkout (a space's folder, <mount>/<space>)"},
                "inputs": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 200},
            }, "required": ["path", "inputs"]}),
            false,
        ),
        tool(
            "_space_changes",
            "Platform use: the changes in a checkout's upper layer (puts with the version digest they replace, deletes) and its cache counters.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            true,
        ),
        tool(
            "_space_unmount",
            "Platform use: end a checkout (its upper layer stays on this host for the next mount at the same folder).",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
            false,
        ),
    ]
}

#[cfg(not(target_os = "linux"))]
fn unsupported(operation: &str) -> Fail {
    Fail::coded(
        "CHECKOUT_UNSUPPORTED",
        format!(
            "这台机器的系统还不能按需检出空间（{operation}）：只有 Linux 可以；平台会改用完整拷贝"
        ),
        json!({"component": "hostd", "operation": operation, "os": std::env::consts::OS, "retryable": false,
               "next": "use the full copy (the sync engine's materialised view) on this OS"}),
    )
}

#[cfg(not(target_os = "linux"))]
#[derive(Default)]
pub(super) struct Spaces {}

#[cfg(not(target_os = "linux"))]
impl Host {
    pub(super) fn space_mount(&self, _args: &Value) -> Result<Value, Fail> {
        Err(unsupported("_space_mount"))
    }
    pub(super) fn space_changes(&self, _args: &Value) -> Result<Value, Fail> {
        Err(unsupported("_space_changes"))
    }
    pub(super) fn space_prefetch(&self, _args: &Value) -> Result<Value, Fail> {
        Err(unsupported("_space_prefetch"))
    }
    pub(super) fn space_unmount(&self, _args: &Value) -> Result<Value, Fail> {
        Err(unsupported("_space_unmount"))
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::Spaces;

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use agentrouter_device::util::hex;
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};

    use super::super::{Fail, Host, arg_bool, arg_str, shown};
    use super::fs::{Shared, SpaceFs};
    use super::lower::{HttpSource, Lower, Ticket};

    /// A prefetch fetches at most this many bytes and files (the rest stay on demand: never a whole large 作品).
    const PREFETCH_BYTES: u64 = 1 << 30;
    const PREFETCH_FILES: usize = 5000;

    struct Mounted {
        fs: Arc<SpaceFs>,
        _session: fuser::BackgroundSession,
    }

    /// The checkouts of this `hostd`, by folder.
    #[derive(Default)]
    pub(in crate::host) struct Spaces {
        mounts: Mutex<HashMap<PathBuf, Mounted>>,
    }

    impl Spaces {
        pub(in crate::host) fn end_all(&self) {
            self.mounts.lock().unwrap().clear();
        }
    }

    fn fail(
        code: &'static str,
        operation: &str,
        path: &Path,
        message: String,
        retryable: bool,
        next: &str,
    ) -> Fail {
        Fail::coded(
            code,
            format!("{}：{message}", shown(path)),
            json!({"component": "hostd", "operation": operation, "path": shown(path), "cause": {"message": message},
                   "retryable": retryable, "next": next}),
        )
    }

    fn ticket_of(args: &Value, path: &Path) -> Result<Ticket, Fail> {
        let text = |k: &str| arg_str(args, k).map(str::to_string);
        let origin =
            text("origin").filter(|o| o.starts_with("http://") || o.starts_with("https://"));
        let (Some(origin), Some(root), Some(exp), Some(t)) =
            (origin, text("root"), args["exp"].as_u64(), text("t"))
        else {
            return Err(fail(
                "HOSTD_ARGS_INVALID",
                "_space_mount",
                path,
                "origin (http/https), root, exp and t are required".into(),
                false,
                "pass the checkout ticket as POST /v1/view/checkout answered it",
            ));
        };
        let prefix = text("prefix")
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        Ok(Ticket {
            origin,
            root,
            exp,
            t,
            prefix,
        })
    }

    /// This checkout's own state folder: the upper layer and its meta, under the device home, by folder.
    fn state_dir(path: &Path) -> PathBuf {
        let key = hex(&Sha256::digest(path.as_os_str().as_encoded_bytes()));
        agentrouter_device::util::data_dir()
            .join("space")
            .join(&key[..24])
    }

    impl Host {
        pub(in crate::host) fn space_mount(&self, args: &Value) -> Result<Value, Fail> {
            let raw = arg_str(args, "path").unwrap_or_default();
            let path = self.absolute(raw);
            let ticket = ticket_of(args, &path)?;
            let prefetch: Vec<String> = args["prefetch"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(|s| s.trim_matches('/').to_string()))
                        .take(5000)
                        .collect()
                })
                .unwrap_or_default();
            let mut mounts = self.spaces.mounts.lock().unwrap();
            if let Some(m) = mounts.get(&path) {
                let before = m.fs.lower.ticket();
                m.fs.lower.switch(ticket.clone());
                // The new version must answer before it replaces the old one in programs' eyes.
                if let Err(e) = m.fs.lower.dir("") {
                    m.fs.lower.switch(before);
                    return Err(fail(
                        "CHECKOUT_SOURCE_FAILED",
                        "_space_mount",
                        &path,
                        format!("the new version could not be read: {e:?}"),
                        true,
                        "ask a new checkout ticket and mount again",
                    ));
                }
                let settled = if arg_bool(args, "settle") {
                    m.fs.settle()
                } else {
                    0
                };
                let fs = m.fs.clone();
                drop(mounts);
                let fetched = fs.prefetch(&prefetch);
                return Ok(
                    json!({"path": shown(&path), "root": ticket.root, "created": false, "moved": before.root != ticket.root,
                                 "settled": settled, "prefetched": fetched, "stats": fs.lower.stats_json()}),
                );
            }
            let state = state_dir(&path);
            let lower = Lower::new(
                Box::new(HttpSource::new()),
                ticket.clone(),
                state.join("objects"),
            )
            .map_err(|e| {
                fail(
                    "IO_ERROR",
                    "_space_mount",
                    &state,
                    e.to_string(),
                    false,
                    "free disk space under the device home",
                )
            })?;
            let lower = Arc::new(lower);
            lower.dir("").map_err(|e| {
                fail(
                    "CHECKOUT_SOURCE_FAILED",
                    "_space_mount",
                    &path,
                    format!("the version's top could not be read: {e:?}"),
                    true,
                    "check the node daemon origin and the checkout ticket",
                )
            })?;
            // SAFETY: getuid/getgid have no preconditions.
            let me = unsafe { (libc::getuid(), libc::getgid()) };
            // Shown as the user commands run as: hostd runs every command itself (`host__exec` never changes user), so
            // that is hostd's own user, and `git` and friends see their own files; the caller may name another. Not the
            // owner of the folder around it (Dev 10-22, Cloud #2767): a cloud session folder belonged to uid 1000 while
            // commands ran as root, so the checkout showed another user's files and `git` refuses those ("dubious ownership").
            let owner = (
                args["owner"]["uid"]
                    .as_u64()
                    .map(|v| v as u32)
                    .unwrap_or(me.0),
                args["owner"]["gid"]
                    .as_u64()
                    .map(|v| v as u32)
                    .unwrap_or(me.1),
            );
            let fs = Arc::new(SpaceFs::new(lower, &state, owner).map_err(|e| {
                fail(
                    "IO_ERROR",
                    "_space_mount",
                    &state,
                    e.to_string(),
                    false,
                    "free disk space under the device home",
                )
            })?);
            std::fs::create_dir_all(&path).map_err(|e| {
                fail(
                    "IO_ERROR",
                    "_space_mount",
                    &path,
                    e.to_string(),
                    false,
                    "choose a folder hostd can create",
                )
            })?;
            let mut config = fuser::Config::default();
            config.mount_options = vec![
                fuser::MountOption::FSName("agentrouter-space".into()),
                fuser::MountOption::Subtype("checkout".into()),
            ];
            // Commands run as another user than hostd: others may use the folder (root may always; else the
            // system's fuse.conf must allow it).
            config.acl = if me.0 == 0 || arg_bool(args, "allowOther") {
                fuser::SessionACL::All
            } else {
                fuser::SessionACL::Owner
            };
            config.n_threads = Some(8);
            let session = fuser::spawn_mount(Shared(fs.clone()), &path, &config).map_err(|e| {
                fail("CHECKOUT_MOUNT_FAILED", "_space_mount", &path, format!("FUSE mount: {e}"), false, "the environment needs /dev/fuse (and fusermount3 when hostd is not root); the platform falls back to a full copy")
            })?;
            mounts.insert(
                path.clone(),
                Mounted {
                    fs: fs.clone(),
                    _session: session,
                },
            );
            drop(mounts);
            let settled = if arg_bool(args, "settle") {
                fs.settle()
            } else {
                0
            };
            let fetched = fs.prefetch(&prefetch);
            Ok(
                json!({"path": shown(&path), "root": ticket.root, "created": true, "moved": true, "settled": settled,
                      "prefetched": fetched, "stats": fs.lower.stats_json()}),
            )
        }

        pub(in crate::host) fn space_changes(&self, args: &Value) -> Result<Value, Fail> {
            let path = self.absolute(arg_str(args, "path").unwrap_or_default());
            let fs = self
                .spaces
                .mounts
                .lock()
                .unwrap()
                .get(&path)
                .map(|m| m.fs.clone());
            let Some(fs) = fs else {
                return Err(fail(
                    "CHECKOUT_NOT_MOUNTED",
                    "_space_changes",
                    &path,
                    "no checkout is mounted here".into(),
                    false,
                    "mount it first with host___space_mount",
                ));
            };
            let changes = fs.changes().map_err(|e| {
                fail(
                    "CHECKOUT_CHANGES_FAILED",
                    "_space_changes",
                    &path,
                    e.to_string(),
                    true,
                    "retry; if it stays, unmount and mount again",
                )
            })?;
            Ok(
                json!({"path": shown(&path), "root": fs.lower.ticket().root, "changes": changes, "stats": fs.lower.stats_json()}),
            )
        }

        /// A task's declared inputs in a folder of a checkout, fetched now (within PREFETCH_BYTES / PREFETCH_FILES).
        pub(in crate::host) fn space_prefetch(&self, args: &Value) -> Result<Value, Fail> {
            let path = self.absolute(arg_str(args, "path").unwrap_or_default());
            let inputs: Vec<String> = args["inputs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|p| p.as_str().map(str::to_string))
                        .take(200)
                        .collect()
                })
                .unwrap_or_default();
            if inputs.is_empty() {
                return Err(fail(
                    "HOSTD_ARGS_INVALID",
                    "_space_prefetch",
                    &path,
                    "inputs must name 1 to 200 path patterns".into(),
                    false,
                    "pass the task's declared inputs",
                ));
            }
            // The checkout this folder is in (the deepest mount above it), and the folder relative to its top.
            let found = self
                .spaces
                .mounts
                .lock()
                .unwrap()
                .iter()
                .filter(|(top, _)| path.starts_with(top))
                .max_by_key(|(top, _)| top.as_os_str().len())
                .map(|(top, m)| (top.clone(), m.fs.clone()));
            let Some((top, fs)) = found else {
                return Err(fail(
                    "CHECKOUT_NOT_MOUNTED",
                    "_space_prefetch",
                    &path,
                    "no checkout is mounted at or above this folder".into(),
                    false,
                    "mount it first with host___space_mount (or read the files as they are)",
                ));
            };
            let rel = path
                .strip_prefix(&top)
                .map(|r| r.to_string_lossy().trim_matches('/').to_string())
                .unwrap_or_default();
            let started = std::time::Instant::now();
            let (files, more) = fs.lower.select(&rel, &inputs).map_err(|e| {
                fail(
                    "CHECKOUT_SOURCE_FAILED",
                    "_space_prefetch",
                    &path,
                    format!("the node daemon could not select the inputs: {e:?}"),
                    true,
                    "the task's stage reads its inputs on demand instead; retry the prefetch later",
                )
            })?;
            let (mut bytes, mut wanted) = (0u64, Vec::new());
            for (file, size) in &files {
                if wanted.len() >= PREFETCH_FILES || bytes + size > PREFETCH_BYTES {
                    break;
                }
                bytes += size;
                wanted.push(if rel.is_empty() {
                    file.clone()
                } else {
                    format!("{rel}/{file}")
                });
            }
            let fetched = fs.prefetch(&wanted);
            Ok(
                json!({"path": shown(&path), "selected": files.len(), "more": more, "prefetched": fetched,
                      "within": wanted.len(), "bytes": bytes, "ms": started.elapsed().as_millis() as u64,
                      "stats": fs.lower.stats_json()}),
            )
        }

        pub(in crate::host) fn space_unmount(&self, args: &Value) -> Result<Value, Fail> {
            let path = self.absolute(arg_str(args, "path").unwrap_or_default());
            let gone = self.spaces.mounts.lock().unwrap().remove(&path).is_some();
            Ok(json!({"path": shown(&path), "unmounted": gone}))
        }
    }
}
