//! Share codes, the device side of the contract in agentrouter-cloud DEVICE-PROTOCOL.md §11–§12:
//! the 9-digit device code, the 8-character temporary password, and the four strings this device signs
//! with its own key (open, new password, stop, add to the cap). It only builds and checks values; the
//! calls themselves go through [`ShareService`], whose real implementation arrives with the backend
//! (`CONTROL_PLANE_DEVICE_SHARES`, off by default). Until then [`Unavailable`] answers every call.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::{DeviceKey, canonical_json, sha256_hex};
use crate::util::{hex, random_bytes};

/// The password alphabet (§11.2): A–Z without I, L, O, plus 2–9. 31 characters.
pub const PASSWORD_ALPHABET: &str = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// The device interface's default cap for sub-agents, in points (§11.4).
pub const DEFAULT_DELEGATE_CAP: i64 = 100;

/// A fresh temporary password from the operating system's random source (rejection sampling, so
/// every character is equally likely).
pub fn generate_password() -> String {
    let alphabet = PASSWORD_ALPHABET.as_bytes();
    let limit = 256 - (256 % alphabet.len());
    let mut out = String::with_capacity(8);
    while out.len() < 8 {
        for b in random_bytes::<16>() {
            if (b as usize) < limit && out.len() < 8 {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    out
}

/// Upper-case, drop spaces and `-`; exactly 8 characters from the alphabet, or `None` (§11.2).
pub fn normalise_password(input: &str) -> Option<String> {
    let value: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect();
    (value.chars().count() == 8 && value.chars().all(|c| PASSWORD_ALPHABET.contains(c)))
        .then_some(value)
}

/// `ABCDEFGH` → `ABCD-EFGH`.
pub fn display_password(password: &str) -> String {
    match normalise_password(password) {
        Some(p) => format!("{}-{}", &p[..4], &p[4..]),
        None => password.to_string(),
    }
}

/// Drop spaces and `-`; 9 digits not starting with 0, or `None` (§11.1). The code is not a secret.
pub fn normalise_code(input: &str) -> Option<String> {
    let value: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (value.len() == 9 && value.bytes().all(|b| b.is_ascii_digit()) && !value.starts_with('0'))
        .then_some(value)
}

/// `123456789` → `123 456 789`.
pub fn display_code(code: &str) -> String {
    match normalise_code(code) {
        Some(c) => format!("{} {} {}", &c[..3], &c[3..6], &c[6..]),
        None => code.to_string(),
    }
}

/// A new share id: `shr_` and 32 hex characters, made on the device (§11.4).
pub fn new_share_id() -> String {
    format!("shr_{}", hex(&random_bytes::<16>()))
}

/// How a share may be used (§11.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareTerms {
    pub share: String,
    /// `readonly`, `folders`, `confirm` or `full`.
    pub access: String,
    pub folders: Vec<String>,
    pub expires_at: i64,
    /// `every_session`, `once_per_account` or `none` (never `none` with `full`).
    pub approval: String,
    /// A non-empty subset of `remote`, `delegate`.
    pub modes: Vec<String>,
    /// Required with `delegate` (1–100000); 0 otherwise.
    pub delegate_cap_points: i64,
}

impl ShareTerms {
    /// `modes` sorted, de-duplicated and joined by `,` as signed.
    pub fn modes_text(&self) -> String {
        let mut modes = self.modes.clone();
        modes.sort();
        modes.dedup();
        modes.join(",")
    }

    /// The local checks of §11.4 (the control plane checks the same and more).
    pub fn problem(&self, now: i64) -> Option<&'static str> {
        let modes_ok =
            !self.modes.is_empty() && self.modes.iter().all(|m| m == "remote" || m == "delegate");
        if !self.share.starts_with("shr_")
            || self.share.len() != 36
            || !self.share[4..]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            Some("share")
        } else if !["readonly", "folders", "confirm", "full"].contains(&self.access.as_str()) {
            Some("access")
        } else if self.folders.len() > 20 || self.folders.iter().any(|f| f.chars().count() > 400) {
            Some("folders")
        } else if self.expires_at <= now + 60_000 || self.expires_at > now + 7 * 24 * 3_600_000 {
            Some("expiresAt")
        } else if !["every_session", "once_per_account", "none"].contains(&self.approval.as_str())
            || (self.access == "full" && self.approval == "none")
        {
            Some("approval")
        } else if !modes_ok {
            Some("modes")
        } else if self.modes.iter().any(|m| m == "delegate")
            && !(1..=100_000).contains(&self.delegate_cap_points)
        {
            Some("delegateCapPoints")
        } else {
            None
        }
    }
}

pub fn share_string(device: &str, terms: &ShareTerms, password: &str, at: i64) -> String {
    let cap = if terms.modes.iter().any(|m| m == "delegate") {
        terms.delegate_cap_points
    } else {
        0
    };
    let folders = sha256_hex(canonical_json(&json!(terms.folders)).as_bytes());
    format!(
        "agentrouter-device-share/v1\n{device}\n{}\n{}\n{folders}\n{}\n{}\n{}\n{cap}\n{}\n{at}",
        terms.share,
        terms.access,
        terms.expires_at,
        terms.approval,
        terms.modes_text(),
        sha256_hex(password.as_bytes()),
    )
}

pub fn password_string(device: &str, share: &str, password: &str, at: i64) -> String {
    format!(
        "agentrouter-device-share-password/v1\n{device}\n{share}\n{}\n{at}",
        sha256_hex(password.as_bytes())
    )
}

pub fn stop_string(device: &str, share: &str, reason: &str, at: i64) -> String {
    format!("agentrouter-device-share-stop/v1\n{device}\n{share}\n{reason}\n{at}")
}

pub fn cap_string(device: &str, share: &str, add_points: i64, at: i64) -> String {
    format!("agentrouter-device-share-cap/v1\n{device}\n{share}\n{add_points}\n{at}")
}

/// The signed body of `POST /device/v1/shares` (§11.4). `password` must already be normalised.
pub fn open_body(
    key: &DeviceKey,
    device: &str,
    terms: &ShareTerms,
    password: &str,
    at: i64,
) -> Value {
    let mut body = serde_json::to_value(terms).unwrap_or_default();
    if !terms.modes.iter().any(|m| m == "delegate") {
        body["delegateCapPoints"] = json!(0);
    }
    body["password"] = json!(password);
    body["at"] = json!(at);
    body["signature"] = json!(key.sign(&share_string(device, terms, password, at)));
    body
}

/// The signed body of `POST /device/v1/shares/:share/password` (§11.5).
pub fn password_body(key: &DeviceKey, device: &str, share: &str, password: &str, at: i64) -> Value {
    json!({"password": password, "at": at, "signature": key.sign(&password_string(device, share, password, at))})
}

/// The signed body of `POST /device/v1/shares/:share/stop`; `reason` is `person`, `expired` or `reported`.
pub fn stop_body(key: &DeviceKey, device: &str, share: &str, reason: &str, at: i64) -> Value {
    json!({"reason": reason, "at": at, "signature": key.sign(&stop_string(device, share, reason, at))})
}

/// The signed body of `POST /device/v1/shares/:share/cap`.
pub fn cap_body(key: &DeviceKey, device: &str, share: &str, add_points: i64, at: i64) -> Value {
    json!({"addPoints": add_points, "at": at, "signature": key.sign(&cap_string(device, share, add_points, at))})
}

/// The share as this device shows it (the panel's 设备码 / 口令 box).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareView {
    pub share: String,
    /// `123 456 789`.
    pub code: String,
    /// `ABCD-EFGH`; only on this device, never sent anywhere but the control plane.
    pub password: String,
    pub expires_at: i64,
    pub access: String,
}

/// A refusal with the control plane's code (§11.0) and a sentence for the person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShareError {
    pub code: String,
    pub message: String,
}

impl ShareError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

/// The share and remote-connect calls of §11–§12, as the desktop UI uses them.
pub trait ShareService: Send + Sync {
    /// Open a share (stops the old one, §11.4).
    fn open(&self, terms: &ShareTerms) -> Result<ShareView, ShareError>;
    /// A new password; connected sessions stay (§11.5).
    fn rotate(&self, share: &str) -> Result<ShareView, ShareError>;
    /// Stop sharing; every session of the share ends.
    fn stop(&self, share: &str, reason: &str) -> Result<(), ShareError>;
    /// Connect to someone else's computer (§12.1); returns the remote session view.
    fn connect(&self, code: &str, password: &str) -> Result<Value, ShareError>;
}

/// Sharing is not switched on for this build yet: every call says so, after checking the input
/// (so the person still learns about a mistyped code right away).
pub struct Unavailable;

pub const NOT_YET: &str = "分享还在开通中，过几天就能用。";

impl ShareService for Unavailable {
    fn open(&self, _: &ShareTerms) -> Result<ShareView, ShareError> {
        Err(ShareError::new("SHARES_UNAVAILABLE", NOT_YET))
    }
    fn rotate(&self, _: &str) -> Result<ShareView, ShareError> {
        Err(ShareError::new("SHARES_UNAVAILABLE", NOT_YET))
    }
    fn stop(&self, _: &str, _: &str) -> Result<(), ShareError> {
        Ok(())
    }
    fn connect(&self, code: &str, password: &str) -> Result<Value, ShareError> {
        if normalise_code(code).is_none() {
            return Err(ShareError::new(
                "SHARE_INPUT_INVALID",
                "设备码是 9 位数字。",
            ));
        }
        if normalise_password(password).is_none() {
            return Err(ShareError::new(
                "SHARE_INPUT_INVALID",
                "口令是 8 位字母和数字，像 ABCD-EFGH。",
            ));
        }
        Err(ShareError::new("SHARES_UNAVAILABLE", NOT_YET))
    }
}

/// The person-facing sentence for a control-plane code of §11–§12.
pub fn message_for(code: &str) -> &'static str {
    match code {
        "SHARE_CREDENTIALS_INVALID" => "设备码或口令不对。",
        "SHARE_LOCKED" => "输错太多次了，等一会儿再试。",
        "SESSION_LIMIT" => "你同时连着的电脑太多了，先断开一台。",
        "REMOTE_SUSPENDED" => "远程连接暂停了，正在审核。",
        "REMOTE_NEW_ACCOUNT_LIMIT" => "新账号每天最多连 2 台电脑。",
        "DEVICE_OFFLINE" => "对方的电脑现在不在线。",
        "SESSION_DENIED" => "对方拒绝了。",
        "SESSION_APPROVAL_TIMEOUT" => "对方没有点允许。",
        "SELF_SHARE" => "这是你自己的电脑，直接在“我的设备”里用。",
        "SHARE_NEW_DEVICE_LIMIT" => "刚链接的电脑，分享最长 1 小时，也不能完全访问。",
        "SHARES_UNAVAILABLE" => NOT_YET,
        _ => "没成功，再试一次。",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_from_the_alphabet() {
        for _ in 0..200 {
            let p = generate_password();
            assert_eq!(normalise_password(&p).as_deref(), Some(p.as_str()));
        }
        assert_eq!(display_password("abcdefgh"), "ABCD-EFGH");
    }

    #[test]
    fn codes() {
        assert_eq!(normalise_code("123 456-789").as_deref(), Some("123456789"));
        assert_eq!(normalise_code("023456789"), None);
        assert_eq!(normalise_code("12345678"), None);
        assert_eq!(display_code("123456789"), "123 456 789");
        assert!(new_share_id().starts_with("shr_"));
        assert_eq!(new_share_id().len(), 36);
    }
}
