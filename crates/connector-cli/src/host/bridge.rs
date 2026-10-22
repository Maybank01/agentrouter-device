//! Preview streams into the sandbox (device #136, HOSTD-MCP §3.8). On Linux folder mode the sandbox has its own
//! network namespace, so a dev server the Agent started listens on the sandbox's loopback, which `hostd` cannot reach.
//! This server runs in that same sandbox: `_port_bridge {port, input, output}` connects to the port here and moves the
//! bytes over two FIFOs `hostd` made in this server's scratch folder (no sockets: seccomp refuses `AF_UNIX`, and the
//! namespace has no route out). `hostd` relays them to the stream's WebSocket. The tool is `hostd`'s own: the
//! aggregator lists no tool whose name starts with `_`.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Fail, Host, arg_str, tool};

const CONNECT_WAIT: Duration = Duration::from_secs(5);

pub(super) fn tools() -> Vec<Value> {
    vec![tool(
        "_port_bridge",
        "hostd only (not for Agents): bridge one preview stream to a port in this sandbox's loopback over two FIFOs.",
        json!({"type": "object", "properties": {
            "port": {"type": "integer", "minimum": 1, "maximum": 65535},
            "input": {"type": "string", "description": "FIFO hostd writes the client's bytes to"},
            "output": {"type": "string", "description": "FIFO this server writes the port's bytes to"},
        }, "required": ["port", "input", "output"]}),
        true,
    )]
}

fn fifo(path: &str) -> Result<PathBuf, Fail> {
    use std::os::unix::fs::FileTypeExt;
    let p = PathBuf::from(path);
    match std::fs::symlink_metadata(&p) {
        Ok(m) if m.file_type().is_fifo() => Ok(p),
        _ => Err(format!("{path} 不是 hostd 建的管道").into()),
    }
}

impl Host {
    pub(super) fn port_bridge(&self, args: &Value) -> Result<Value, Fail> {
        let port = args
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| Fail::from("port 要是 1–65535".to_string()))?;
        let input = fifo(arg_str(args, "input").unwrap_or(""))?;
        let output = fifo(arg_str(args, "output").unwrap_or(""))?;
        let tcp = ["127.0.0.1", "::1"]
            .iter()
            .find_map(|ip| {
                let addr = std::net::SocketAddr::new(ip.parse().ok()?, port);
                TcpStream::connect_timeout(&addr, CONNECT_WAIT).ok()
            })
            .ok_or_else(|| {
                Fail::coded(
                    "PORT_CLOSED",
                    format!("这台电脑的隔离环境里没有程序在端口 {port} 上监听"),
                    json!({"port": port}),
                )
            })?;
        let _ = tcp.set_nodelay(true);
        // The FIFOs open once hostd has its ends open (it does right after this answer); then both names go.
        std::thread::Builder::new()
            .name(format!("port-{port}"))
            .spawn(move || {
                let (Ok(mut to_hostd), Ok(mut from_hostd)) = (
                    std::fs::OpenOptions::new().write(true).open(&output),
                    std::fs::File::open(&input),
                ) else {
                    return;
                };
                let _ = std::fs::remove_file(&output);
                let _ = std::fs::remove_file(&input);
                let Ok(mut tcp_in) = tcp.try_clone() else {
                    return;
                };
                let mut tcp_out = tcp;
                // Client → port: until hostd closes its end (the browser went away), then the port sees the end.
                let down = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut from_hostd, &mut tcp_out);
                    let _ = tcp_out.shutdown(Shutdown::Write);
                });
                let mut buf = vec![0u8; 64 << 10];
                loop {
                    match tcp_in.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if to_hostd.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                drop(to_hostd);
                let _ = tcp_in.shutdown(Shutdown::Both);
                let _ = down.join();
            })
            .map_err(|e| format!("开不了转发线程：{e}"))?;
        Ok(json!({"connected": port}))
    }
}
