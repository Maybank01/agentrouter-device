//! Small helpers: time, encodings, randomness, where the app keeps its files, and the log.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// An ISO-8601 UTC timestamp (`2026-10-08T12:00:00.000Z`).
pub fn iso_now() -> String {
    iso(now_ms())
}

pub fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

pub fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

pub fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the operating system's random source failed");
    out
}

/// A short random id with a prefix (`job_k3x9q2m1`).
pub fn short_id(prefix: &str) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = random_bytes::<10>();
    let tail: String = bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect();
    format!("{prefix}{tail}")
}

/// Where this app keeps its config, key, audit log and job output. `AGENTROUTER_DEVICE_HOME` overrides it (tests, portable use).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AGENTROUTER_DEVICE_HOME") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("AgentRouter").join("Device");
        }
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir()
            .join("Library")
            .join("Application Support")
            .join("AgentRouter Device");
    }
    #[allow(unreachable_code)]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("agentrouter-device");
        }
        home_dir()
            .join(".local")
            .join("share")
            .join("agentrouter-device")
    }
}

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let var = std::env::var_os("HOME");
    var.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn host_name() -> String {
    #[cfg(windows)]
    let name = std::env::var("COMPUTERNAME").ok();
    #[cfg(not(windows))]
    let name = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok());
    name.map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "我的电脑".to_string())
}

pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "darwin",
        other => other,
    }
}

pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// A line in the app's own log (`device.log`, capped; never secrets).
pub fn log(message: &str) {
    let line = format!("{} {message}\n", iso_now());
    eprint!("{line}");
    let dir = data_dir();
    let path = dir.join("device.log");
    if std::fs::metadata(&path)
        .map(|m| m.len() > 4 << 20)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, dir.join("device.log.1"));
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Cut text to at most `max` characters, saying how long it was.
pub fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…（共 {count} 字）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_formats_utc() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_760_000_060_123), "2025-10-09T08:54:20.123Z");
    }

    #[test]
    fn hex_and_b64url() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(b64url_decode(&b64url(b"hello")).unwrap(), b"hello");
    }
}
