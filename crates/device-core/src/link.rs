//! Linking this computer (device code flow) and the consent texts shown before it and before any
//! change of access level.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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

/// How often the first pairing request is sent before giving up on a transient failure (#190: from a PC in China the
/// TLS handshake to the gateway sometimes stalls past the request timeout; one stall failed the whole first link).
const PAIR_ATTEMPTS: u32 = 4;

/// A failure a new attempt can fix: no answer (network, timeout), rate limited, or the gateway's own 5xx.
fn transient(e: &net::HttpError) -> bool {
    e.status == 0 && e.code == "NETWORK" || e.status == 429 || e.status >= 500
}

/// The pairing request's failure as the person reads it: what failed, where, the cause, and what to do.
fn pairing_error(gateway: &str, e: &net::HttpError, attempts: u32) -> String {
    let tried = if attempts > 1 {
        format!("（试了 {attempts} 次）")
    } else {
        String::new()
    };
    match e.status {
        0 if e.code == "NETWORK" => format!(
            "连不上网关 {gateway}，没拿到配对码{tried}：{}。检查这台电脑能不能打开这个网址（代理、防火墙），再运行一次 agentrouter link。",
            e.message
        ),
        0 => format!("没拿到配对码：{}", e.message),
        404 => format!(
            "网关 {gateway} 没有设备链接（/device/v1/pair 404）：网关地址不对，或这个环境没开放设备链接。"
        ),
        _ => format!("网关 {gateway} 拒绝了配对请求{tried}：{e}"),
    }
}

/// Pair: a new key, a code for the person, then wait for their confirmation. Returns the device id; the error says
/// what failed (never a fixed sentence: #190).
pub fn link(
    cfg: &Config,
    stop: &AtomicBool,
    show_code: impl FnOnce(&Pairing),
) -> Result<String, String> {
    let key = DeviceKey::generate();
    let mut attempt = 0;
    let pairing = loop {
        attempt += 1;
        match net::start_pairing(&cfg.gateway, &key, &cfg.name) {
            Ok(pairing) => break pairing,
            Err(e) => {
                crate::util::log(&format!(
                    "pairing request {attempt}/{PAIR_ATTEMPTS} to {} failed: {e}",
                    cfg.gateway
                ));
                if !transient(&e) || attempt >= PAIR_ATTEMPTS {
                    return Err(pairing_error(&cfg.gateway, &e, attempt));
                }
            }
        }
        // 2 s, 4 s, 8 s; Ctrl+C ends the wait.
        let until = Instant::now() + Duration::from_secs(1 << attempt);
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return Err("已取消链接。".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    };
    show_code(&pairing);
    net::wait_for_approval(&cfg.gateway, &key, &pairing, stop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(status: u16, code: &str, message: &str) -> net::HttpError {
        net::HttpError {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    #[test]
    fn only_transient_pairing_failures_are_retried() {
        assert!(transient(&err(0, "NETWORK", "timeout")));
        assert!(transient(&err(429, "", "")));
        assert!(transient(&err(502, "", "")));
        assert!(!transient(&err(0, "GATEWAY_NOT_ALLOWED", "")));
        assert!(!transient(&err(400, "BAD_REQUEST", "")));
        assert!(!transient(&err(404, "", "")));
    }

    #[test]
    fn a_pairing_error_names_the_gateway_and_the_cause() {
        let g = "https://agent-gateway-dev.agentrouter.top";
        let text = pairing_error(g, &err(0, "NETWORK", "timeout: global"), 4);
        assert!(
            text.contains(g) && text.contains("timeout: global") && text.contains("试了 4 次"),
            "{text}"
        );
        let text = pairing_error(
            g,
            &err(400, "BAD_SIGNATURE", "signature does not verify"),
            1,
        );
        assert!(
            text.contains("BAD_SIGNATURE") && text.contains("signature does not verify"),
            "{text}"
        );
        assert!(!text.contains("配对码过期"), "{text}");
    }

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
