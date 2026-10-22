//! Preview streams (Cloud #2387, one mechanism with the runnable-space preview of Cloud #2246): the person opens a dev
//! server that runs on this host (`http://localhost:5173`) in the web preview, without an inbound port.
//!
//! The reverse-dial pattern of wstunnel/chisel reverse tunnels, on the link's own code: the gateway asks over the link
//! `{"op":"port_open","id":"pst_…","port":5173,"ticket":"…"}`; `hostd` connects to the port on this host's loopback
//! (`127.0.0.1`, then `::1`) and dials **one WebSocket per connection** to `<gateway>/device/v2/port/<id>` with the
//! one-time ticket (`Authorization: Bearer`). Bytes go both ways as binary messages until either side closes. One
//! WebSocket per connection keeps MCP frames on the link from waiting behind preview bytes and gives each stream its
//! own flow control. A connection that cannot be made is answered on the link: `{"op":"port_error","id","code"}`.
//!
//! **Which ports:** loopback only, never another address. In 完全访问 and in a cloud sandbox every loopback port
//! (the person's own rights, or the sandbox is the boundary). In folder mode the commands run inside the OS sandbox
//! (on Linux its own network namespace, so the dev server's loopback is not this process's): refused with
//! `PORT_SANDBOXED` unless the consumer gives a [`Bridge`]: on Linux the link's host server, which runs in the same
//! sandbox, connects to the port in the sandbox's own loopback and hands the bytes over (device #136, HOSTD-MCP §3.8).

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// The path prefix of a stream's WebSocket on the gateway (`/device/v2/port/<id>`).
pub const PORT_PATH: &str = "/device/v2/port/";
/// Streams open at once per link; one more is answered `PORT_BUSY`.
pub const STREAMS_MAX: usize = 64;
const CONNECT_WAIT: Duration = Duration::from_secs(5);
const DIAL_WAIT: Duration = Duration::from_secs(20);
const CHUNK: usize = 64 << 10;

/// The gateway's request for one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub id: String,
    pub port: u16,
    pub ticket: String,
}

fn hex32(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A `port_open` frame, checked: `pst_` + 32 hex, a port 1–65535, a URL-safe ticket of 16–256 characters.
pub fn parse(frame: &Value) -> Option<Open> {
    let id = frame.get("id")?.as_str()?;
    let port = u16::try_from(frame.get("port")?.as_u64()?)
        .ok()
        .filter(|p| *p > 0)?;
    let ticket = frame.get("ticket")?.as_str()?;
    let ticket_ok = (16..=256).contains(&ticket.len())
        && ticket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (id.strip_prefix("pst_").is_some_and(hex32) && ticket_ok).then(|| Open {
        id: id.to_string(),
        port,
        ticket: ticket.to_string(),
    })
}

/// The stream's WebSocket URL, from the link's (`…/device/v2/link` → `…/device/v2/port/<id>`).
pub fn stream_url(link_url: &str, id: &str) -> Option<String> {
    link_url
        .strip_suffix(crate::PATH)
        .map(|base| format!("{base}{PORT_PATH}{id}"))
}

/// Both directions of one stream's local end (a loopback TCP connection, or a bridge's pipes).
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// Opens one stream's local end for a port inside the sandbox (folder mode): `Err(code)` as for [`serve`]
/// (`PORT_CLOSED`, `PORT_SANDBOXED` when nothing can bridge now).
pub type Bridge = std::sync::Arc<
    dyn Fn(u16) -> futures_util::future::BoxFuture<'static, Result<Box<dyn Duplex>, &'static str>>
        + Send
        + Sync,
>;

/// Whether this link serves preview streams at all (see the module docs).
pub fn allowed(info: &crate::LinkInfo) -> bool {
    info.full || info.placement == "cloud"
}

/// The per-link limit on open streams.
pub fn limit() -> std::sync::Arc<Semaphore> {
    std::sync::Arc::new(Semaphore::new(STREAMS_MAX))
}

async fn connect_loopback(port: u16) -> Option<TcpStream> {
    for addr in [format!("127.0.0.1:{port}"), format!("[::1]:{port}")] {
        if let Ok(Ok(stream)) = tokio::time::timeout(CONNECT_WAIT, TcpStream::connect(&addr)).await
        {
            let _ = stream.set_nodelay(true);
            return Some(stream);
        }
    }
    None
}

/// Serves one stream to its end. `Err(code)` when it could not be opened (`PORT_CLOSED`: nothing listens there;
/// `PORT_DIAL_FAILED`: the gateway's stream endpoint could not be reached). `bridge`: the local end comes from it
/// (folder mode) instead of this process's loopback.
pub async fn serve(url: &str, open: &Open, bridge: Option<&Bridge>) -> Result<(), &'static str> {
    let local: Box<dyn Duplex> = match bridge {
        Some(bridge) => bridge(open.port).await?,
        None => Box::new(connect_loopback(open.port).await.ok_or("PORT_CLOSED")?),
    };
    let mut request = url.into_client_request().map_err(|_| "PORT_DIAL_FAILED")?;
    let auth = HeaderValue::from_str(&format!("Bearer {}", open.ticket))
        .map_err(|_| "PORT_DIAL_FAILED")?;
    request.headers_mut().insert("authorization", auth);
    let config = WebSocketConfig::default()
        .max_message_size(Some(crate::FRAME_MAX))
        .max_frame_size(Some(crate::FRAME_MAX));
    let (ws, _) = tokio::time::timeout(DIAL_WAIT, crate::open(url, request, config))
        .await
        .map_err(|_| "PORT_DIAL_FAILED")?
        .map_err(|_| "PORT_DIAL_FAILED")?;
    let (mut sink, mut stream) = ws.split();
    let (mut rd, mut wr) = tokio::io::split(local);
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::binary(buf[..n].to_vec())).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    };
    let down = async {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(bytes) => {
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    };
    // Either side closing ends the stream (the local server or the person's browser went away).
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "pst_0123456789abcdef0123456789abcdef";

    #[test]
    fn port_open_frames_are_checked() {
        let ok =
            json!({"op": "port_open", "id": ID, "port": 5173, "ticket": "abcdefghijklmnop_-12"});
        assert_eq!(parse(&ok).unwrap().port, 5173);
        for bad in [
            json!({"id": "pst_xyz", "port": 5173, "ticket": "abcdefghijklmnop"}),
            json!({"id": ID, "port": 0, "ticket": "abcdefghijklmnop"}),
            json!({"id": ID, "port": 70000, "ticket": "abcdefghijklmnop"}),
            json!({"id": ID, "port": 5173, "ticket": "short"}),
            json!({"id": ID, "port": 5173, "ticket": "abcdefghijklmnop\r\nX: y"}),
        ] {
            assert!(parse(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_stream_url_sits_next_to_the_link() {
        assert_eq!(
            stream_url("wss://gw.example/device/v2/link", ID).unwrap(),
            format!("wss://gw.example/device/v2/port/{ID}")
        );
        assert!(stream_url("wss://gw.example/other", ID).is_none());
    }

    #[test]
    fn only_full_access_and_cloud_hosts_serve_ports() {
        let mut info = crate::LinkInfo {
            id: "lnk_x".into(),
            folder: "f".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            app: "cli".into(),
            version: "0".into(),
            sandbox: "srt@0".into(),
            read_only: false,
            folders: vec![],
            full: false,
            placement: "pc".into(),
            capacity: serde_json::Value::Null,
            labels: serde_json::Value::Null,
            pool: None,
        };
        assert!(!allowed(&info));
        info.full = true;
        assert!(allowed(&info));
        info.full = false;
        info.placement = "cloud".into();
        assert!(allowed(&info));
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // tungstenite's handshake callback type
    async fn bytes_flow_both_ways_between_the_port_and_the_gateway() {
        use tokio::net::TcpListener;
        // The dev server: answers what it reads, uppercased.
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = server.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n].to_ascii_uppercase()).await.unwrap();
        });
        // The gateway's stream endpoint: checks the ticket, sends a request, reads the answer.
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw = gateway.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (s, _) = gateway.accept().await.unwrap();
            let mut auth = String::new();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                s,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request, res| {
                    auth = req
                        .headers()
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                    assert_eq!(req.uri().path(), format!("{PORT_PATH}{ID}"));
                    Ok(res)
                },
            )
            .await
            .unwrap();
            ws.send(Message::binary(b"get /".to_vec())).await.unwrap();
            let mut got = Vec::new();
            while let Some(Ok(m)) = ws.next().await {
                match m {
                    Message::Binary(b) => got.extend_from_slice(&b),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            (auth, got)
        });
        let open = Open {
            id: ID.into(),
            port,
            ticket: "abcdefghijklmnop".into(),
        };
        let url = stream_url(&format!("ws://{gw}{}", crate::PATH), ID).unwrap();
        serve(&url, &open, None).await.unwrap();
        let (auth, got) = seen.await.unwrap();
        assert_eq!(auth, "Bearer abcdefghijklmnop");
        assert_eq!(got, b"GET /");
    }

    #[tokio::test]
    async fn nothing_listening_is_port_closed() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let open = Open {
            id: ID.into(),
            port,
            ticket: "abcdefghijklmnop".into(),
        };
        assert_eq!(
            serve("ws://127.0.0.1:9/device/v2/port/x", &open, None).await,
            Err("PORT_CLOSED")
        );
    }
}
