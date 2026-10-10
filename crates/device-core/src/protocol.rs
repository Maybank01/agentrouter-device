//! The linked-device protocol, device side (agentrouter-cloud `docs/product/workspace-v1/DEVICE-PROTOCOL.md`):
//! the three signed strings, canonical JSON for argument digests, Ed25519 keys as raw base64url, and the
//! checks every signed request goes through before the local gate sees it.

use std::collections::HashMap;
use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

use crate::util::{b64url, b64url_decode, hex};

pub const PROTOCOL_VERSION: i64 = 1;
/// A signed request lives 60 s; a device refuses one claiming to live longer (plus 5 s of clock skew).
pub const REQUEST_MAX_AHEAD_MS: i64 = 60_000 + 5_000;

pub fn pair_string(public_key: &str, name: &str, os: &str, arch: &str) -> String {
    format!("agentrouter-device-pair/v1\n{public_key}\n{name}\n{os}\n{arch}")
}

pub fn token_string(device: &str, challenge: &str) -> String {
    format!("agentrouter-device-token/v1\n{device}\n{challenge}")
}

pub fn request_string(
    device: &str,
    session: &str,
    action: &str,
    digest: &str,
    exp: i64,
    nonce: &str,
) -> String {
    format!(
        "agentrouter-device-request/v1\n{device}\n{session}\n{action}\n{digest}\n{exp}\n{nonce}"
    )
}

/// A protocol error: the code the gateway and the tool see, and a sentence the AI can pass on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceError {
    pub code: &'static str,
    pub message: String,
    /// What the AI should do next (DEVICE-PROTOCOL.md §5.3), when there is something better than retrying.
    pub next: Option<String>,
}

/// `next` for a read-only device (DEVICE-PROTOCOL.md §5.3).
pub const NEXT_READONLY: &str = "这台设备只读：可以读文件、搜索。要改文件或运行命令，请用户在电脑上运行 agentrouter access folders。";
/// `next` for a path outside the linked folders.
pub const NEXT_OUTSIDE: &str = "只能用链接的文件夹里的路径（info 里的 folders）。需要别的文件夹时，请用户在那个文件夹里运行 agentrouter link。";
/// `next` when no folder is linked.
pub const NEXT_NO_FOLDER: &str =
    "这台设备还没有链接文件夹。请用户在项目文件夹里运行 agentrouter link。";

impl DeviceError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            next: None,
        }
    }
    pub fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new("DENIED", message)
    }
    pub fn invalid_signature(message: impl Into<String>) -> Self {
        Self::new("SIGNATURE_INVALID", message)
    }
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DeviceError {}

/// Canonical JSON: object keys sorted by UTF-16 code units, no whitespace, arrays in order, strings and
/// numbers as JavaScript's `JSON.stringify` writes them.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&js_number(n)),
        Value::String(s) => out.push_str(&json_string(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
    }
}

/// A JSON string the way `JSON.stringify` writes it (serde_json escapes the same characters).
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// A number the way JavaScript prints it (Number::toString: shortest round-trip digits, exponent past 1e21 or below 1e-6).
pub fn js_number(n: &Number) -> String {
    const SAFE: u64 = 1 << 53;
    if let Some(i) = n.as_u64() {
        if i <= SAFE {
            return i.to_string();
        }
    } else if let Some(i) = n.as_i64()
        && i.unsigned_abs() <= SAFE
    {
        return i.to_string();
    }
    js_f64(n.as_f64().unwrap_or(0.0))
}

pub fn js_f64(x: f64) -> String {
    if x.is_nan() || x.is_infinite() {
        return "null".to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let negative = x < 0.0;
    // Rust's `{:e}` gives the shortest round-trip digits: "1.234e-7".
    let formatted = format!("{:e}", x.abs());
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let k = digits.len() as i64;
    let n = exponent.parse::<i64>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{sign}{}", e.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    if negative { format!("-{body}") } else { body }
}

/// The argument digest: SHA-256 (hex) of the canonical JSON.
pub fn args_digest(args: &Value) -> String {
    hex(&Sha256::digest(canonical_json(args).as_bytes()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The control plane's signing key a device pins at pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlKey {
    pub kid: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
}

impl ControlKey {
    /// The kid is the first 16 hex characters of SHA-256 over the raw public key; check it matches.
    pub fn is_consistent(&self) -> bool {
        match b64url_decode(&self.public_key) {
            Some(raw) if raw.len() == 32 => sha256_hex(&raw)[..16] == self.kid,
            _ => false,
        }
    }
}

/// Verify an Ed25519 signature (raw base64url key and signature) over a UTF-8 string.
pub fn verify(public_key: &str, message: &str, signature: &str) -> bool {
    let Some(key) = b64url_decode(public_key).and_then(|k| <[u8; 32]>::try_from(k).ok()) else {
        return false;
    };
    let Some(sig) = b64url_decode(signature).and_then(|s| <[u8; 64]>::try_from(s).ok()) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key) else {
        return false;
    };
    key.verify_strict(message.as_bytes(), &Signature::from_bytes(&sig))
        .is_ok()
}

/// The device's own key pair (Ed25519; the private half never leaves the device).
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn generate() -> Self {
        Self::from_seed(crate::util::random_bytes::<32>())
    }
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
    /// The raw public key, base64url (43 characters).
    pub fn public_key(&self) -> String {
        b64url(self.0.verifying_key().as_bytes())
    }
    /// A base64url signature (86 characters) over a UTF-8 string.
    pub fn sign(&self, message: &str) -> String {
        b64url(&self.0.sign(message.as_bytes()).to_bytes())
    }
}

/// Nonces seen, kept until their request would have expired anyway.
#[derive(Default)]
pub struct ReplayCache {
    seen: HashMap<String, i64>,
}

impl ReplayCache {
    /// False when this nonce was already used.
    pub fn remember(&mut self, nonce: &str, exp: i64, now: i64) -> bool {
        self.seen.retain(|_, e| *e > now);
        if self.seen.contains_key(nonce) {
            return false;
        }
        if self.seen.len() > 100_000 {
            return false;
        }
        self.seen.insert(nonce.to_string(), exp);
        true
    }
}

/// A request that passed every protocol check.
#[derive(Debug, Clone)]
pub struct Verified {
    pub session: String,
    pub action: String,
}

/// The protocol checks, in the order DEVICE-PROTOCOL.md §5.1 gives them. Only after these does the
/// local gate (access level, folders, confirmation) decide.
pub fn check_request(
    request: &Value,
    args: &Value,
    pinned: &ControlKey,
    device_id: &str,
    now: i64,
    replay: &mut ReplayCache,
) -> Result<Verified, DeviceError> {
    let field = |name: &str| request.get(name).and_then(Value::as_str).unwrap_or("");
    if request.get("v").and_then(Value::as_i64) != Some(PROTOCOL_VERSION) {
        return Err(DeviceError::invalid_signature(
            "unsupported request version",
        ));
    }
    if field("kid") != pinned.kid {
        return Err(DeviceError::invalid_signature("unknown signing key"));
    }
    let exp_value = request.get("exp");
    let exp = exp_value.and_then(Value::as_i64);
    let (device, session, action, digest, nonce) = (
        field("device"),
        field("session"),
        field("action"),
        field("digest"),
        field("nonce"),
    );
    let Some(exp) = exp else {
        return Err(DeviceError::invalid_signature("bad expiry"));
    };
    let text = request_string(device, session, action, digest, exp, nonce);
    if !verify(&pinned.public_key, &text, field("sig")) {
        return Err(DeviceError::invalid_signature("bad signature"));
    }
    if device != device_id {
        return Err(DeviceError::new(
            "WRONG_DEVICE",
            "signed for another device",
        ));
    }
    if args_digest(args) != digest {
        return Err(DeviceError::invalid_signature(
            "arguments do not match the signature",
        ));
    }
    if exp <= now {
        return Err(DeviceError::new("REQUEST_EXPIRED", "expired"));
    }
    if exp > now + REQUEST_MAX_AHEAD_MS {
        return Err(DeviceError::new("REQUEST_EXPIRED", "lives too long"));
    }
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(DeviceError::invalid_signature("bad nonce"));
    }
    if !replay.remember(nonce, exp, now) {
        return Err(DeviceError::new(
            "REPLAYED",
            "this request was already used",
        ));
    }
    Ok(Verified {
        session: session.to_string(),
        action: action.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_numbers() {
        let cases = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (0.1, "0.1"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.234e-7, "1.234e-7"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (123456.789, "123456.789"),
            (-3.25e-10, "-3.25e-10"),
        ];
        for (x, want) in cases {
            assert_eq!(js_f64(x), want, "{x}");
        }
    }

    #[test]
    fn keys_sort_by_utf16() {
        // U+E000 sorts after U+1F600 in UTF-16 (surrogates are 0xD83D…) but before it in UTF-8.
        let v: Value =
            serde_json::from_str("{\"\u{E000}\":1,\"\u{1F600}\":2,\"b\":3,\"B\":4}").unwrap();
        assert_eq!(
            canonical_json(&v),
            "{\"B\":4,\"b\":3,\"\u{1F600}\":2,\"\u{E000}\":1}"
        );
    }

    #[test]
    fn device_key_round_trip() {
        let key = DeviceKey::generate();
        let sig = key.sign("hello");
        assert_eq!(key.public_key().len(), 43);
        assert_eq!(sig.len(), 86);
        assert!(verify(&key.public_key(), "hello", &sig));
        assert!(!verify(&key.public_key(), "hellO", &sig));
    }
}
