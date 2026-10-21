//! Talking to the gateway over HTTPS: pairing and the short-lived token for a link. The link itself (the one
//! outbound WebSocket, reconnects, frames) is the `agentrouter-hostd-link` crate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::keystore::{self, Identity};
use crate::protocol::{ControlKey, DeviceKey, pair_string, token_string};
use crate::util::log;

#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "HTTP {} {} {}", self.status, self.code, self.message)
        }
    }
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)));
    // The binary picks the TLS backend (connector-cli features): the system's on PCs, rustls in the static build.
    #[cfg(feature = "native-tls")]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .build(),
    );
    #[cfg(all(feature = "rustls", not(feature = "native-tls")))]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .build(),
    );
    config.build().new_agent()
}

/// Gateways the device talks to: a domain name over TLS (no raw IP addresses), or this computer for
/// local testing (docs/AV-HYGIENE.md).
pub fn gateway_allowed(gateway: &str) -> Result<(), String> {
    let lower = gateway.trim().to_ascii_lowercase();
    for local in ["http://127.0.0.1", "http://localhost"] {
        if let Some(rest) = lower.strip_prefix(local)
            && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
        {
            return Ok(());
        }
    }
    let Some(rest) = lower.strip_prefix("https://") else {
        return Err(format!("网关必须是 https 网址：{gateway}"));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let name = host.split(':').next().unwrap_or("");
    let ip_like = host.starts_with('[') || name.chars().all(|c| c.is_ascii_digit() || c == '.');
    if name.is_empty() || ip_like || !name.contains('.') || host.contains('@') {
        return Err(format!("网关必须是域名（不能是 IP 地址）：{gateway}"));
    }
    Ok(())
}

pub fn post(gateway: &str, path: &str, body: &Value) -> Result<Value, HttpError> {
    gateway_allowed(gateway).map_err(|message| HttpError {
        status: 0,
        code: "GATEWAY_NOT_ALLOWED".into(),
        message,
    })?;
    let url = format!("{}{path}", gateway.trim_end_matches('/'));
    let mut response = agent().post(&url).send_json(body).map_err(|e| HttpError {
        status: 0,
        code: "NETWORK".into(),
        message: e.to_string(),
    })?;
    let status = response.status().as_u16();
    let value: Value = response.body_mut().read_json().unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let error = value.get("error");
    Err(HttpError {
        status,
        code: error
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        message: error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// A pairing in progress: the code the person types on the web.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub pairing: String,
    pub poll_secret: String,
    pub code: String,
    pub expires_in: u64,
    pub verify_url: String,
    /// The same page with this code filled in (RFC 8628 `verification_uri_complete`), when the gateway sends one:
    /// the person only confirms there (double-click start, `start.rs`).
    pub verify_url_complete: Option<String>,
}

pub fn start_pairing(gateway: &str, key: &DeviceKey, name: &str) -> Result<Pairing, HttpError> {
    let (os, arch) = (crate::util::os_name(), crate::util::arch_name());
    let public_key = key.public_key();
    let signature = key.sign(&pair_string(&public_key, name, os, arch));
    let v = post(
        gateway,
        "/device/v1/pair",
        &json!({"publicKey": public_key, "name": name, "os": os, "arch": arch, "signature": signature}),
    )?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Pairing {
        pairing: s("pairing"),
        poll_secret: s("pollSecret"),
        code: s("code"),
        expires_in: v.get("expiresIn").and_then(Value::as_u64).unwrap_or(600),
        verify_url: s("verifyUrl"),
        verify_url_complete: Some(s("verifyUrlComplete")).filter(|u| !u.is_empty()),
    })
}

pub enum PollResult {
    Pending,
    Expired,
    Approved { device: String, control: ControlKey },
}

pub fn poll_pairing(gateway: &str, pairing: &Pairing) -> Result<PollResult, HttpError> {
    let v = post(
        gateway,
        "/device/v1/pair/poll",
        &json!({"pairing": pairing.pairing, "pollSecret": pairing.poll_secret}),
    )?;
    match v.get("status").and_then(Value::as_str) {
        Some("approved") => {
            let device = v
                .get("device")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let control: Option<ControlKey> = v
                .get("control")
                .and_then(|c| serde_json::from_value(c.clone()).ok());
            match control {
                Some(control) if control.is_consistent() && device.starts_with("lnd_") => {
                    Ok(PollResult::Approved { device, control })
                }
                _ => Err(HttpError {
                    status: 0,
                    code: "BAD_ANSWER".into(),
                    message: "the approval did not carry a valid control key".into(),
                }),
            }
        }
        Some("expired") => Ok(PollResult::Expired),
        _ => Ok(PollResult::Pending),
    }
}

/// Wait for the person to confirm the code (until it expires or `stop` is set); saves the identity.
pub fn wait_for_approval(
    gateway: &str,
    key: &DeviceKey,
    pairing: &Pairing,
    stop: &AtomicBool,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(pairing.expires_in.max(30));
    while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        match poll_pairing(gateway, pairing) {
            Ok(PollResult::Approved { device, control }) => {
                keystore::save(key, &device, &control)
                    .map_err(|e| format!("could not store the device key: {e}"))?;
                return Ok(device);
            }
            Ok(PollResult::Expired) => return Err("配对码已过期，请重新链接。".into()),
            Ok(PollResult::Pending) => {}
            Err(e) if e.status == 404 => return Err("配对已失效，请重新链接。".into()),
            Err(e) => log(&format!("pairing poll failed: {e}")),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if stop.load(Ordering::SeqCst) {
        return Err("已取消链接。".into());
    }
    Err("配对码已过期，请重新链接。".into())
}

/// A signed challenge buys a short-lived token for one connection.
pub fn fetch_token(gateway: &str, identity: &Identity) -> Result<String, HttpError> {
    let v = post(
        gateway,
        "/device/v1/challenge",
        &json!({"device": identity.device}),
    )?;
    let challenge = v
        .get("challenge")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let signature = identity
        .key
        .sign(&token_string(&identity.device, &challenge));
    let v = post(
        gateway,
        "/device/v1/token",
        &json!({"device": identity.device, "challenge": challenge, "signature": signature}),
    )?;
    Ok(v.get("token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

#[cfg(test)]
mod gateway_tests {
    use super::gateway_allowed;

    #[test]
    fn only_domains_over_tls_or_this_computer() {
        assert!(gateway_allowed("https://agent-gateway-dev.agentrouter.top").is_ok());
        assert!(gateway_allowed("https://agent-gateway-dev.agentrouter.top:443/").is_ok());
        assert!(gateway_allowed("http://127.0.0.1:8081").is_ok());
        assert!(gateway_allowed("http://localhost:8081").is_ok());
        assert!(gateway_allowed("http://agent-gateway-dev.agentrouter.top").is_err());
        assert!(gateway_allowed("https://203.0.113.7").is_err());
        assert!(gateway_allowed("https://[2001:db8::1]:443").is_err());
        assert!(gateway_allowed("https://user@203.0.113.7").is_err());
        assert!(gateway_allowed("http://127.0.0.1.evil.example").is_err());
        assert!(gateway_allowed("https://intranet").is_err());
    }
}
