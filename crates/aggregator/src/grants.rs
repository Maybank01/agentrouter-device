//! What the person allowed a local tool on this computer (device #97, Cloud #2396; docs/HOSTD-MCP.md §6.7), and the
//! tools they added themselves (`agentrouter mcp add`).
//!
//! - **Grants.** A tool that needs the person's yes (a custom command, or one asking for the display) is granted when
//!   they say yes on this computer, or when they add it themselves. The grant is recorded here, in the device data
//!   folder (no sandbox reads or writes it), keyed by the tool's name and the hash of its definition: the same
//!   definition is not asked about again; a changed one (another command, other arguments, other environment, other
//!   network hosts, display) asks again. The record carries what was granted (the display, the network hosts), so the
//!   person's yes covers exactly what they were shown. The cloud can ask for a grant, never make one.
//! - **The person's own tools** (`agentrouter mcp add`): their definitions, started by every `agentrouter link` on this
//!   computer like the project's `.agentrouter/mcp.json`; nothing is deleted except by `agentrouter mcp remove`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::servers::ServerDef;

const GRANTS: &str = "grants.json";
const OWN: &str = "tools.json";
/// At most this many tools of the person's own (as many as a link runs).
pub const OWN_MAX: usize = crate::servers::MAX_SERVERS;

/// The hash a grant is keyed by: everything that decides what the tool does and may do. Environment values are
/// hashed, never stored.
pub fn digest(def: &ServerDef) -> String {
    let canonical = json!({
        "command": def.command,
        "args": def.args,
        "env": def.env,
        "network": def.network,
        "display": def.display,
        "pin": def.pin.as_ref().map(|p| p.digest.clone()),
        "preset": def.preset,
    });
    let mut h = Sha256::new();
    h.update(canonical.to_string().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn read(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Written whole through a temporary file beside it, then renamed: a crash never leaves half a file.
fn write(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}：{e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("{}：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}：{e}", path.display()))
}

fn grants_path(data: &Path) -> PathBuf {
    data.join(GRANTS)
}

/// Whether the person granted exactly this definition.
pub fn granted(data: &Path, def: &ServerDef) -> bool {
    read(&grants_path(data))
        .get(&def.name)
        .and_then(|g| g.get("sha256"))
        .and_then(Value::as_str)
        == Some(digest(def).as_str())
}

/// Record the person's yes for this definition (replacing an older grant of the same name).
pub fn record(data: &Path, def: &ServerDef) -> Result<(), String> {
    let path = grants_path(data);
    let mut all = read(&path);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    all.insert(
        def.name.clone(),
        json!({
            "sha256": digest(def),
            "command": def.shown(),
            "display": def.display,
            "network": def.network,
            "at": at,
        }),
    );
    write(&path, &Value::Object(all))
}

/// Forget a tool's grant (`agentrouter mcp remove`): the next start asks again.
pub fn forget(data: &Path, name: &str) -> Result<bool, String> {
    let path = grants_path(data);
    let mut all = read(&path);
    let had = all.remove(name).is_some();
    if had {
        write(&path, &Value::Object(all))?;
    }
    Ok(had)
}

/// The person's own tools, as `link__server_add` arguments (name, title, command, args, network, display).
pub fn own(data: &Path) -> Vec<Value> {
    read(&data.join(OWN))
        .get("servers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Add (or replace, by name) one of the person's own tools.
pub fn own_add(data: &Path, def: &ServerDef) -> Result<(), String> {
    let mut list: Vec<Value> = own(data)
        .into_iter()
        .filter(|s| s.get("name").and_then(Value::as_str) != Some(def.name.as_str()))
        .collect();
    if list.len() >= OWN_MAX {
        return Err(format!(
            "最多 {OWN_MAX} 个自己加的本机工具；先用 agentrouter mcp remove 去掉不用的"
        ));
    }
    let mut entry = json!({
        "name": def.name, "title": def.title, "command": def.command, "args": def.args, "network": def.network,
    });
    if def.display {
        entry["display"] = json!(true);
    }
    list.push(entry);
    write(&data.join(OWN), &json!({"servers": list}))
}

/// Remove one of the person's own tools and its grant. `false`: there was none of that name.
pub fn own_remove(data: &Path, name: &str) -> Result<bool, String> {
    let list = own(data);
    let kept: Vec<Value> = list
        .iter()
        .filter(|s| s.get("name").and_then(Value::as_str) != Some(name))
        .cloned()
        .collect();
    let had = kept.len() != list.len();
    if had {
        write(&data.join(OWN), &json!({"servers": kept}))?;
    }
    Ok(forget(data, name)? || had)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentrouter-grants-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn def(args: Value) -> ServerDef {
        ServerDef::parse(&args).unwrap()
    }

    #[test]
    fn a_grant_covers_exactly_the_definition_it_was_given_for() {
        let data = temp("exact");
        let screen = def(
            json!({"name": "screen", "command": "uvx", "args": ["windows-mcp"], "display": true}),
        );
        assert!(screen.display && screen.needs_yes());
        assert!(!granted(&data, &screen));
        record(&data, &screen).unwrap();
        assert!(granted(&data, &screen));
        // Another argument, the display dropped or added, other network hosts: asked again.
        for changed in [
            json!({"name": "screen", "command": "uvx", "args": ["windows-mcp", "--x"], "display": true}),
            json!({"name": "screen", "command": "uvx", "args": ["windows-mcp"]}),
            json!({"name": "screen", "command": "uvx", "args": ["windows-mcp"], "display": true, "network": ["example.com"]}),
            json!({"name": "screen", "command": "uvx", "args": ["windows-mcp"], "display": true, "env": {"A": "1"}}),
        ] {
            assert!(!granted(&data, &def(changed.clone())), "{changed}");
        }
        // The record names what was granted, never an environment value.
        let secret = def(
            json!({"name": "api", "command": "node", "args": ["s.js"], "env": {"TOKEN": "s3cret-value"}}),
        );
        record(&data, &secret).unwrap();
        let text = std::fs::read_to_string(data.join(GRANTS)).unwrap();
        assert!(!text.contains("s3cret-value"), "{text}");
        assert!(text.contains("\"display\": true"), "{text}");
        assert!(forget(&data, "screen").unwrap());
        assert!(!granted(&data, &screen));
        assert!(granted(&data, &secret));
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn the_persons_own_tools_are_kept_until_they_remove_them() {
        let data = temp("own");
        assert!(own(&data).is_empty());
        let screen = def(
            json!({"name": "screen", "title": "屏幕", "command": "uvx", "args": ["windows-mcp"], "display": true}),
        );
        own_add(&data, &screen).unwrap();
        record(&data, &screen).unwrap();
        // Replaced by name, not duplicated.
        own_add(&data, &screen).unwrap();
        let list = own(&data);
        assert_eq!(list.len(), 1);
        // What `link` starts from it is the same definition, so the grant holds.
        let again = ServerDef::parse(&list[0]).unwrap();
        assert_eq!(again, screen);
        assert!(granted(&data, &again));
        assert!(own_remove(&data, "screen").unwrap());
        assert!(own(&data).is_empty() && !granted(&data, &screen));
        assert!(!own_remove(&data, "screen").unwrap());
        let _ = std::fs::remove_dir_all(&data);
    }
}
