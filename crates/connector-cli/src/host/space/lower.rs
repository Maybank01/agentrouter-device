//! The lower layer of a checkout: one space version (a snapshot root), read-only, fetched on first read (Cloud
//! PROJECT-SERVICE.md §5.5.2). Listings and file bytes come from the node daemon's content routes (the node-level
//! cache, T1) with a checkout ticket; this process keeps what it fetched by digest — objects are immutable, so nothing
//! here is ever invalidated: a new version is a new root whose unchanged directories and files hit the same entries.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use agentrouter_device::util::hex;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// The largest file this layer fetches whole (bigger files wait for chunked reads, §4.1 chunk lists).
pub const OBJECT_MAX: u64 = 2 << 30;

/// Where and what to read: the node daemon's origin, the snapshot root and its checkout ticket (`POST
/// /v1/view/checkout`), and the path inside the snapshot that is this mount's top (`work`).
#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    pub origin: String,
    pub root: String,
    pub exp: u64,
    pub t: String,
    pub prefix: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Dir,
    File { executable: bool },
    Link(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// `<sha256>-<size>` for files and directories.
    pub digest: Option<String>,
}

/// Why the lower layer could not answer: the path is not there, or the source failed (named for the caller).
#[derive(Debug, Clone, PartialEq)]
pub enum LowerError {
    NotFound,
    Failed { code: String, message: String },
}

/// Where listings and bytes come from (the node daemon over HTTP; a map in tests). Paths are snapshot paths.
pub trait Source: Send + Sync {
    fn list(&self, ticket: &Ticket, path: &str) -> Result<Vec<Entry>, LowerError>;
    /// The whole file at `path`, with the digest the source says it has.
    fn read(&self, ticket: &Ticket, path: &str) -> Result<(Vec<u8>, String), LowerError>;
}

/// Counters of this layer (the T0 tier of §5.1, per environment instance) for the sync's answer.
#[derive(Default)]
pub struct Stats {
    pub object_hits: AtomicU64,
    pub object_misses: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub fetch_ms_total: AtomicU64,
    pub fetch_ms_max: AtomicU64,
    pub list_hits: AtomicU64,
    pub list_misses: AtomicU64,
}

pub struct Lower {
    source: Box<dyn Source>,
    ticket: RwLock<Ticket>,
    /// Directory listings by directory digest (and the mount's top by root + prefix).
    dirs: Mutex<HashMap<String, Arc<Vec<Entry>>>>,
    /// File bytes by digest: `<objects>/<sha256>-<size>`.
    objects: PathBuf,
    pub stats: Stats,
    /// The newest failure of the source, for the sync's answer (a mount that cannot read says why).
    pub last_error: Mutex<Option<Value>>,
}

impl Lower {
    pub fn new(
        source: Box<dyn Source>,
        ticket: Ticket,
        objects: PathBuf,
    ) -> std::io::Result<Lower> {
        std::fs::create_dir_all(&objects)?;
        Ok(Lower {
            source,
            ticket: RwLock::new(ticket),
            dirs: Mutex::default(),
            objects,
            stats: Stats::default(),
            last_error: Mutex::default(),
        })
    }

    pub fn ticket(&self) -> Ticket {
        self.ticket.read().unwrap().clone()
    }

    /// Moves the layer to another version (or renews the ticket of the same one). Cached listings and objects stay:
    /// they are keyed by digest.
    pub fn switch(&self, ticket: Ticket) {
        *self.ticket.write().unwrap() = ticket;
    }

    fn snapshot_path(ticket: &Ticket, rel: &str) -> String {
        match (ticket.prefix.is_empty(), rel.is_empty()) {
            (true, _) => rel.to_string(),
            (false, true) => ticket.prefix.clone(),
            (false, false) => format!("{}/{rel}", ticket.prefix),
        }
    }

    fn note(&self, error: &LowerError) {
        if let LowerError::Failed { code, message } = error {
            *self.last_error.lock().unwrap() = Some(json!({"code": code, "message": message}));
        }
    }

    fn listing(
        &self,
        ticket: &Ticket,
        key: &str,
        rel: &str,
    ) -> Result<Arc<Vec<Entry>>, LowerError> {
        if let Some(hit) = self.dirs.lock().unwrap().get(key) {
            self.stats.list_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit.clone());
        }
        self.stats.list_misses.fetch_add(1, Ordering::Relaxed);
        let entries = self
            .source
            .list(ticket, &Self::snapshot_path(ticket, rel))
            .inspect_err(|e| self.note(e))?;
        let entries = Arc::new(entries);
        self.dirs
            .lock()
            .unwrap()
            .insert(key.to_string(), entries.clone());
        Ok(entries)
    }

    /// The listing of the directory at `rel` (relative to the mount's top), or NotFound when it is not a directory.
    pub fn dir(&self, rel: &str) -> Result<Arc<Vec<Entry>>, LowerError> {
        let ticket = self.ticket();
        let mut here = String::new();
        let mut listing = self.listing(
            &ticket,
            &format!("top:{}:{}", ticket.root, ticket.prefix),
            "",
        )?;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            let entry = listing
                .iter()
                .find(|e| e.name == part)
                .ok_or(LowerError::NotFound)?;
            let (Kind::Dir, Some(digest)) = (&entry.kind, &entry.digest) else {
                return Err(LowerError::NotFound);
            };
            here = if here.is_empty() {
                part.to_string()
            } else {
                format!("{here}/{part}")
            };
            listing = self.listing(&ticket, &format!("dir:{digest}"), &here)?;
        }
        Ok(listing)
    }

    /// The entry at `rel` (never the top itself).
    pub fn entry(&self, rel: &str) -> Result<Entry, LowerError> {
        let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        if name.is_empty() {
            return Err(LowerError::NotFound);
        }
        self.dir(parent)?
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or(LowerError::NotFound)
    }

    pub fn object_path(&self, digest: &str) -> PathBuf {
        self.objects.join(digest)
    }

    /// The local file holding the bytes of the file at `rel` (fetched now on its first read on this host), checked
    /// against its digest.
    pub fn object(&self, rel: &str, digest: &str) -> Result<PathBuf, LowerError> {
        let path = self.object_path(digest);
        if path.is_file() {
            self.stats.object_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(path);
        }
        self.stats.object_misses.fetch_add(1, Ordering::Relaxed);
        let ticket = self.ticket();
        let started = Instant::now();
        let (bytes, said) = self
            .source
            .read(&ticket, &Self::snapshot_path(&ticket, rel))
            .inspect_err(|e| self.note(e))?;
        let ms = started.elapsed().as_millis() as u64;
        self.stats.fetch_ms_total.fetch_add(ms, Ordering::Relaxed);
        self.stats.fetch_ms_max.fetch_max(ms, Ordering::Relaxed);
        self.stats
            .fetched_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let actual = format!("{}-{}", hex(&Sha256::digest(&bytes)), bytes.len());
        if actual != digest || said != digest {
            let error = LowerError::Failed {
                code: "CHECKOUT_DIGEST_MISMATCH".into(),
                message: format!(
                    "{rel}: the content read has digest {actual} (the source said {said}), the version names {digest}"
                ),
            };
            self.note(&error);
            return Err(error);
        }
        let part = self
            .objects
            .join(format!("{digest}.{}.part", std::process::id()));
        let written = std::fs::File::create(&part)
            .and_then(|mut f| f.write_all(&bytes))
            .and_then(|_| std::fs::rename(&part, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&part);
            let error = LowerError::Failed {
                code: "CHECKOUT_CACHE_WRITE_FAILED".into(),
                message: format!(
                    "{rel}: could not keep the fetched file in {}: {e}",
                    self.objects.display()
                ),
            };
            self.note(&error);
            return Err(error);
        }
        Ok(path)
    }

    pub fn stats_json(&self) -> Value {
        let s = &self.stats;
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        json!({
            "objectHits": get(&s.object_hits), "objectMisses": get(&s.object_misses),
            "fetchedBytes": get(&s.fetched_bytes), "fetchMsTotal": get(&s.fetch_ms_total), "fetchMsMax": get(&s.fetch_ms_max),
            "listHits": get(&s.list_hits), "listMisses": get(&s.list_misses),
            "lastError": self.last_error.lock().unwrap().clone(),
        })
    }
}

/// The node daemon's content routes (`GET /v1/content/{list,read}`), through the sandbox's egress proxy when the
/// environment names one (ureq reads `HTTP_PROXY` / `NO_PROXY`).
pub struct HttpSource {
    agent: ureq::Agent,
}

impl HttpSource {
    pub fn new() -> HttpSource {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_global(Some(Duration::from_secs(15 * 60)))
            .http_status_as_error(false);
        HttpSource {
            agent: config.build().new_agent(),
        }
    }

    fn get(
        &self,
        ticket: &Ticket,
        route: &str,
        path: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, LowerError> {
        let url = format!("{}/v1/content/{route}", ticket.origin.trim_end_matches('/'));
        let response = self
            .agent
            .get(&url)
            .query("root", &ticket.root)
            .query("exp", ticket.exp.to_string())
            .query("t", &ticket.t)
            .query("path", path)
            .call()
            .map_err(|e| LowerError::Failed {
                code: "CHECKOUT_SOURCE_UNREACHABLE".into(),
                message: format!("{route} {path} at {}: {e}", ticket.origin),
            })?;
        match response.status().as_u16() {
            200 => Ok(response),
            404 => Err(LowerError::NotFound),
            status => {
                let mut body = response.into_body();
                let text = body.read_to_string().unwrap_or_default();
                let code = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["code"].as_str().map(str::to_string))
                    .unwrap_or_else(|| format!("HTTP_{status}"));
                Err(LowerError::Failed {
                    code,
                    message: format!(
                        "{route} {path} at {}: {status} {}",
                        ticket.origin,
                        text.chars().take(300).collect::<String>()
                    ),
                })
            }
        }
    }
}

impl Source for HttpSource {
    fn list(&self, ticket: &Ticket, path: &str) -> Result<Vec<Entry>, LowerError> {
        let mut body = self.get(ticket, "list", path)?.into_body();
        let text = body.read_to_string().map_err(|e| LowerError::Failed {
            code: "CHECKOUT_SOURCE_UNREACHABLE".into(),
            message: format!("list {path}: {e}"),
        })?;
        parse_listing(&text).ok_or_else(|| LowerError::Failed {
            code: "CHECKOUT_SOURCE_INVALID".into(),
            message: format!("list {path}: not a listing"),
        })
    }

    fn read(&self, ticket: &Ticket, path: &str) -> Result<(Vec<u8>, String), LowerError> {
        let response = self.get(ticket, "read", path)?;
        let said = response
            .headers()
            .get("x-ps-digest")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_with_config()
            .limit(OBJECT_MAX)
            .reader()
            .read_to_end(&mut bytes)
            .map_err(|e| LowerError::Failed {
                code: "CHECKOUT_SOURCE_UNREACHABLE".into(),
                message: format!("read {path}: {e}"),
            })?;
        Ok((bytes, said))
    }
}

/// `{ entries: [{ name, kind: dir|file|link, size, digest?, executable?, target? }] }`.
pub fn parse_listing(text: &str) -> Option<Vec<Entry>> {
    let value: Value = serde_json::from_str(text).ok()?;
    value["entries"]
        .as_array()?
        .iter()
        .map(|e| {
            let name = e["name"].as_str()?.to_string();
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return None;
            }
            let kind = match e["kind"].as_str()? {
                "dir" => Kind::Dir,
                "file" => Kind::File {
                    executable: e["executable"].as_bool() == Some(true),
                },
                "link" => Kind::Link(e["target"].as_str()?.to_string()),
                _ => return None,
            };
            Some(Entry {
                name,
                kind,
                size: e["size"].as_u64().unwrap_or(0),
                digest: e["digest"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// A file's `<sha256>-<size>`, as the store names it.
pub fn digest_of(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hash.update(&buf[..n]);
    }
    Ok(format!("{}-{size}", hex(&hash.finalize())))
}
