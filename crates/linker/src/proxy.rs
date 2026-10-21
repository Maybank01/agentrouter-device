//! The link through an HTTP proxy (issue #85): a cloud sandbox has no direct egress, only its node's proxy
//! (`HTTPS_PROXY`, CONNECT to :443), and some PCs sit behind one too. The WebSocket client does not read proxy
//! variables, so the link opens a CONNECT tunnel itself and runs TLS and the WebSocket handshake over it.
//!
//! The usual conventions (curl's): `HTTPS_PROXY`/`https_proxy` for `wss://`, `HTTP_PROXY`/`http_proxy` for `ws://`,
//! `NO_PROXY`/`no_proxy` (comma list of host suffixes, `*` = none) wins. Only `http://` proxies; credentials in the
//! proxy URL are sent as Basic auth.

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The proxy one connection goes through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    /// `host:port` of the proxy.
    pub addr: String,
    /// `Basic <base64>` when the proxy URL carries credentials.
    pub auth: Option<String>,
    /// `host:port` of the gateway the tunnel goes to.
    pub target: String,
}

/// The proxy for `url` (`ws://` or `wss://`), read through `env`; `None`: connect directly.
pub fn proxy_for(url: &str, env: impl Fn(&str) -> Option<String>) -> Option<Proxy> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else {
        (false, url.strip_prefix("ws://")?)
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h, p.to_string())
        }
        _ => (authority, if secure { "443".into() } else { "80".into() }),
    };
    if host.is_empty() {
        return None;
    }
    let pick = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|n| env(n).filter(|v| !v.trim().is_empty()))
    };
    let raw = if secure {
        pick(["HTTPS_PROXY", "https_proxy"])
    } else {
        pick(["HTTP_PROXY", "http_proxy"])
    }?;
    if let Some(list) = pick(["NO_PROXY", "no_proxy"])
        && bypassed(host, &list)
    {
        return None;
    }
    let raw = raw.trim();
    let rest = raw.strip_prefix("http://").unwrap_or(raw);
    if rest.contains("://") {
        return None; // https:// or socks proxies: not supported, connect directly
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    if hostport.is_empty() {
        return None;
    }
    let addr = if hostport
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let auth = userinfo.map(|u| {
        let (user, pass) = u.split_once(':').unwrap_or((u, ""));
        let pair = format!("{}:{}", percent_decode(user), percent_decode(pass));
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    });
    Some(Proxy {
        addr,
        auth,
        target: format!("{host}:{port}"),
    })
}

/// Whether `NO_PROXY` exempts `host` (exact name or a suffix at a dot; `*` exempts every host).
fn bypassed(host: &str, list: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    list.split(',')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            let entry = entry
                .split(':')
                .next()
                .unwrap_or("")
                .trim_start_matches('.')
                .to_string();
            !entry.is_empty() && (host == entry || host.ends_with(&format!(".{entry}")))
        })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                16,
            )
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Most bytes a proxy's answer to CONNECT may take before its blank line.
const ANSWER_MAX: usize = 16 * 1024;

impl Proxy {
    /// A TCP stream to the gateway through the proxy (CONNECT answered 2xx); the bytes after it are the gateway's.
    pub async fn tunnel(&self) -> std::io::Result<TcpStream> {
        let mut stream = TcpStream::connect(&self.addr).await?;
        let _ = stream.set_nodelay(true);
        let mut ask = format!("CONNECT {t} HTTP/1.1\r\nHost: {t}\r\n", t = self.target);
        if let Some(auth) = &self.auth {
            ask.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
        }
        ask.push_str("\r\n");
        stream.write_all(ask.as_bytes()).await?;
        // Byte by byte up to the blank line: nothing of the tunnel's own traffic may be read here.
        let mut answer = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        while !answer.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "代理在回答前断开了",
                ));
            }
            answer.push(byte[0]);
            if answer.len() > ANSWER_MAX {
                return Err(std::io::Error::other("代理的回答太长"));
            }
        }
        let status = String::from_utf8_lossy(&answer);
        let line = status.lines().next().unwrap_or("");
        let code = line.split_whitespace().nth(1).unwrap_or("");
        if !code.starts_with('2') {
            return Err(std::io::Error::other(format!(
                "代理拒绝了连接：{}",
                line.chars().take(120).collect::<String>()
            )));
        }
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn picks_the_proxy_by_scheme_and_honours_no_proxy() {
        let e = env(&[
            ("HTTPS_PROXY", "http://10.0.0.1:3128"),
            ("NO_PROXY", "127.0.0.1,localhost,.internal"),
        ]);
        let p = proxy_for("wss://agent-gateway-dev.agentrouter.top/device/v2/link", &e).unwrap();
        assert_eq!(
            p,
            Proxy {
                addr: "10.0.0.1:3128".into(),
                auth: None,
                target: "agent-gateway-dev.agentrouter.top:443".into()
            }
        );
        assert!(
            proxy_for("ws://127.0.0.1:8080/device/v2/link", &e).is_none(),
            "ws uses HTTP_PROXY, unset"
        );
        assert!(proxy_for("wss://gw.internal/x", &e).is_none());
        assert!(proxy_for("wss://localhost:9/x", &e).is_none());
        assert!(proxy_for("wss://gw.example.com/x", env(&[])).is_none());
        assert!(
            proxy_for(
                "wss://gw.example.com/x",
                env(&[("https_proxy", "http://p:1"), ("no_proxy", "*")])
            )
            .is_none()
        );
        assert!(
            proxy_for(
                "wss://gw.example.com/x",
                env(&[("https_proxy", "socks5://p:1")])
            )
            .is_none()
        );
    }

    #[test]
    fn credentials_become_basic_auth_and_ports_default() {
        let p = proxy_for(
            "wss://gw.example.com:8443/x",
            env(&[("https_proxy", "http://us%40er:p%3Ass@proxy.local/")]),
        )
        .unwrap();
        assert_eq!(p.addr, "proxy.local:80");
        assert_eq!(p.target, "gw.example.com:8443");
        assert_eq!(
            p.auth.as_deref(),
            Some(
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("us@er:p:ss")
                )
                .as_str()
            )
        );
    }

    #[tokio::test]
    async fn tunnels_through_connect_and_refuses_a_non_2xx() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            for answer in [
                "HTTP/1.1 200 Connection established\r\n\r\n",
                "HTTP/1.1 403 Forbidden\r\nX: y\r\n\r\n",
            ] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut got = Vec::new();
                let mut b = [0u8; 1];
                while !got.ends_with(b"\r\n\r\n") {
                    s.read_exact(&mut b).await.unwrap();
                    got.push(b[0]);
                }
                let text = String::from_utf8(got).unwrap();
                assert!(
                    text.starts_with(
                        "CONNECT gw.example.com:443 HTTP/1.1\r\nHost: gw.example.com:443\r\n"
                    ),
                    "{text}"
                );
                s.write_all(answer.as_bytes()).await.unwrap();
                s.write_all(b"after").await.unwrap();
                if answer.contains("200") {
                    let mut rest = [0u8; 5];
                    s.read_exact(&mut rest).await.unwrap(); // the client's echo
                    assert_eq!(&rest, b"after");
                }
            }
        });
        let proxy = Proxy {
            addr: addr.clone(),
            auth: None,
            target: "gw.example.com:443".into(),
        };
        let mut tunnel = proxy.tunnel().await.unwrap();
        let mut first = [0u8; 5];
        tunnel.read_exact(&mut first).await.unwrap();
        assert_eq!(
            &first, b"after",
            "bytes after the blank line belong to the tunnel"
        );
        tunnel.write_all(&first).await.unwrap();
        let refused = proxy.tunnel().await.unwrap_err();
        assert!(refused.to_string().contains("403"), "{refused}");
        server.await.unwrap();
    }
}
