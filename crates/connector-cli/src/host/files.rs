//! Files by their real path: `read`, `write` (chunked transfers), and the batched metadata a bound session needs
//! (`stat` of many paths in one call, optionally with small files' content; `list` of directories, to a depth).
//!
//! Versions, for guarded edits (DSH's `replaceIfVersion` / `createIfAbsent`): every `read`, `stat` and `write` answer
//! carries `version`, an opaque token of the file's stat identity (size, modification time in ns, and on Unix inode
//! and change time) that is cheap to compute, and whole-file reads and writes also carry its `sha256`. A `write` with
//! `expected` happens only if the file is still that version (and, when given, still has that sha256) or still
//! absent; otherwise it fails with code `STALE_VERSION` / `EXISTS` and the current version. Whole-file writes replace
//! the file atomically (a temporary file renamed over it, permissions kept).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde_json::{Value, json};

use agentrouter_device::util::{b64, b64_decode};

use super::{CHUNK_MAX, Fail, Host, arg_bool, arg_paths, arg_str, shown, tool};

/// Paths in one `stat`.
const STAT_MAX: usize = 1000;
/// Inline content of one small file in `stat`.
const STAT_CONTENT_MAX: u64 = 256 << 10;
/// Inline content of one `stat` call in total (under the link frame with base64).
const STAT_CONTENT_TOTAL: u64 = 4 << 20;
/// Folders in one `list`.
const LIST_PATHS_MAX: usize = 64;
const LIST_LIMIT_DEFAULT: u64 = 5000;
const LIST_LIMIT_MAX: u64 = 50_000;
const LIST_DEPTH_MAX: u64 = 16;

pub(super) fn tools() -> Vec<Value> {
    vec![
        tool(
            "read",
            "Read a file by its real path (relative = inside the first folder): utf8 text or base64 bytes, a range with offset/length (at most 4 MiB). Answers its version (and sha256 when the whole file was read, or on request), and for whole text files the line ending and BOM.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 0},
                "length": {"type": "integer", "minimum": 0, "maximum": CHUNK_MAX},
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
                "sha256": {"type": "boolean"},
            }, "required": ["path"]}),
            true,
        ),
        tool(
            "write",
            "Write a file by its real path (parents are created). Without offset: create or replace atomically. With offset: write there (truncate: cut the file to offset first). data is utf8 text or base64 bytes (at most 4 MiB). expected guards it: {version, sha256?} = only if the file is still that version, {absent: true} = only if it does not exist (else STALE_VERSION / EXISTS with the current version).",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "data": {"type": "string"},
                "encoding": {"type": "string", "enum": ["utf8", "base64"]},
                "offset": {"type": "integer", "minimum": 0},
                "truncate": {"type": "boolean"},
                "expected": {"type": "object", "properties": {
                    "version": {"type": "string"},
                    "sha256": {"type": "string"},
                    "absent": {"type": "boolean"},
                }},
            }, "required": ["path", "data"]}),
            false,
        ),
        tool(
            "stat",
            "Metadata of many paths in one call (kind, size, mtime, readonly, symlink); a missing path is exists:false, not an error. content_max > 0 also returns the content of files up to that size (utf8, else base64).",
            json!({"type": "object", "properties": {
                "paths": {"type": "array", "items": {"type": "string"}, "maxItems": STAT_MAX},
                "content_max": {"type": "integer", "minimum": 0, "maximum": STAT_CONTENT_MAX},
            }, "required": ["paths"]}),
            true,
        ),
        tool(
            "list",
            "List directories (path or paths): entries sorted by name with kind, size and mtime; depth > 1 walks down (names are relative paths). hidden (default true) includes dot entries; ignore (default false) applies .gitignore/.ignore like ripgrep. Symlinks are not followed.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "paths": {"type": "array", "items": {"type": "string"}, "maxItems": LIST_PATHS_MAX},
                "depth": {"type": "integer", "minimum": 1, "maximum": LIST_DEPTH_MAX},
                "hidden": {"type": "boolean"},
                "ignore": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIST_LIMIT_MAX},
            }}),
            true,
        ),
    ]
}

/// Milliseconds since the Unix epoch (negative before it), `null` where the filesystem has no mtime.
pub(super) fn mtime_ms(meta: &std::fs::Metadata) -> Value {
    let Ok(t) = meta.modified() else {
        return Value::Null;
    };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => json!(d.as_millis() as i64),
        Err(e) => json!(-(e.duration().as_millis() as i64)),
    }
}

fn kind(ft: std::fs::FileType) -> &'static str {
    if ft.is_symlink() {
        "symlink"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    }
}

/// The opaque version of a file: its stat identity, cheap to compute on every `stat` (DSH's local backend does the
/// same). Content changes that keep size and modification time (same ns) are caught by `sha256` in `expected`.
pub(super) fn version(meta: &std::fs::Metadata) -> String {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let extra = {
        use std::os::unix::fs::MetadataExt;
        format!(
            "-{:x}-{:x}.{:x}",
            meta.ino(),
            meta.ctime(),
            meta.ctime_nsec()
        )
    };
    #[cfg(not(unix))]
    let extra = String::new();
    format!("v1-{:x}-{mtime_ns:x}{extra}", meta.len())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The line ending a text uses (`crlf`, `lf`, `mixed`, or none for one line) and whether it starts with a BOM.
fn line_ending(text: &str) -> (Value, bool) {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    let ending = match (crlf, lf) {
        (0, 0) => Value::Null,
        (_, 0) => json!("crlf"),
        (0, _) => json!("lf"),
        _ => json!("mixed"),
    };
    (ending, text.starts_with('\u{feff}'))
}

/// Replace `path` with `bytes` atomically: a temporary file next to it, renamed over it (its permissions kept). Where
/// the rename is refused (Windows: the file is open elsewhere without delete sharing) it is written in place.
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let old = std::fs::metadata(path).ok();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.agentrouter-{}-{:x}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        if let Some(old) = &old {
            let _ = std::fs::set_permissions(&tmp, old.permissions());
        }
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        // A folder where a new file cannot be made next to it: write in place.
        return if old.is_some() {
            std::fs::write(path, bytes)
        } else {
            Err(e)
        };
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(path, bytes)
        }
    }
}

/// Bytes as utf8 text when they are, else base64.
fn inline(bytes: Vec<u8>) -> (String, &'static str) {
    match String::from_utf8(bytes) {
        Ok(text) => (text, "utf8"),
        Err(e) => (b64(e.as_bytes()), "base64"),
    }
}

impl Host {
    pub(super) fn read(&self, args: &Value) -> Result<Value, String> {
        let path = self.resolve(arg_str(args, "path").unwrap_or(""))?;
        let mut file =
            std::fs::File::open(&path).map_err(|e| format!("读不了 {}：{e}", shown(&path)))?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(size);
        let length = args
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(CHUNK_MAX)
            .min(size - offset)
            .min(CHUNK_MAX);
        let base64 = arg_str(args, "encoding") == Some("base64");
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0u8; length as usize];
        file.read_exact(&mut bytes)
            .map_err(|e| format!("读不了 {}：{e}", shown(&path)))?;
        let bytes_read = if base64 { bytes.clone() } else { Vec::new() };
        let data = if base64 {
            b64(&bytes)
        } else {
            match String::from_utf8(bytes) {
                Ok(text) => text,
                // A range may end inside a character: keep the whole characters.
                Err(e) if e.utf8_error().valid_up_to() + 4 > e.as_bytes().len() => {
                    let valid = e.utf8_error().valid_up_to();
                    let bytes = e.into_bytes();
                    String::from_utf8_lossy(&bytes[..valid]).into_owned()
                }
                Err(_) => {
                    return Err(format!(
                        "{} 不是 UTF-8 文本：用 encoding base64 读，或用传文件的工具",
                        shown(&path)
                    ));
                }
            }
        };
        let length = if base64 { length } else { data.len() as u64 };
        let meta = file.metadata().map_err(|e| e.to_string())?;
        let mut answer = json!({
            "path": shown(&path), "size": size, "offset": offset, "length": length,
            "eof": offset + length >= size, "encoding": if base64 { "base64" } else { "utf8" }, "data": data,
            "version": version(&meta), "mtime_ms": mtime_ms(&meta),
        });
        let whole = offset == 0 && length == size;
        if whole {
            answer["sha256"] = json!(sha256_hex(if base64 {
                &bytes_read
            } else {
                answer["data"].as_str().unwrap_or("").as_bytes()
            }));
            if !base64 {
                let (ending, bom) = line_ending(answer["data"].as_str().unwrap_or(""));
                answer["line_ending"] = ending;
                answer["bom"] = json!(bom);
            }
        } else if arg_bool(args, "sha256") {
            answer["sha256"] = json!(sha256_file(&path)?);
        }
        Ok(answer)
    }

    pub(super) fn write(&self, args: &Value) -> Result<Value, Fail> {
        let path = self.resolve(arg_str(args, "path").unwrap_or(""))?;
        let data = arg_str(args, "data").unwrap_or("");
        let bytes = if arg_str(args, "encoding") == Some("base64") {
            b64_decode(data).ok_or("data 不是有效的 base64")?
        } else {
            data.as_bytes().to_vec()
        };
        if bytes.len() as u64 > CHUNK_MAX {
            return Err(format!("一次最多写 {} 字节；大文件分块写（offset）", CHUNK_MAX).into());
        }
        let before = std::fs::metadata(&path).ok().filter(|m| m.is_file());
        if let Some(expected) = args.get("expected").filter(|e| e.is_object()) {
            check_expected(&path, before.as_ref(), expected)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建不了 {}：{e}", shown(parent)))?;
        }
        let fail = |e: std::io::Error| format!("写不了 {}：{e}", shown(&path));
        let offset = args.get("offset").and_then(Value::as_u64);
        match offset {
            None => replace(&path, &bytes).map_err(fail)?,
            Some(offset) => {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(&path)
                    .map_err(fail)?;
                if arg_bool(args, "truncate") {
                    file.set_len(offset).map_err(fail)?;
                }
                file.seek(SeekFrom::Start(offset)).map_err(fail)?;
                file.write_all(&bytes).map_err(fail)?;
            }
        }
        let meta = std::fs::metadata(&path).map_err(fail)?;
        let mut answer = json!({
            "path": shown(&path), "size": meta.len(), "written": bytes.len(), "created": before.is_none(),
            "version": version(&meta), "mtime_ms": mtime_ms(&meta),
        });
        if offset.is_none() {
            answer["sha256"] = json!(sha256_hex(&bytes));
        }
        Ok(answer)
    }

    /// Many paths' metadata in one round trip (a PC link is 30–200 ms per call). Per-path failures are entries,
    /// not a failed call.
    pub(super) fn stat(&self, args: &Value) -> Result<Value, String> {
        let paths = arg_paths(args, STAT_MAX)?;
        let content_max = args
            .get("content_max")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(STAT_CONTENT_MAX);
        let mut budget = STAT_CONTENT_TOTAL;
        let mut entries = Vec::with_capacity(paths.len());
        for raw in &paths {
            let real = match self.resolve(raw) {
                Ok(p) => p,
                Err(e) => {
                    entries.push(json!({"path": raw, "exists": false, "error": e}));
                    continue;
                }
            };
            // The path as named (not canonical) says whether it is a symlink.
            let named = self.absolute(raw);
            let symlink =
                std::fs::symlink_metadata(&named).is_ok_and(|m| m.file_type().is_symlink());
            let mut entry = match std::fs::metadata(&real) {
                Ok(meta) => json!({
                    "path": shown(&real), "exists": true, "kind": kind(meta.file_type()), "size": meta.len(),
                    "mtime_ms": mtime_ms(&meta), "readonly": meta.permissions().readonly(), "version": version(&meta),
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if symlink {
                        json!({"path": shown(&named), "exists": true, "kind": "symlink", "broken": true})
                    } else {
                        json!({"path": shown(&real), "exists": false})
                    }
                }
                Err(e) => json!({"path": shown(&real), "exists": false, "error": e.to_string()}),
            };
            if symlink {
                entry["symlink"] = json!(true);
            }
            let size = entry["size"].as_u64().unwrap_or(u64::MAX);
            if content_max > 0 && entry["kind"] == "file" && size <= content_max && size <= budget {
                match std::fs::read(&real) {
                    Ok(bytes) => {
                        budget -= bytes.len() as u64;
                        let (data, encoding) = inline(bytes);
                        entry["data"] = json!(data);
                        entry["encoding"] = json!(encoding);
                    }
                    Err(e) => entry["error"] = json!(e.to_string()),
                }
            }
            entries.push(entry);
        }
        Ok(json!({"entries": entries}))
    }

    /// Directory listings, sorted by name; with `depth` a pre-order walk (directories before their contents).
    pub(super) fn list(&self, args: &Value) -> Result<Value, String> {
        let paths = arg_paths(args, LIST_PATHS_MAX)?;
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, LIST_DEPTH_MAX) as usize;
        let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(true);
        let ignore = arg_bool(args, "ignore");
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .clamp(1, LIST_LIMIT_MAX) as usize;
        let mut listings = Vec::with_capacity(paths.len());
        for raw in &paths {
            let dir = match self.resolve(raw) {
                Ok(p) => p,
                Err(e) => {
                    listings.push(json!({"path": raw, "error": e}));
                    continue;
                }
            };
            listings.push(match list_one(&dir, depth, hidden, ignore, limit) {
                Ok(v) => v,
                Err(e) => json!({"path": shown(&dir), "error": e}),
            });
        }
        Ok(json!({"listings": listings}))
    }
}

fn list_one(
    dir: &Path,
    depth: usize,
    hidden: bool,
    ignore: bool,
    limit: usize,
) -> Result<Value, String> {
    let meta = std::fs::metadata(dir).map_err(|e| format!("读不了 {}：{e}", shown(dir)))?;
    if !meta.is_dir() {
        return Err(format!("{} 不是一个文件夹", shown(dir)));
    }
    // The walker from ripgrep (`ignore`): the same .gitignore/.ignore rules as `search` when asked.
    let walker = ignore::WalkBuilder::new(dir)
        .standard_filters(ignore)
        .hidden(!hidden)
        .parents(ignore)
        .follow_links(false)
        .max_depth(Some(depth))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in walker {
        let Ok(entry) = item else { continue };
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        let name = entry
            .path()
            .strip_prefix(dir)
            .map(shown)
            .unwrap_or_else(|_| shown(entry.path()));
        let mut e = json!({"name": name, "kind": entry.file_type().map(kind).unwrap_or("other")});
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                e["size"] = json!(meta.len());
            }
            e["mtime_ms"] = mtime_ms(&meta);
        }
        entries.push(e);
    }
    Ok(json!({"path": shown(dir), "entries": entries, "truncated": truncated}))
}

/// A guarded write's precondition: still absent, or still the version (and content) the caller read.
fn check_expected(
    path: &Path,
    before: Option<&std::fs::Metadata>,
    expected: &Value,
) -> Result<(), Fail> {
    let current = before.map(version);
    if expected.get("absent").and_then(Value::as_bool) == Some(true) {
        return match current {
            None => Ok(()),
            Some(v) => Err(Fail::coded(
                "EXISTS",
                format!("{} 已经存在：先读它，再按版本改", shown(path)),
                json!({"version": v}),
            )),
        };
    }
    let Some(current) = current else {
        return Err(Fail::coded(
            "STALE_VERSION",
            format!("{} 已经不在了", shown(path)),
            json!({"version": null}),
        ));
    };
    let stale = |why: &str| {
        Fail::coded(
            "STALE_VERSION",
            format!("{} 在读过之后{why}：重新读，再改", shown(path)),
            json!({"version": current}),
        )
    };
    if let Some(want) = expected.get("version").and_then(Value::as_str)
        && want != current
    {
        return Err(stale("被改过了"));
    }
    if let Some(want) = expected.get("sha256").and_then(Value::as_str)
        && !want.eq_ignore_ascii_case(&sha256_file(path)?)
    {
        return Err(stale("内容变了"));
    }
    Ok(())
}

pub(super) fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};
    use super::*;

    #[test]
    fn chunked_write_then_read_back_with_the_same_hash() {
        let dir = temp("chunks");
        let host = host_in(&dir);
        let first = host
            .write(&json!({"path": "out/a.bin", "data": b64(&[1, 2, 3]), "encoding": "base64", "offset": 0, "truncate": true}))
            .unwrap();
        assert_eq!(first["path"], json!(shown(&dir.join("out").join("a.bin"))));
        host.write(
            &json!({"path": "out/a.bin", "data": b64(&[4, 5]), "encoding": "base64", "offset": 3}),
        )
        .unwrap();
        let back = host
            .read(&json!({"path": dir.join("out/a.bin").to_string_lossy(), "encoding": "base64", "sha256": true}))
            .unwrap();
        assert_eq!(back["size"], json!(5));
        assert_eq!(
            b64_decode(back["data"].as_str().unwrap()).unwrap(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(back["eof"], json!(true));
        assert_eq!(back["sha256"].as_str().unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_carry_the_version_and_guarded_writes_respect_it() {
        let dir = temp("guard");
        let host = host_in(&dir);
        std::fs::write(dir.join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let read = host.read(&json!({"path": "a.txt"})).unwrap();
        let v1 = read["version"].as_str().unwrap().to_string();
        assert_eq!(read["line_ending"], "crlf");
        assert_eq!(read["bom"], false);
        assert_eq!(
            read["sha256"].as_str().unwrap(),
            sha256_hex(b"one\r\ntwo\r\n")
        );
        let stat = host.stat(&json!({"paths": ["a.txt"]})).unwrap();
        assert_eq!(stat["entries"][0]["version"], v1.as_str());
        // The version read: the write goes through and answers the new version.
        let w = host
            .write(&json!({"path": "a.txt", "data": "one\r\n", "expected": {"version": v1}}))
            .unwrap();
        assert_eq!(w["created"], false);
        let v2 = w["version"].as_str().unwrap().to_string();
        assert_ne!(v1, v2);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\r\n"
        );
        // The old version again: refused with the current one, nothing written.
        let stale = host
            .write(&json!({"path": "a.txt", "data": "x", "expected": {"version": v1}}))
            .unwrap_err();
        assert_eq!(stale.code, Some("STALE_VERSION"));
        assert_eq!(stale.data["version"], v2.as_str());
        let wrong_hash = host
            .write(
                &json!({"path": "a.txt", "data": "x", "expected": {"version": v2, "sha256": "00"}}),
            )
            .unwrap_err();
        assert_eq!(wrong_hash.code, Some("STALE_VERSION"));
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\r\n"
        );
        // Create only if absent.
        let exists = host
            .write(&json!({"path": "a.txt", "data": "x", "expected": {"absent": true}}))
            .unwrap_err();
        assert_eq!(exists.code, Some("EXISTS"));
        let created = host
            .write(&json!({"path": "new/b.txt", "data": "b", "expected": {"absent": true}}))
            .unwrap();
        assert_eq!(created["created"], true);
        let gone = host
            .write(&json!({"path": "missing.txt", "data": "x", "expected": {"version": v2}}))
            .unwrap_err();
        assert_eq!(gone.code, Some("STALE_VERSION"));
        // The atomic replace leaves no temporary file behind.
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stat_answers_many_paths_with_small_contents_inline() {
        let dir = temp("stat");
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 159, 146, 150]).unwrap();
        std::fs::write(dir.join("big.txt"), "x".repeat(100)).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let host = host_in(&dir);
        let out = host
            .stat(&json!({"paths": ["a.txt", "b.bin", "big.txt", "sub", "missing.txt"], "content_max": 50}))
            .unwrap();
        let e = out["entries"].as_array().unwrap();
        assert_eq!(e.len(), 5);
        assert_eq!(e[0]["kind"], "file");
        assert_eq!(e[0]["size"], 5);
        assert_eq!(e[0]["data"], "hello");
        assert_eq!(e[0]["encoding"], "utf8");
        assert!(e[0]["mtime_ms"].as_i64().unwrap() > 0);
        assert_eq!(e[1]["encoding"], "base64");
        assert!(e[2].get("data").is_none(), "over content_max");
        assert_eq!(e[3]["kind"], "dir");
        assert_eq!(e[4]["exists"], false);
        assert!(e[4].get("error").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_sorted_walks_to_a_depth_and_can_apply_gitignore() {
        let dir = temp("list");
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::write(dir.join("src/b.rs"), "b").unwrap();
        std::fs::write(dir.join("src/a.rs"), "a").unwrap();
        std::fs::write(dir.join("src/deep/c.rs"), "c").unwrap();
        std::fs::write(dir.join(".env"), "x").unwrap();
        std::fs::write(dir.join(".gitignore"), "out/\n").unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/gen.js"), "g").unwrap();
        let host = host_in(&dir);
        let names = |v: &Value| -> Vec<String> {
            v["listings"][0]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().replace('\\', "/"))
                .collect()
        };
        let top = host.list(&json!({"path": "."})).unwrap();
        assert_eq!(names(&top), [".env", ".gitignore", "out", "src"]);
        let deep = host
            .list(&json!({"path": ".", "depth": 3, "hidden": false}))
            .unwrap();
        assert_eq!(
            names(&deep),
            [
                "out",
                "out/gen.js",
                "src",
                "src/a.rs",
                "src/b.rs",
                "src/deep",
                "src/deep/c.rs"
            ]
        );
        // .gitignore applies only when asked, and only inside a git repository (ripgrep's rule).
        std::fs::create_dir(dir.join(".git")).unwrap();
        let ignored = host
            .list(&json!({"path": ".", "depth": 2, "hidden": false, "ignore": true}))
            .unwrap();
        assert_eq!(names(&ignored), ["src", "src/a.rs", "src/b.rs", "src/deep"]);
        let limited = host
            .list(&json!({"paths": ["src", "nope"], "limit": 1}))
            .unwrap();
        assert_eq!(limited["listings"][0]["truncated"], true);
        assert!(limited["listings"][1]["error"].is_string());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
