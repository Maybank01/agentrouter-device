//! hostd's link layer (formerly the Linker; docs/HOSTD-MCP.md §3.1, §3.2, §3.6, issue #45): the stable outbound
//! link of one running `agentrouter link`. It owns the device identity's token, one WebSocket to `wss://<gateway>/device/v2/link`,
//! reconnects with backoff and jitter, treats 75 s of silence as a dead link, and sends the audit chain head with
//! every heartbeat. It has **no capabilities**: it only moves MCP JSON-RPC messages between the gateway and
//! whoever consumes [`Event`]s (the MCP aggregator), inside the two-field envelope `{s, m}`.
//!
//! Frames on the link (that is the whole custom surface; everything inside `m` is MCP):
//!
//! ```text
//! data     {"s": "<mcp session id>", "m": <JSON-RPC 2.0 message, unchanged>}
//! control  device → gateway  {"op": "hello", "v": 2, "token": "…", "link": {id, folder, os, arch, app, version, sandbox}}
//!          gateway → device  {"op": "welcome", "link": "lnk_…"} | {"op": "error", "code", "message"} | {"op": "revoked", "reason"}
//!          either way        {"op": "close", "s": "<session>"}
//!          device → gateway  {"op": "audit_head", "seq": n, "hash": "<sha256>"}
//!          (`link` also carries `folders`, `readOnly`, `full` and `placement`: `pc` | `cloud`, HOSTD-MCP §13.1)
//!          gateway → device  {"op": "port_open", "id": "pst_…", "port": n, "ticket": "…"}   (a preview stream, [`port`])
//!          device → gateway  {"op": "port_error", "id": "pst_…", "code": "PORT_CLOSED" | …}
//! ```
//!
//! Calls in flight when the link drops are not replayed: an answer produced while the link is down is dropped
//! (the gateway already answered `LINK_LOST`). Sessions themselves outlive a drop on the aggregator side.

pub mod port;
pub mod proxy;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// The link protocol version this link layer speaks.
pub const VERSION: i64 = 2;
/// The path of the link endpoint on the gateway.
pub const PATH: &str = "/device/v2/link";
/// Frames are at most 8 MiB (both ways).
pub const FRAME_MAX: usize = 8 << 20;
/// The gateway pings every 20 s; this long without any traffic means the link is gone.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(75);
/// The audit chain head goes to the gateway this often (and right after `welcome`).
pub const HEARTBEAT: Duration = Duration::from_secs(20);
/// The first frame must be `hello`, within the gateway's 10 s; this is how long `welcome` may take.
const WELCOME_WAIT: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Close code the gateway uses when a newer connection of the same link replaced this one.
pub const CLOSE_REPLACED: u16 = 4409;

/// What `hello` (and `info`) tells the cloud about this link: the stable [`link_id`], the first folder's display
/// name and, since 2026-10-20, the folders' real paths (running `link` on them is the person's choice to show them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    /// `lnk_…` ([`link_id`]).
    pub id: String,
    /// The folder's display name (`my-mod`).
    pub folder: String,
    pub os: String,
    pub arch: String,
    /// Always `cli` for the console.
    pub app: String,
    pub version: String,
    /// `srt@0.0.81` when the sandbox is ready, else empty (always empty in 完全访问).
    pub sandbox: String,
    pub read_only: bool,
    /// The linked folders' real absolute paths, the first is home (owner 2026-10-20: real paths, several folders).
    pub folders: Vec<String>,
    /// 完全访问 (`agentrouter link --full`, chosen on the computer only).
    pub full: bool,
    /// Where this `hostd` runs (HOSTD-MCP §13.1): `pc` (the person's computer) or `cloud` (a cloud sandbox,
    /// started by its entrypoint with a platform-issued host token).
    pub placement: String,
}

impl LinkInfo {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "folder": self.folder,
            "os": self.os,
            "arch": self.arch,
            "app": self.app,
            "version": self.version,
            "sandbox": self.sandbox,
            "readOnly": self.read_only,
            "folders": self.folders,
            "full": self.full,
            "placement": self.placement,
        })
    }
}

/// The stable id of one linked folder on one device: `lnk_` + the first 32 hex characters of
/// SHA-256(`agentrouter-link/v1\n<device id>\n<canonical folder path>`). The same folder on the same computer is
/// always the same link; another folder or another computer is another link.
pub fn link_id(device: &str, canonical_folder: &Path) -> String {
    let text = format!(
        "agentrouter-link/v1\n{device}\n{}",
        canonical_folder.to_string_lossy()
    );
    let digest = Sha256::digest(text.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("lnk_{hex}")
}

/// Why a token could not be had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// The web no longer knows this device (revoked or unpaired there).
    Revoked(String),
    /// Anything else (network, gateway down): try again later.
    Retry(String),
}

/// Buys a short-lived token for one connection (a signed challenge; `fetch_token` of the device core).
pub type TokenSource = Arc<dyn Fn() -> Result<String, TokenError> + Send + Sync>;
/// The audit chain head as `{seq, hash}`.
pub type AuditHead = Arc<dyn Fn() -> Value + Send + Sync>;

/// The token of the paired identity in the key store.
pub fn keystore_tokens(gateway: String) -> TokenSource {
    Arc::new(move || {
        let identity = agentrouter_device::keystore::load()
            .ok_or_else(|| TokenError::Revoked("这台电脑还没有链接".into()))?;
        agentrouter_device::net::fetch_token(&gateway, &identity).map_err(|e| {
            if e.code == "DEVICE_REVOKED" {
                TokenError::Revoked(e.to_string())
            } else {
                TokenError::Retry(e.to_string())
            }
        })
    })
}

/// A token the platform issued for this host (a cloud sandbox, HOSTD-MCP §13.1): no pairing, no device key. Read
/// from the file (re-read before every connection, so the platform can rotate it) or, without a file, the fixed value.
pub fn host_token(file: Option<std::path::PathBuf>, value: Option<String>) -> TokenSource {
    Arc::new(move || {
        let token = match &file {
            Some(path) => std::fs::read_to_string(path).map_err(|e| {
                TokenError::Retry(format!("读不到主机令牌 {}：{e}", path.display()))
            })?,
            None => value.clone().unwrap_or_default(),
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err(TokenError::Retry("主机令牌是空的".into()));
        }
        Ok(token)
    })
}

pub struct Options {
    /// `https://agent-gateway.example` (or `http://127.0.0.1:<port>` for local tests).
    pub gateway: String,
    pub info: LinkInfo,
    pub tokens: TokenSource,
    pub audit_head: AuditHead,
}

/// What the link reports to its consumer.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Welcomed by the gateway (every successful (re)connect).
    Up { link: String },
    /// The link is down; the link layer tries again after `retry_in`.
    Down { reason: String, retry_in: Duration },
    /// One MCP message of a session.
    Data { s: String, m: Value },
    /// The cloud ended a session.
    Close { s: String },
    /// The link layer stopped for good (after this, no more events).
    Ended(End),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// [`Handle::stop`] (Ctrl+C).
    Stopped,
    /// The web revoked this device: the identity is no longer valid.
    Revoked(String),
    /// Another `link` for the same folder took over (close code 4409).
    Replaced,
    /// The gateway refused this link for a reason retrying cannot fix (`DEVICE_OUTDATED`, …).
    Refused { code: String, message: String },
}

enum Out {
    Data {
        s: String,
        m: Value,
    },
    Close {
        s: String,
    },
    /// The link's info changed (a folder was added): `{"op":"info"}` with the current info.
    Info,
    /// A preview stream could not be opened ([`port`]).
    PortError {
        id: String,
        code: &'static str,
    },
}

type SharedInfo = std::sync::Arc<std::sync::Mutex<LinkInfo>>;

/// Sends MCP messages into the link; stops it.
#[derive(Clone)]
pub struct Handle {
    out: mpsc::UnboundedSender<Out>,
    stop: watch::Sender<bool>,
    info: SharedInfo,
}

impl Handle {
    /// One MCP message of session `s` to the cloud. Dropped while the link is down (no replay).
    pub fn send(&self, s: &str, m: Value) {
        let _ = self.out.send(Out::Data {
            s: s.to_string(),
            m,
        });
    }

    /// This side ended session `s`.
    pub fn close(&self, s: &str) {
        let _ = self.out.send(Out::Close { s: s.to_string() });
    }

    /// New info for this link (same id): sent now as `{"op":"info"}` and used by every later `hello`.
    pub fn set_info(&self, info: LinkInfo) {
        *self.info.lock().unwrap() = info;
        let _ = self.out.send(Out::Info);
    }

    /// Close the link for good ([`End::Stopped`]).
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

/// Start the link layer on the current tokio runtime.
pub fn start(options: Options) -> (Handle, mpsc::UnboundedReceiver<Event>) {
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let info: SharedInfo = std::sync::Arc::new(std::sync::Mutex::new(options.info.clone()));
    tokio::spawn(run(
        options,
        info.clone(),
        out_rx,
        out_tx.clone(),
        event_tx,
        stop_rx,
    ));
    (
        Handle {
            out: out_tx,
            stop: stop_tx,
            info,
        },
        event_rx,
    )
}

/// The `wss://…/device/v2/link` URL of a gateway (TLS for domains; plain only for this computer).
pub fn link_url(gateway: &str) -> Result<String, String> {
    agentrouter_device::net::gateway_allowed(gateway)?;
    let base = gateway.trim().trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        Ok(format!("wss://{rest}{PATH}"))
    } else if let Some(rest) = base.strip_prefix("http://") {
        Ok(format!("ws://{rest}{PATH}"))
    } else {
        Err(format!("不是 http(s) 网关：{gateway}"))
    }
}

/// `backoff` ± 20 %.
fn jitter(backoff: Duration) -> Duration {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    let unit = f64::from(u16::from_le_bytes(b)) / f64::from(u16::MAX); // 0..1
    backoff.mul_f64(0.8 + 0.4 * unit)
}

/// How one connection ended.
enum Conn {
    /// Dropped after a welcome or before one: reconnect.
    Lost(String),
    /// The gateway said something retrying cannot fix.
    End(End),
}

async fn run(
    options: Options,
    info: SharedInfo,
    mut out_rx: mpsc::UnboundedReceiver<Out>,
    out_tx: mpsc::UnboundedSender<Out>,
    events: mpsc::UnboundedSender<Event>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = BACKOFF_MIN;
    let streams = port::limit();
    let end = loop {
        if *stop.borrow() {
            break End::Stopped;
        }
        let tokens = options.tokens.clone();
        let reason = match tokio::task::spawn_blocking(move || tokens()).await {
            Ok(Ok(token)) => {
                let mut welcomed = false;
                let conn = connection(
                    &options,
                    &info,
                    &token,
                    (&mut out_rx, &out_tx),
                    &streams,
                    &events,
                    &mut stop,
                    &mut welcomed,
                )
                .await;
                if welcomed {
                    backoff = BACKOFF_MIN;
                }
                match conn {
                    Conn::End(end) => break end,
                    Conn::Lost(why) => why,
                }
            }
            Ok(Err(TokenError::Revoked(why))) => break End::Revoked(why),
            Ok(Err(TokenError::Retry(why))) => why,
            Err(e) => e.to_string(),
        };
        // Answers produced while down are dropped: the gateway already answered those calls LINK_LOST.
        while out_rx.try_recv().is_ok() {}
        let wait = jitter(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
        let _ = events.send(Event::Down {
            reason,
            retry_in: wait,
        });
        tokio::select! {
            _ = sleep(wait) => {}
            _ = stop.changed() => {}
        }
    };
    let _ = events.send(Event::Ended(end));
}

/// The WebSocket to the gateway: through the proxy the environment names (proxy.rs; a cloud sandbox's only egress),
/// else directly. `request` is `url` with any headers of its own.
async fn open(
    url: &str,
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    config: WebSocketConfig,
) -> Result<
    (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    tokio_tungstenite::tungstenite::Error,
> {
    #[cfg(any(feature = "native-tls", feature = "rustls"))]
    if let Some(proxy) = proxy::proxy_for(url, |name| std::env::var(name).ok()) {
        let stream = proxy
            .tunnel()
            .await
            .map_err(tokio_tungstenite::tungstenite::Error::Io)?;
        return tokio_tungstenite::client_async_tls_with_config(
            request,
            stream,
            Some(config),
            None,
        )
        .await;
    }
    tokio_tungstenite::connect_async_with_config(request, Some(config), true).await
}

#[allow(clippy::too_many_arguments)]
async fn connection(
    options: &Options,
    info: &SharedInfo,
    token: &str,
    (out_rx, out_tx): (
        &mut mpsc::UnboundedReceiver<Out>,
        &mpsc::UnboundedSender<Out>,
    ),
    streams: &Arc<tokio::sync::Semaphore>,
    events: &mpsc::UnboundedSender<Event>,
    stop: &mut watch::Receiver<bool>,
    welcomed: &mut bool,
) -> Conn {
    let url = match link_url(&options.gateway) {
        Ok(url) => url,
        Err(e) => {
            return Conn::End(End::Refused {
                code: "GATEWAY_NOT_ALLOWED".into(),
                message: e,
            });
        }
    };
    let config = WebSocketConfig::default()
        .max_message_size(Some(FRAME_MAX))
        .max_frame_size(Some(FRAME_MAX));
    let request =
        match tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
            url.as_str(),
        ) {
            Ok(request) => request,
            Err(e) => return Conn::Lost(format!("网关地址不对：{e}")),
        };
    let connect = open(&url, request, config);
    let mut ws = match timeout(Duration::from_secs(20), connect).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => return Conn::Lost(format!("连不上网关：{e}")),
        Err(_) => return Conn::Lost("连网关超时".into()),
    };
    let current = info.lock().unwrap().to_json();
    let hello = json!({"op": "hello", "v": VERSION, "token": token, "link": current});
    if let Err(e) = ws.send(Message::text(hello.to_string())).await {
        return Conn::Lost(format!("hello 没发出去：{e}"));
    }
    let mut last_rx = Instant::now();
    let welcome_by = Instant::now() + WELCOME_WAIT;
    // The first beat goes right after `welcome`; the timer starts one period later.
    let mut heartbeat = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let head = |options: &Options| {
        let h = (options.audit_head)();
        json!({"op": "audit_head", "seq": h.get("seq").cloned().unwrap_or(json!(0)), "hash": h.get("hash").cloned().unwrap_or(json!(""))})
    };
    let result = loop {
        let silence = SILENCE_LIMIT.saturating_sub(last_rx.elapsed());
        tokio::select! {
            frame = ws.next() => {
                last_rx = Instant::now();
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match on_text(text.as_str(), events) {
                            Inbound::None => {}
                            Inbound::Welcome(link) => {
                                // Anything queued before this welcome answers a request of an earlier connection:
                                // the gateway already answered it LINK_LOST, so it is never replayed.
                                while out_rx.try_recv().is_ok() {}
                                *welcomed = true;
                                let _ = events.send(Event::Up { link });
                                if ws.send(Message::text(head(options).to_string())).await.is_err() {
                                    break Conn::Lost("心跳没发出去".into());
                                }
                            }
                            Inbound::End(end) => break Conn::End(end),
                            Inbound::Lost(why) => break Conn::Lost(why),
                            Inbound::PortOpen(open) => {
                                let refuse = |code| { let _ = out_tx.send(Out::PortError { id: open.id.clone(), code }); };
                                if !port::allowed(&info.lock().unwrap()) {
                                    refuse("PORT_SANDBOXED");
                                } else if let (Ok(permit), Some(stream_url)) = (streams.clone().try_acquire_owned(), port::stream_url(&url, &open.id)) {
                                    let out = out_tx.clone();
                                    tokio::spawn(async move {
                                        if let Err(code) = port::serve(&stream_url, &open).await {
                                            let _ = out.send(Out::PortError { id: open.id, code });
                                        }
                                        drop(permit);
                                    });
                                } else {
                                    refuse("PORT_BUSY");
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(1005);
                        if code == CLOSE_REPLACED {
                            break Conn::End(End::Replaced);
                        }
                        let reason = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                        break Conn::Lost(format!("网关关闭了连接（{code}）{reason}"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Conn::Lost(e.to_string()),
                    None => break Conn::Lost("连接断了".into()),
                }
            }
            out = out_rx.recv(), if *welcomed => {
                let Some(out) = out else { break Conn::End(End::Stopped) };
                let frame = match out {
                    Out::Data { s, m } => json!({"s": s, "m": m}),
                    Out::Close { s } => json!({"op": "close", "s": s}),
                    Out::Info => json!({"op": "info", "link": info.lock().unwrap().to_json()}),
                    Out::PortError { id, code } => json!({"op": "port_error", "id": id, "code": code}),
                };
                let text = frame.to_string();
                if text.len() > FRAME_MAX {
                    // Never sent: the aggregator answers TOO_LARGE before it gets here; drop rather than break the link.
                    continue;
                }
                if let Err(e) = ws.send(Message::text(text)).await {
                    break Conn::Lost(e.to_string());
                }
            }
            _ = heartbeat.tick(), if *welcomed => {
                if let Err(e) = ws.send(Message::text(head(options).to_string())).await {
                    break Conn::Lost(e.to_string());
                }
            }
            _ = sleep(silence) => {
                break Conn::Lost("网关太久没有动静".into());
            }
            _ = sleep(welcome_by.saturating_duration_since(Instant::now())), if !*welcomed => {
                break Conn::Lost("网关没有回 welcome".into());
            }
            _ = stop.changed() => {
                let _ = ws.close(None).await;
                break Conn::End(End::Stopped);
            }
        }
    };
    if !matches!(result, Conn::End(End::Stopped)) {
        let _ = ws.close(None).await;
    }
    result
}

enum Inbound {
    None,
    Welcome(String),
    End(End),
    Lost(String),
    PortOpen(port::Open),
}

/// Codes after which retrying is pointless: the link layer ends with [`End::Refused`].
const FINAL_CODES: &[&str] = &[
    "DEVICE_OUTDATED",
    "LINK_DISABLED",
    "GATEWAY_NOT_ALLOWED",
    "SANDBOX_REQUIRED",
];

fn on_text(text: &str, events: &mpsc::UnboundedSender<Event>) -> Inbound {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return Inbound::None;
    };
    let field = |k: &str| {
        frame
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if let (Some(s), Some(m)) = (frame.get("s").and_then(Value::as_str), frame.get("m"))
        && frame.get("op").is_none()
    {
        let _ = events.send(Event::Data {
            s: s.to_string(),
            m: m.clone(),
        });
        return Inbound::None;
    }
    match field("op").as_str() {
        "welcome" => Inbound::Welcome(field("link")),
        "close" => {
            let s = field("s");
            if !s.is_empty() {
                let _ = events.send(Event::Close { s });
            }
            Inbound::None
        }
        "revoked" => Inbound::End(End::Revoked(field("reason"))),
        "port_open" => port::parse(&frame).map_or(Inbound::None, Inbound::PortOpen),
        "error" => {
            let code = field("code");
            let message = field("message");
            if code == "DEVICE_REVOKED" {
                Inbound::End(End::Revoked(message))
            } else if FINAL_CODES.contains(&code.as_str()) {
                Inbound::End(End::Refused { code, message })
            } else {
                Inbound::Lost(format!("网关拒绝了：{code} {message}"))
            }
        }
        _ => Inbound::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_ids_are_stable_and_per_folder() {
        let a = link_id("lnd_1", Path::new("/work/my-mod"));
        assert_eq!(a, link_id("lnd_1", Path::new("/work/my-mod")));
        assert!(a.starts_with("lnk_") && a.len() == 4 + 32, "{a}");
        assert_ne!(a, link_id("lnd_1", Path::new("/work/other")));
        assert_ne!(a, link_id("lnd_2", Path::new("/work/my-mod")));
    }

    #[test]
    fn urls_keep_tls_and_refuse_ip_addresses() {
        assert_eq!(
            link_url("https://agent-gateway-dev.agentrouter.top/").unwrap(),
            "wss://agent-gateway-dev.agentrouter.top/device/v2/link"
        );
        assert_eq!(
            link_url("http://127.0.0.1:8081").unwrap(),
            "ws://127.0.0.1:8081/device/v2/link"
        );
        assert!(link_url("https://203.0.113.7").is_err());
        assert!(link_url("http://agent-gateway-dev.agentrouter.top").is_err());
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..200 {
            let j = jitter(Duration::from_secs(10));
            assert!(
                j >= Duration::from_secs(8) && j <= Duration::from_secs(12),
                "{j:?}"
            );
        }
    }

    #[test]
    fn frames_demux_into_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(matches!(
            on_text(
                r#"{"s":"mcs_1","m":{"jsonrpc":"2.0","id":1,"method":"tools/list"}}"#,
                &tx
            ),
            Inbound::None
        ));
        assert_eq!(
            rx.try_recv().unwrap(),
            Event::Data {
                s: "mcs_1".into(),
                m: json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
            }
        );
        assert!(matches!(
            on_text(r#"{"op":"close","s":"mcs_1"}"#, &tx),
            Inbound::None
        ));
        assert_eq!(rx.try_recv().unwrap(), Event::Close { s: "mcs_1".into() });
        assert!(matches!(
            on_text(r#"{"op":"welcome","link":"lnk_x"}"#, &tx),
            Inbound::Welcome(l) if l == "lnk_x"
        ));
        assert!(matches!(
            on_text(
                r#"{"op":"error","code":"DEVICE_OUTDATED","message":"update"}"#,
                &tx
            ),
            Inbound::End(End::Refused { .. })
        ));
        assert!(matches!(
            on_text(r#"{"op":"error","code":"DEVICE_REVOKED"}"#, &tx),
            Inbound::End(End::Revoked(_))
        ));
        assert!(matches!(
            on_text(r#"{"op":"error","code":"BUSY"}"#, &tx),
            Inbound::Lost(_)
        ));
        assert!(matches!(on_text("not json", &tx), Inbound::None));
    }
}
