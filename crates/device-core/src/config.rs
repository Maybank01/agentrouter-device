//! What the person chose on this device (access level, folders, gateway, pause). Not secret:
//! the device key and the pinned control key live in the key store (DPAPI on Windows).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::util::data_dir;

/// Where devices connect. MVP preview: the Dev gateway (production enables devices later).
pub const DEFAULT_GATEWAY: &str = "https://agent-gateway-dev.agentrouter.top";
/// The cloud web app the desktop shell shows. MVP preview: Dev.
pub const DEFAULT_WEB: &str = "https://edge-d53hmn9blslvhcuff8rjsdvt.agentrouter.top";

/// The access level, chosen on the device and only on the device (the web shows it, never sets it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read files in the chosen folders; no commands, no writes.
    #[default]
    Readonly,
    /// Read and write files in the chosen folders; each command is confirmed on this device.
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
                "AI 可以：读写你选的文件夹里的文件；运行命令，但每条命令都先在这台电脑上弹窗问你。\nAI 不能：不经你同意运行命令、读写这些文件夹以外的文件。"
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
    /// The web app the desktop shell loads (same pages as the browser).
    pub web: String,
    pub name: String,
    pub access: Access,
    pub folders: Vec<String>,
    /// Paused: the channel is closed until the person resumes; running jobs keep running.
    pub paused: bool,
    /// Disconnected from the tray: every job was stopped; only the person can connect again.
    pub disconnected: bool,
    /// Unattended (servers, CI): the machine's operator approved commands up front, at link time, on
    /// this machine. Nothing is asked at run time; everything is still audited.
    pub unattended: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gateway: DEFAULT_GATEWAY.to_string(),
            web: DEFAULT_WEB.to_string(),
            name: crate::util::host_name(),
            access: Access::default(),
            folders: Vec::new(),
            paused: false,
            disconnected: false,
            unattended: false,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.json")
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
