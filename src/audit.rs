//! The local audit log: append-only JSON lines, each carrying the hash of the line before it, so an
//! edited or deleted line shows up on `agentrouter-device audit verify`. It records who (which
//! conversation), when, what and the outcome; file contents are never written to it.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::protocol::{canonical_json, sha256_hex};
use crate::util::iso_now;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

pub struct Audit {
    path: PathBuf,
    last: String,
}

impl Audit {
    pub fn open(path: &Path) -> Audit {
        let last = last_hash(path).unwrap_or_else(|| GENESIS.to_string());
        Audit {
            path: path.to_path_buf(),
            last,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry (an object); `at`, `prev` and `hash` are added here.
    pub fn record(&mut self, entry: Value) {
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
                }
            }
            Err(e) => crate::util::log(&format!("audit log not writable: {e}")),
        }
    }
}

fn last_hash(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some(h) = v.get("hash").and_then(Value::as_str)
        {
            last = Some(h.to_string());
        }
    }
    last
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
        let dir = std::env::temp_dir().join(format!("ar-audit-{}", crate::util::short_id("")));
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
}
