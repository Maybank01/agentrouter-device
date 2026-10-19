//! The local audit log: append-only JSON lines, each carrying the hash of the line before it, so an
//! edited or deleted line shows up on `agentrouter audit verify`. It records who (which conversation),
//! when, what — the request's **arguments** ([`redact_args`]: secrets blanked, long strings stored as a
//! hash) — how long it took and the outcome with a digest of the result ([`result_summary`]).
//! The chain head (`seq`, `hash`, [`head`]) goes to the cloud with every heartbeat, so a log rewritten
//! on the PC no longer matches what the platform last saw (docs/LINKER-MCP.md §7.1, #44).

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::protocol::{canonical_json, sha256_hex};
use crate::util::iso_now;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Strings longer than this are stored as sha256 + length + the first [`HEAD_BYTES`] bytes.
const LONG_STRING: usize = 8 * 1024;
const HEAD_BYTES: usize = 512;
pub const SECRET: &str = "[secret]";

pub struct Audit {
    path: PathBuf,
    last: String,
    seq: u64,
}

impl Audit {
    pub fn open(path: &Path) -> Audit {
        let (seq, last) = tail(path);
        Audit {
            path: path.to_path_buf(),
            last,
            seq,
        }
    }

    /// The chain head as the heartbeat reports it: `{seq, hash}` (`seq` = entries so far).
    pub fn head(&self) -> Value {
        json!({"seq": self.seq, "hash": self.last})
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry (an object); `at`, `prev` and `hash` are added here. The chain is read and
    /// written under a cross-process lock, so the connector and a local Agent's MCP service can both
    /// append without breaking it.
    pub fn record(&mut self, entry: Value) {
        let chain_lock = match crate::lockfile::acquire(
            &self.path.with_extension("lock"),
            Duration::from_secs(5),
        ) {
            Ok(lock) => Some(lock),
            Err(e) => {
                crate::util::log(&format!(
                    "audit lock unavailable, appending anyway: {} {}",
                    e.code, e.message
                ));
                None
            }
        };
        // Another process may have appended since this handle was opened.
        (self.seq, self.last) = tail(&self.path);
        let mut map = match entry {
            Value::Object(map) => map,
            other => {
                let mut m = Map::new();
                m.insert("value".into(), other);
                m
            }
        };
        map.insert("at".into(), json!(iso_now()));
        map.insert("prev".into(), json!(self.last));
        let hash = sha256_hex(canonical_json(&Value::Object(map.clone())).as_bytes());
        map.insert("hash".into(), json!(hash));
        let line = format!("{}\n", Value::Object(map));
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(mut f) => {
                if f.write_all(line.as_bytes()).is_ok() {
                    self.last = hash;
                    self.seq += 1;
                }
            }
            Err(e) => crate::util::log(&format!("audit log not writable: {e}")),
        }
        drop(chain_lock);
    }
}

/// Append one entry with a fresh handle (a local Agent MCP service keeps no long-lived chain).
pub fn append(path: &Path, entry: Value) {
    Audit::open(path).record(entry);
}

/// The chain head of a log file on disk: `{seq, hash}` (`seq` 0 and the genesis hash when empty).
pub fn head(path: &Path) -> Value {
    let (seq, hash) = tail(path);
    json!({"seq": seq, "hash": hash})
}

/// (entries, hash of the last entry).
fn tail(path: &Path) -> (u64, String) {
    let mut seq = 0;
    let mut last = GENESIS.to_string();
    let Ok(file) = std::fs::File::open(path) else {
        return (seq, last);
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some(h) = v.get("hash").and_then(Value::as_str)
        {
            last = h.to_string();
            seq += 1;
        }
    }
    (seq, last)
}

/// The last `n` entries, oldest first (for `agentrouter audit`).
pub fn last_entries(path: &Path, n: usize) -> Vec<Value> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut out = std::collections::VecDeque::with_capacity(n);
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if out.len() == n {
                out.pop_front();
            }
            out.push_back(v);
        }
    }
    out.into_iter().collect()
}

/// A field whose value is a secret by its name (`apiKey`, `GITHUB_TOKEN`, `password`, `Authorization` …).
fn secret_name(name: &str) -> bool {
    let n = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    n == "key"
        || n == "auth"
        || (n.ends_with("key") && !n.ends_with("monkey") && n != "hotkey")
        || [
            "secret",
            "token",
            "password",
            "passwd",
            "passphrase",
            "authorization",
            "cookie",
            "credential",
            "bearer",
            "signature",
        ]
        .iter()
        .any(|s| n.contains(s))
}

/// A request's arguments as the audit stores them: every value under a secret name and every value of
/// an `env` object replaced by `[secret]`, strings over 8 KiB replaced by `{sha256, len, head}` (the
/// first 512 bytes). Written canonically by [`Audit::record`].
pub fn redact_args(args: &Value) -> Value {
    redact(args, false)
}

fn redact(value: &Value, secret: bool) -> Value {
    if secret && !value.is_null() {
        return json!(SECRET);
    }
    match value {
        Value::String(s) if s.len() > LONG_STRING => {
            let mut end = HEAD_BYTES;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            json!({"sha256": sha256_hex(s.as_bytes()), "len": s.len(), "head": &s[..end]})
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| redact(v, false)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let hidden = if k.eq_ignore_ascii_case("env") {
                        // Environment: names stay, values never.
                        match v {
                            Value::Object(env) => Value::Object(
                                env.keys()
                                    .map(|name| (name.clone(), json!(SECRET)))
                                    .collect(),
                            ),
                            other => redact(other, true),
                        }
                    } else {
                        redact(v, secret_name(k))
                    };
                    (k.clone(), hidden)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// What came back, without the bytes: size and sha256 of the canonical result, `isError`, and the
/// MCP content items' types and sizes.
pub fn result_summary(result: &Value) -> Value {
    let canonical = canonical_json(result);
    let mut out = json!({"bytes": canonical.len(), "sha256": sha256_hex(canonical.as_bytes())});
    if let Some(is_error) = result.get("isError").and_then(Value::as_bool) {
        out["isError"] = json!(is_error);
    }
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        out["content"] = Value::Array(
            items
                .iter()
                .map(|item| {
                    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                    let size = ["text", "data", "blob"]
                        .iter()
                        .find_map(|k| item.get(*k).and_then(Value::as_str))
                        .map(str::len)
                        .unwrap_or(0);
                    json!({"type": kind, "size": size})
                })
                .collect(),
        );
    }
    out
}

/// Check the chain: Ok(number of entries) or Err(the first broken line, 1-based, and why).
pub fn verify(path: &Path) -> Result<usize, (usize, String)> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(0);
    };
    let mut prev = GENESIS.to_string();
    let mut count = 0;
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| (i + 1, e.to_string()))?;
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&line) else {
            return Err((i + 1, "not a JSON object".into()));
        };
        let Some(Value::String(hash)) = map.remove("hash") else {
            return Err((i + 1, "no hash".into()));
        };
        if map.get("prev").and_then(Value::as_str) != Some(prev.as_str()) {
            return Err((i + 1, "does not follow the line before it".into()));
        }
        if sha256_hex(canonical_json(&Value::Object(map)).as_bytes()) != hash {
            return Err((i + 1, "was changed after it was written".into()));
        }
        prev = hash;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_detects_edits() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ar-audit-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        let mut audit = Audit::open(&path);
        audit.record(json!({"session": "ags_1", "action": "exec", "text": "dir"}));
        audit.record(json!({"session": "ags_1", "action": "read_file", "text": "C:\\x"}));
        // Reopening continues the chain.
        let mut again = Audit::open(&path);
        again.record(json!({"session": "ags_2", "action": "info"}));
        assert_eq!(verify(&path), Ok(3));
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("dir", "del");
        std::fs::write(&path, text).unwrap();
        assert_eq!(verify(&path).unwrap_err().0, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn head_follows_the_chain() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ar-audit-head-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        assert_eq!(head(&path), json!({"seq": 0, "hash": GENESIS}));
        let mut audit = Audit::open(&path);
        audit.record(json!({"action": "a"}));
        audit.record(json!({"action": "b"}));
        assert_eq!(audit.head()["seq"], 2);
        assert_eq!(head(&path), audit.head());
        let last = last_entries(&path, 1);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0]["action"], "b");
        assert_eq!(audit.head()["hash"], last[0]["hash"]);
        // Another handle appending moves the head on disk.
        Audit::open(&path).record(json!({"action": "c"}));
        assert_eq!(head(&path)["seq"], 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn arguments_are_kept_but_secrets_and_bulk_are_not() {
        let long = "é".repeat(5000);
        let args = json!({
            "name": "github",
            "command": "npx",
            "env": {"GITHUB_TOKEN": "ghp_123", "MODE": "fast"},
            "arguments": {"path": "src/a.rs", "apiKey": "sk-1", "Authorization": "Bearer x",
                "nested": [{"password": "pw-9", "keep": "yes"}], "content": long, "monkey": "ok"},
        });
        let out = redact_args(&args);
        assert_eq!(out["command"], "npx");
        assert_eq!(out["env"], json!({"GITHUB_TOKEN": SECRET, "MODE": SECRET}));
        assert_eq!(out["arguments"]["path"], "src/a.rs");
        assert_eq!(out["arguments"]["apiKey"], SECRET);
        assert_eq!(out["arguments"]["Authorization"], SECRET);
        assert_eq!(out["arguments"]["nested"][0]["password"], SECRET);
        assert_eq!(out["arguments"]["nested"][0]["keep"], "yes");
        assert_eq!(out["arguments"]["monkey"], "ok");
        let content = &out["arguments"]["content"];
        assert_eq!(content["len"], long.len());
        assert_eq!(content["sha256"], sha256_hex(long.as_bytes()));
        assert!(content["head"].as_str().unwrap().len() <= HEAD_BYTES);
        let text = out.to_string();
        for secret in ["ghp_123", "sk-1", "Bearer x", "pw-9"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn result_summary_has_no_bytes() {
        let result = json!({"isError": false, "content": [{"type": "text", "text": "hello"}, {"type": "image", "data": "AAAA"}]});
        let summary = result_summary(&result);
        assert_eq!(summary["isError"], false);
        assert_eq!(
            summary["content"],
            json!([{"type": "text", "size": 5}, {"type": "image", "size": 4}])
        );
        assert!(!summary.to_string().contains("hello"));
    }

    #[test]
    fn concurrent_appends_keep_one_chain() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("ar-audit-concurrent-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.log");
        // Two independent handles (like the connector and a local Agent MCP service) appending at once.
        let mut handles = Vec::new();
        for worker in 0..2 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut audit = Audit::open(&path);
                for i in 0..10 {
                    audit.record(json!({"action": "test", "worker": worker, "i": i}));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(verify(&path), Ok(20));
        let _ = std::fs::remove_dir_all(dir);
    }
}
