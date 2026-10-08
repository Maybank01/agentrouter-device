//! Talking to the gateway: pairing and token calls over HTTPS, and the device's one outbound
//! WebSocket (no inbound port, ever), kept up with reconnects.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::config::Config;
use crate::device::Device;
use crate::keystore::{self, Identity};
use crate::protocol::{ControlKey, DeviceKey, pair_string, token_string};
use crate::util::log;

const FRAME_MAX: usize = 8 << 20;
/// The gateway pings every 20 s; this long without any traffic means the channel is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);

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
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent()
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

/// What the tray shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    NotLinked,
    Paused,
    Disconnected,
    Connecting,
    Online,
    Offline(String),
}

/// The app's running state, shared by the connection loop and the tray.
pub struct Runtime {
    pub device: Arc<Device>,
    pub config: Mutex<Config>,
    pub conn: Mutex<Conn>,
    pub stop: AtomicBool,
    /// Set to drop the current connection (access changed: the next `hello` reports it).
    pub reconnect: AtomicBool,
    /// Set when the web revoked this device (the tray tells the person once).
    pub revoked_notice: AtomicBool,
}

impl Runtime {
    pub fn new(device: Arc<Device>, config: Config) -> Arc<Runtime> {
        Arc::new(Runtime {
            device,
            config: Mutex::new(config),
            conn: Mutex::new(Conn::NotLinked),
            stop: AtomicBool::new(false),
            reconnect: AtomicBool::new(false),
            revoked_notice: AtomicBool::new(false),
        })
    }

    pub fn conn(&self) -> Conn {
        self.conn.lock().unwrap().clone()
    }

    fn set_conn(&self, conn: Conn) {
        let mut current = self.conn.lock().unwrap();
        if *current != conn {
            log(&format!("connection: {conn:?}"));
            *current = conn;
        }
    }

    fn leave_requested(&self) -> bool {
        let cfg = self.config.lock().unwrap();
        self.stop.load(Ordering::SeqCst)
            || cfg.paused
            || cfg.disconnected
            || self.reconnect.load(Ordering::SeqCst)
    }

    /// Sleep, but wake early for a stop, a reconnect or a pause.
    fn nap(&self, total: Duration) {
        let end = Instant::now() + total;
        while Instant::now() < end && !self.leave_requested() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Forget this device's identity (revoked on the web): stop its jobs; only a new link brings it back.
    fn revoked(&self) {
        self.device.stop_all("revoked");
        keystore::forget();
        self.device.set_identity(None);
        self.revoked_notice.store(true, Ordering::SeqCst);
        self.device.record(json!({"event": "revoked"}));
    }
}

enum End {
    Left,
    Revoked,
    Refused(String),
    Closed(u16),
    Broken(String),
}

/// The connection loop: link state, token, WebSocket, reconnect with backoff. Runs until `stop`.
pub fn run(rt: Arc<Runtime>) {
    let mut backoff = Duration::from_secs(1);
    while !rt.stop.load(Ordering::SeqCst) {
        rt.reconnect.store(false, Ordering::SeqCst);
        let (gateway, paused, disconnected) = {
            let c = rt.config.lock().unwrap();
            (c.gateway.clone(), c.paused, c.disconnected)
        };
        if disconnected {
            rt.set_conn(Conn::Disconnected);
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        if paused {
            rt.set_conn(Conn::Paused);
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        let Some(identity) = keystore::load() else {
            rt.device.set_identity(None);
            rt.set_conn(Conn::NotLinked);
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let device_id = identity.device.clone();
        let token = match fetch_token(&gateway, &identity) {
            Ok(token) => token,
            // Only an explicit revocation forgets the identity (a gateway without device routes also answers 404).
            Err(e) if e.code == "DEVICE_REVOKED" => {
                log(&format!(
                    "the web no longer knows this device ({e}); forgetting it"
                ));
                rt.revoked();
                continue;
            }
            Err(e) => {
                rt.set_conn(Conn::Offline(e.to_string()));
                rt.nap(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        rt.device.set_identity(Some(identity));
        rt.set_conn(Conn::Connecting);
        let end = match connect(&gateway) {
            Ok(ws) => session(&rt, ws, &token),
            Err(e) => End::Broken(e),
        };
        rt.device.set_console(None);
        let welcomed = *rt.conn.lock().unwrap() == Conn::Online;
        match end {
            End::Left => {
                rt.set_conn(Conn::Connecting);
                continue;
            }
            End::Revoked => {
                log(&format!("device {device_id} was disconnected on the web"));
                rt.revoked();
                continue;
            }
            End::Refused(code) if code == "DEVICE_REVOKED" => {
                rt.revoked();
                continue;
            }
            End::Refused(code) => rt.set_conn(Conn::Offline(format!("refused: {code}"))),
            End::Closed(4409) => {
                // Another copy of this device connected: back off for a while.
                rt.set_conn(Conn::Offline("replaced by another connection".into()));
                rt.nap(Duration::from_secs(30));
                continue;
            }
            End::Closed(code) => rt.set_conn(Conn::Offline(format!("closed ({code})"))),
            End::Broken(why) => rt.set_conn(Conn::Offline(why)),
        }
        if welcomed {
            backoff = Duration::from_secs(1);
        }
        rt.nap(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn connect(gateway: &str) -> Result<Ws, String> {
    gateway_allowed(gateway)?;
    let base = gateway.trim_end_matches('/');
    let url = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/device/v1/connect")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/device/v1/connect")
    } else {
        return Err(format!("not an http(s) gateway: {gateway}"));
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(FRAME_MAX))
        .max_frame_size(Some(FRAME_MAX));
    let (mut ws, _) = tungstenite::client::connect_with_config(url.as_str(), Some(config), 3)
        .map_err(|e| format!("connect failed: {e}"))?;
    let tcp = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Some(&*s),
        MaybeTlsStream::NativeTls(t) => Some(t.get_ref()),
        _ => None,
    };
    if let Some(tcp) = tcp {
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(50)));
        let _ = tcp.set_nodelay(true);
    }
    Ok(ws)
}

fn would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

/// One connection: `hello`, then frames until it ends.
fn session(rt: &Arc<Runtime>, mut ws: Ws, token: &str) -> End {
    let hello = json!({"op": "hello", "v": 1, "token": token, "info": rt.device.info()});
    if let Err(e) = ws.send(Message::text(hello.to_string())) {
        return End::Broken(format!("hello failed: {e}"));
    }
    let (tx, rx) = mpsc::channel::<String>();
    let cancels: Arc<Mutex<HashMap<i64, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut last_rx = Instant::now();
    let end = loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                last_rx = Instant::now();
                if let Some(end) = on_frame(rt, text.as_str(), &tx, &cancels) {
                    break end;
                }
            }
            Ok(Message::Close(frame)) => {
                break End::Closed(frame.map(|f| u16::from(f.code)).unwrap_or(1005));
            }
            Ok(_) => last_rx = Instant::now(),
            Err(e) if would_block(&e) => {}
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                break End::Closed(1006);
            }
            Err(e) => break End::Broken(e.to_string()),
        }
        while let Ok(out) = rx.try_recv() {
            if let Err(e) = ws.send(Message::text(out))
                && !would_block(&e)
            {
                return End::Broken(e.to_string());
            }
        }
        if let Err(e) = ws.flush()
            && !would_block(&e)
        {
            break End::Broken(e.to_string());
        }
        if last_rx.elapsed() > SILENCE_LIMIT {
            break End::Broken("the gateway went silent".into());
        }
        if rt.leave_requested() {
            let _ = ws.close(None);
            let _ = ws.flush();
            break End::Left;
        }
    };
    for flag in cancels.lock().unwrap().values() {
        flag.store(true, Ordering::SeqCst);
    }
    end
}

fn on_frame(
    rt: &Arc<Runtime>,
    text: &str,
    tx: &Sender<String>,
    cancels: &Arc<Mutex<HashMap<i64, Arc<AtomicBool>>>>,
) -> Option<End> {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return None;
    };
    let op = frame.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "welcome" => {
            rt.set_conn(Conn::Online);
            rt.device
                .record(json!({"event": "online", "device": frame.get("device")}));
            return None;
        }
        "error" => {
            return Some(End::Refused(
                frame
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("REFUSED")
                    .to_string(),
            ));
        }
        "revoked" => return Some(End::Revoked),
        _ => {}
    }
    let id = frame.get("id").and_then(Value::as_i64)?;
    match op {
        "cancel" => {
            if let Some(flag) = cancels.lock().unwrap().get(&id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
        "health" => {
            let _ = tx.send(
                json!({"id": id, "value": {"ok": true, "jobs": rt.device.jobs.running()}})
                    .to_string(),
            );
        }
        "events" if frame.get("handle").and_then(Value::as_str) == Some("console") => {
            rt.device.set_console(Some((tx.clone(), id)));
        }
        "rpc" => {
            let flag = Arc::new(AtomicBool::new(false));
            cancels.lock().unwrap().insert(id, flag.clone());
            let (device, tx, cancels) = (rt.device.clone(), tx.clone(), cancels.clone());
            std::thread::spawn(move || {
                let body = frame.get("body").cloned().unwrap_or(Value::Null);
                let request = body.get("request").cloned().unwrap_or(Value::Null);
                let args = body.get("args").cloned().unwrap_or_else(|| json!({}));
                let answer = match device.serve(&request, &args, &flag) {
                    Ok(value) => json!({"id": id, "value": value}),
                    Err(e) => {
                        json!({"id": id, "error": {"code": e.code, "message": crate::util::clip(&e.message, 300)}})
                    }
                };
                cancels.lock().unwrap().remove(&id);
                let _ = tx.send(answer.to_string());
            });
        }
        _ => {
            let _ = tx.send(json!({"id": id, "error": {"code": "UNKNOWN_OPERATION", "message": "unknown operation"}}).to_string());
        }
    }
    None
}

/// Changes made from the command line (`access`, `link`) reach the running app.
pub fn watch_config(rt: Arc<Runtime>) {
    let path = Config::path();
    let stamp = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut seen: Option<std::time::SystemTime> = stamp(&path);
    while !rt.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_secs(1));
        let now = stamp(&path);
        if now == seen {
            continue;
        }
        seen = now;
        let fresh = Config::load();
        let mut cfg = rt.config.lock().unwrap();
        let level_changed = fresh.access != cfg.access || fresh.folders != cfg.folders;
        let gateway_changed = fresh.gateway != cfg.gateway;
        *cfg = fresh;
        if level_changed {
            rt.device.set_access(cfg.access, &cfg.folders);
        }
        drop(cfg);
        if level_changed || gateway_changed {
            rt.reconnect.store(true, Ordering::SeqCst);
        }
    }
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
