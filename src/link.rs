//! Linking this computer (device code flow) and the consent texts shown before it and before any
//! change of access level.

use std::sync::atomic::AtomicBool;

use crate::config::{Access, Config};
use crate::net::{self, Pairing};
use crate::protocol::DeviceKey;

fn folders_text(folders: &[String]) -> String {
    if folders.is_empty() {
        "（没有）".to_string()
    } else {
        folders.join("\n")
    }
}

/// The question before the first connection.
pub fn link_question(cfg: &Config) -> String {
    format!(
        "把这台电脑链接到 AgentRouter？\n\n名字：{}\n访问级别：{}\n{}\n\n文件夹：\n{}\n\n链接后，你在网页上同意的对话就能按这个级别使用这台电脑。随时可以在托盘里暂停或断开。",
        cfg.name,
        cfg.access.label(),
        cfg.access.explain(),
        folders_text(&cfg.folders)
    )
}

/// The question before an access level or folder change.
pub fn change_question(access: Access, folders: &[String]) -> String {
    format!(
        "把这台电脑的访问级别设成「{}」？\n\n{}\n\n文件夹：\n{}",
        access.label(),
        access.explain(),
        folders_text(folders)
    )
}

pub fn code_text(pairing: &Pairing) -> String {
    format!(
        "在网页上输入这个配对码：\n\n{}\n\n网址：{}\n{} 分钟内有效。",
        pairing.code,
        pairing.verify_url,
        pairing.expires_in / 60
    )
}

/// Only a web page may be opened from what the gateway sends (never a file or a program).
pub fn safe_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let local = [
        "http://127.0.0.1:",
        "http://127.0.0.1/",
        "http://localhost:",
        "http://localhost/",
    ];
    (lower.starts_with("https://") || local.iter().any(|p| lower.starts_with(p)))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || (c as u32) < 32)
}

/// Pair: a new key, a code for the person, then wait for their confirmation. Returns the device id.
pub fn link(
    cfg: &Config,
    stop: &AtomicBool,
    show_code: impl FnOnce(&Pairing),
) -> Result<String, String> {
    let key = DeviceKey::generate();
    let pairing =
        net::start_pairing(&cfg.gateway, &key, &cfg.name).map_err(|e| match e.status {
            0 => format!("连不上 {}：{}", cfg.gateway, e.message),
            404 => "这个网关还没有开放设备链接。".to_string(),
            _ => format!("链接没有成功（{e}）"),
        })?;
    show_code(&pairing);
    net::wait_for_approval(&cfg.gateway, &key, &pairing, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_pages_open() {
        assert!(safe_url("https://agentrouter.top/settings/devices"));
        assert!(safe_url("http://127.0.0.1:3000/settings/devices"));
        assert!(!safe_url("file:///C:/Windows/System32/calc.exe"));
        assert!(!safe_url("C:\\Windows\\System32\\calc.exe"));
        assert!(!safe_url("https://x.example/\" & calc"));
        assert!(!safe_url("http://evil.example/"));
    }
}
