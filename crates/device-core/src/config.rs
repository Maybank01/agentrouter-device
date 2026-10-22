//! What the person chose on this device (access level, folders, gateway, pause). Not secret:
//! the device key and the pinned control key live in the key store (DPAPI on Windows).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::data_dir;

/// Where devices connect. MVP preview: the Dev gateway (production enables devices later).
pub const DEFAULT_GATEWAY: &str = "https://agent-gateway-dev.agentrouter.top";

/// The access level of the old device chain. Console mode has none (the folder and `--read-only` are the boundary);
/// only `agentrouter mcp-serve local` still reads it (deploy targets at `full`), pending its design (HOSTD-MCP §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read files in the chosen folders; no commands, no writes.
    #[default]
    Readonly,
    /// Read, write and run commands in the chosen folders without asking (the coding default, owner
    /// decision 2026-10-10); commands that clearly act outside them are refused, checkpoints allow undo.
    Folders,
    /// Like `folders`, and every file write is confirmed too.
    Confirm,
    /// Like giving the AI an ssh account: nothing is blocked, everything is recorded.
    Full,
}

impl Access {
    pub const ALL: [Access; 4] = [
        Access::Readonly,
        Access::Folders,
        Access::Confirm,
        Access::Full,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Readonly => "readonly",
            Access::Folders => "folders",
            Access::Confirm => "confirm",
            Access::Full => "full",
        }
    }

    pub fn parse(text: &str) -> Option<Access> {
        match text.trim().to_ascii_lowercase().as_str() {
            "readonly" | "read-only" | "read" => Some(Access::Readonly),
            "folders" | "folder" | "workspace" => Some(Access::Folders),
            "confirm" => Some(Access::Confirm),
            "full" => Some(Access::Full),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Access::Readonly => "只读",
            Access::Folders => "只限这些文件夹",
            Access::Confirm => "每条都确认",
            Access::Full => "完全访问",
        }
    }

    /// What the level lets a cloud conversation do, for the consent dialogs and the README.
    pub fn explain(self) -> &'static str {
        match self {
            Access::Readonly => {
                "AI 可以：读取你选的文件夹里的文件。\nAI 不能：运行命令、修改或删除文件、碰这些文件夹以外的任何东西。"
            }
            Access::Folders => {
                "AI 可以：在你链接的文件夹里读写文件、运行命令，不用每次问你；每轮改动之前自动打检查点，可以用 agentrouter undo 撤销。\nAI 不能：读写这些文件夹以外的文件；明显越界的命令（改系统、碰密码和密钥、提权、全局安装）会被直接拦下，确实需要时单独问你一次。"
            }
            Access::Confirm => {
                "AI 可以：读取你选的文件夹里的文件；每条命令、每次写文件都先在这台电脑上弹窗问你。\nAI 不能：不经你同意做任何改动、碰这些文件夹以外的文件。"
            }
            Access::Full => {
                "AI 可以：像有了这台电脑的 ssh 账号一样，运行任何命令、读写任何你能读写的文件。所有操作都记在本机审计日志和网页控制台里。\n请只在信任这个对话时使用。"
            }
        }
    }

    pub fn needs_folders(self) -> bool {
        self != Access::Full
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub gateway: String,
    pub name: String,
    pub access: Access,
    pub folders: Vec<String>,
    /// Unattended (servers, CI): the machine's operator approved commands up front, at link time, on
    /// this machine. Nothing is asked at run time; everything is still audited.
    pub unattended: bool,
    /// Nobody sits at this machine (CI, headless test runs): a local question answers "no" at once
    /// instead of opening a dialog. Only this local file sets it; it can never approve anything.
    pub headless: bool,
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
            access: Access::default(),
            folders: Vec::new(),
            unattended: false,
            headless: false,
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
    fn access_round_trip() {
        for a in Access::ALL {
            assert_eq!(Access::parse(a.as_str()), Some(a));
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(json, format!("\"{}\"", a.as_str()));
        }
        assert_eq!(Access::parse("Read-Only"), Some(Access::Readonly));
        assert_eq!(Access::parse("root"), None);
    }
}
