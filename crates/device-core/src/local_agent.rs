//! Local Agent identity and first-seen registration (owner direction 2026-10-18: a local MCP entry
//! for Agents on this machine, with `lag_`/`lses_` ids).
//!
//! The identity is self-reported by the launching Agent through the environment
//! (`AGENTROUTER_LOCAL_AGENT`, `AGENTROUTER_LOCAL_SESSION`) — there is no token yet: we rely on the
//! same OS user + the Agent's claim, and record the limitation. First sight registers the pair in
//! `local-agents.json` in the device data folder; `agentrouter agent list` shows them. A later slice
//! adds revocation/tokens.
//!
//! The ids are derived deterministically so the same Agent/session keeps the same worktree and
//! deploy records: `lag_<slug(agent)>` / `lses_<slug(session)>`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::protocol::DeviceError;
use crate::util::iso_now;

pub const FILE: &str = "local-agents.json";
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `lag_<slug>`: the local Agent (name/type).
    pub agent_id: String,
    /// The reported name/type (not a secret; shown in the audit/consent text).
    pub agent_name: String,
    /// `lses_<slug>`: one conversation/session of that Agent (isolation unit).
    pub session_id: String,
}

impl Identity {
    pub fn from_env() -> Identity {
        Self::from_raw(
            std::env::var("AGENTROUTER_LOCAL_AGENT").ok().as_deref(),
            std::env::var("AGENTROUTER_LOCAL_SESSION").ok().as_deref(),
        )
    }

    /// Parse the two environment values. Missing/blank values fall back to `local`/`default`, so a
    /// locally launched MCP service without any environment still works (compatibility).
    pub fn from_raw(agent: Option<&str>, session: Option<&str>) -> Identity {
        let agent_name = agent
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("local")
            .to_string();
        let agent_id = format!("lag_{}", slug(&agent_name));
        let session = session
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("default");
        Identity {
            agent_id,
            agent_name,
            session_id: format!("lses_{}", slug(session)),
        }
    }

    pub fn caller(&self) -> Value {
        json!({
            "agent": self.agent_id,
            "name": self.agent_name,
            "session": self.session_id,
        })
    }
}

/// A stable id component: lowercase ASCII letters/digits, other runs collapsed to `-`, 40 chars max.
pub fn slug(raw: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn registry_path(data: &Path) -> PathBuf {
    data.join(FILE)
}

fn load(data: &Path) -> Value {
    std::fs::read(registry_path(data))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"schemaVersion": SCHEMA, "agents": []}))
}

fn save(data: &Path, value: &Value) -> Result<(), DeviceError> {
    std::fs::create_dir_all(data).map_err(|e| {
        DeviceError::new(
            "LOCAL_AGENT_REGISTRY_FAILED",
            format!("建不了设备数据目录：{e}"),
        )
    })?;
    let path = registry_path(data);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| DeviceError::new("LOCAL_AGENT_REGISTRY_FAILED", format!("写不出来：{e}")))?;
    std::fs::write(&tmp, bytes)
        .map_err(|e| DeviceError::new("LOCAL_AGENT_REGISTRY_FAILED", format!("写入失败：{e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| DeviceError::new("LOCAL_AGENT_REGISTRY_FAILED", format!("换名失败：{e}")))
}

/// First-seen registration: the agent and its session appear once, timestamps updated after that.
pub fn register(data: &Path, identity: &Identity) -> Result<(), DeviceError> {
    let mut registry = load(data);
    let now = iso_now();
    if !registry["agents"].is_array() {
        registry["agents"] = json!([]);
    }
    let agents = registry["agents"]
        .as_array_mut()
        .expect("agents is an array");
    match agents
        .iter_mut()
        .find(|agent| agent.get("id").and_then(Value::as_str) == Some(identity.agent_id.as_str()))
    {
        Some(agent) => {
            agent["name"] = json!(identity.agent_name);
            agent["lastSeenAt"] = json!(now);
            let sessions = agent
                .get_mut("sessions")
                .and_then(Value::as_array_mut)
                .map(|sessions| {
                    sessions
                        .iter_mut()
                        .find(|session| {
                            session.get("id").and_then(Value::as_str)
                                == Some(identity.session_id.as_str())
                        })
                        .map(|session| session["lastSeenAt"] = json!(now))
                        .is_some()
                })
                .unwrap_or(false);
            if !sessions && let Some(sessions) = agent["sessions"].as_array_mut() {
                sessions.push(json!({
                    "id": identity.session_id,
                    "createdAt": now,
                    "lastSeenAt": now,
                }));
            }
        }
        None => agents.push(json!({
            "id": identity.agent_id,
            "name": identity.agent_name,
            "createdAt": now,
            "lastSeenAt": now,
            "sessions": [{"id": identity.session_id, "createdAt": now, "lastSeenAt": now}],
        })),
    }
    registry["schemaVersion"] = json!(SCHEMA);
    save(data, &registry)
}

/// What `agentrouter agent list` shows: the registered local agents and their sessions.
pub fn list(data: &Path) -> Result<Value, DeviceError> {
    Ok(load(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("target");
        let dir = target
            .join("tmp")
            .join(format!("local-agent-{tag}-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_is_deterministic_and_normalised() {
        let first = Identity::from_raw(Some("DSH"), Some("abc/123"));
        assert_eq!(first.agent_id, "lag_dsh");
        assert_eq!(first.agent_name, "DSH");
        assert_eq!(first.session_id, "lses_abc-123");
        let again = Identity::from_raw(Some("dsh"), Some("abc/123"));
        assert_eq!(first.agent_id, again.agent_id);
        assert_eq!(first.session_id, again.session_id);
    }

    #[test]
    fn missing_environment_still_works() {
        let none = Identity::from_raw(None, None);
        assert_eq!(none.agent_id, "lag_local");
        assert_eq!(none.session_id, "lses_default");
        let blank = Identity::from_raw(Some("  "), Some(""));
        assert_eq!(blank.agent_id, "lag_local");
        assert_eq!(blank.session_id, "lses_default");
        assert_eq!(slug("!!!"), "unknown");
    }

    #[test]
    fn registration_is_first_seen_and_deduplicated() {
        let data = tmp("registry");
        let dsh = Identity::from_raw(Some("dsh"), Some("s1"));
        let other = Identity::from_raw(Some("dsh"), Some("s2"));
        register(&data, &dsh).unwrap();
        register(&data, &dsh).unwrap();
        register(&data, &other).unwrap();
        let listed = list(&data).unwrap();
        let agents = listed["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1, "{listed}");
        assert_eq!(agents[0]["id"], "lag_dsh");
        let sessions = agents[0]["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2, "{listed}");
        assert!(sessions.iter().any(|s| s["id"] == "lses_s1"));
        assert!(sessions.iter().any(|s| s["id"] == "lses_s2"));
        // A second Agent is another entry.
        register(&data, &Identity::from_raw(Some("codex"), Some("s3"))).unwrap();
        assert_eq!(list(&data).unwrap()["agents"].as_array().unwrap().len(), 2);
    }
}
