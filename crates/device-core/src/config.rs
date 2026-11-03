//! What the person chose on this device (gateway, name, start folder, labels, limits, software, local data). Not secret:
//! the device key and the pinned control key live in the key store (DPAPI on Windows).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::data_dir;

/// Where devices connect. MVP preview: the Dev gateway (production enables devices later). Every gateway name is
/// served through the Gate (the mainland relay in front of the origin, Cloud #3423): the device needs nothing but the
/// name, and `agent-gateway-dev.agentrouter.page` reaches the same gateway.
pub const DEFAULT_GATEWAY: &str = "https://agent-gateway-dev.agentrouter.top";
/// The gateway for one run, above config.json (a cloud host's start, a test, a person trying the other name).
pub const ENV_GATEWAY: &str = "AGENTROUTER_GATEWAY";

/// The gateway a run uses and where it came from, one rule for every command (Cloud #3423): `--gateway`, else
/// [`ENV_GATEWAY`], else config.json's `gateway` (what `link --gateway` saved), else [`DEFAULT_GATEWAY`]. A blank value
/// counts as unset.
pub fn gateway_for(flag: Option<&str>, env: Option<&str>, saved: &str) -> (String, &'static str) {
    let set = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    if let Some(g) = set(flag) {
        return (g, "--gateway");
    }
    if let Some(g) = set(env) {
        return (g, ENV_GATEWAY);
    }
    match set(Some(saved)) {
        Some(g) if g != DEFAULT_GATEWAY => (g, "config.json"),
        _ => (DEFAULT_GATEWAY.to_string(), "默认"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub gateway: String,
    pub name: String,
    /// The folder the double-click start links (chosen once in the folder picker, then reused; `start.rs`).
    pub start_folder: Option<String>,
    /// The owner's labels for this host (`gpu`, `rack-2`, 「我的渲染机」), reported with its capacity (`capacity.rs`).
    pub labels: Vec<String>,
    /// How many environments (Agents' workplaces) this host takes at once; unset = from cores and memory.
    pub max_environments: Option<u32>,
    /// The pool the owner puts this host in (「渲染机」, `rack-2`; Cloud VISION §4 D2): a definition may admit it as
    /// `pool:<name>`. Unset = the person's default pool. 1–40 characters, no control characters (else not sent).
    pub pool: Option<String>,
    /// The owner's ceiling on what an environment here may use (`{"memory_mb", "processes", "cpu_percent"}`, each
    /// optional; device #127): the platform's limits never go above it. Unset = only the platform's.
    pub limits: crate::limits::Limits,
    /// Installed-software discovery (Cloud DEVICES §2.3): per-source switches, hidden ids and the person's own entries
    /// (`agentrouter software add|hide`). Unset = every source on.
    pub software: crate::software::SoftwareConfig,
    /// Named local data (Cloud DEVICES §2.5): folders the person registered once with a name (`agentrouter data add`),
    /// reported as `link.localData` and, in folder mode, inside the link's grants.
    pub local_data: Vec<crate::local_data::LocalData>,
    // `rules` (the former recipe lists, `agentrouter rules set`) is gone (Cloud #2836): which Agent may use this computer
    // is the grant of its device space. A config.json an older version saved with `rules` still reads (serde skips it)
    // until the minimum device version moves past it.
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gateway: DEFAULT_GATEWAY.to_string(),
            name: crate::util::host_name(),
            start_folder: None,
            labels: Vec::new(),
            max_environments: None,
            pool: None,
            limits: Default::default(),
            software: Default::default(),
            local_data: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.json")
    }

    /// [`Config::pool`] when it is a usable name (trimmed, 1–40 characters, no control characters).
    pub fn pool_name(&self) -> Option<String> {
        let p = self.pool.as_deref()?.trim();
        (!p.is_empty() && p.chars().count() <= 40 && !p.chars().any(char::is_control))
            .then(|| p.to_string())
    }

    pub fn load() -> Config {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Config {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// The file as it is, or why it does not read (a missing file is the defaults). A running link applies rules and
    /// local data only from a file that reads: a hand edit that breaks it never widens what this computer allows.
    pub fn try_load() -> Result<Config, String> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("{} 读不懂：{e}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{} 读不了：{e}", path.display())),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_an_older_version_saved_with_recipe_lists_still_reads() {
        let text = r#"{"name":"书房","labels":["game"],"rules":{"agents":["w_me/agents/剪辑"],"local_data":{"素材":[]}}}"#;
        let cfg: Config = serde_json::from_str(text).unwrap();
        assert_eq!(cfg.name, "书房");
        assert_eq!(cfg.labels, vec!["game".to_string()]);
        assert!(serde_json::to_value(&cfg).unwrap().get("rules").is_none());
    }

    #[test]
    fn a_pool_is_sent_only_when_it_is_a_usable_name() {
        let with = |p: &str| Config {
            pool: Some(p.into()),
            ..Config::default()
        };
        assert_eq!(Config::default().pool_name(), None);
        assert_eq!(with(" 渲染机 ").pool_name().as_deref(), Some("渲染机"));
        assert_eq!(with("   ").pool_name(), None);
        assert_eq!(with("a\u{0}b").pool_name(), None);
        assert_eq!(with(&"x".repeat(41)).pool_name(), None);
        let parsed: Config = serde_json::from_str(r#"{"pool": "rack-2"}"#).unwrap();
        assert_eq!(parsed.pool_name().as_deref(), Some("rack-2"));
    }

    #[test]
    fn one_gateway_rule_for_every_command() {
        let page = "https://agent-gateway-dev.agentrouter.page";
        let other = "https://gw.example.com";
        assert_eq!(
            gateway_for(Some(other), Some(page), page),
            (other.to_string(), "--gateway")
        );
        assert_eq!(
            gateway_for(Some(" "), Some(page), DEFAULT_GATEWAY),
            (page.to_string(), ENV_GATEWAY)
        );
        assert_eq!(
            gateway_for(None, None, page),
            (page.to_string(), "config.json")
        );
        assert_eq!(
            gateway_for(None, Some(""), ""),
            (DEFAULT_GATEWAY.to_string(), "默认")
        );
        assert_eq!(
            gateway_for(None, None, DEFAULT_GATEWAY),
            (DEFAULT_GATEWAY.to_string(), "默认")
        );
    }
}
