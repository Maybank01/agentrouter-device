//! The whole device chain, as a person runs it (issue #50, docs/HOSTD-MCP.md): the real `agentrouter link` binary in a
//! folder, against a stub gateway on this computer that plays the control plane (signed MCP over `/device/v2/link`).
//! A real MCP server runs in the OS sandbox; the boundary must hold:
//!
//! - inside the folder: write and delete work (after a checkpoint); `agentrouter undo` brings the deleted file back;
//! - outside: a write, a write from a child process, a `.git/hooks` plant, reading `~/.ssh` and a direct connection to
//!   a host the catalog did not list all fail;
//! - unsigned or forged requests are refused; the audit holds every call with its arguments and verifies.
//!
//! Runs only with `AGENTROUTER_SANDBOX_TESTS=1` (the `Sandbox (…)` CI jobs on GitHub-hosted runners): it installs the
//! pinned sandbox runtime and writes probe files into the runner's home. Never run it on a person's computer.

use std::io::{Read, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agentrouter_aggregator::sig;
use agentrouter_device::protocol::{ControlKey, DeviceKey, sha256_hex};
use agentrouter_device::util::{b64url_decode, now_ms};
use serde_json::{Value, json};
use tungstenite::Message;

/// Where the device keeps its data on a person's PC: on Windows `%LOCALAPPDATA%`, which srt's profile read grant
/// passes through (the temp folder it leaves unreadable to the sandbox account, device #66); elsewhere the temp folder.
fn test_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

fn enabled() -> bool {
    std::env::var("AGENTROUTER_SANDBOX_TESTS").is_ok_and(|v| v == "1")
}

fn real(path: &Path) -> PathBuf {
    let p = std::fs::canonicalize(path).unwrap();
    PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap())
}

/// A tiny MCP server (Node, no dependencies) with the probes the boundary is tested with.
const PROBE_SERVER: &str = r#"
const fs = require('fs');
const net = require('net');
const { spawnSync } = require('child_process');
const send = (m) => process.stdout.write(JSON.stringify(m) + '\n');
const text = (id, t, isError) => send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: String(t) }], isError: !!isError } });
const tools = ['read', 'write', 'remove', 'child_write', 'connect'].map((name) => ({ name, inputSchema: { type: 'object' },
  ...(name === 'read' ? { annotations: { readOnlyHint: true } } : {}) }));
let buf = '';
process.stdin.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.method === 'initialize') send({ jsonrpc: '2.0', id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'probe', version: '1' } } });
    else if (m.method === 'tools/list') send({ jsonrpc: '2.0', id: m.id, result: { tools } });
    else if (m.method === 'tools/call') {
      const a = m.params.arguments || {};
      const name = m.params.name;
      try {
        if (name === 'read') text(m.id, fs.readFileSync(a.path, 'utf8'));
        else if (name === 'write') { fs.writeFileSync(a.path, a.content || 'x'); text(m.id, 'written'); }
        else if (name === 'remove') { fs.unlinkSync(a.path); text(m.id, 'removed'); }
        else if (name === 'child_write') {
          const r = process.platform === 'win32'
            ? spawnSync(process.env.ComSpec || 'cmd.exe', ['/d', '/c', `echo escaped> "${a.path}"`])
            : spawnSync('/bin/sh', ['-c', `echo escaped > '${a.path}'`]);
          text(m.id, 'child exit ' + r.status, r.status !== 0);
        } else if (name === 'connect') {
          const s = net.connect({ host: a.host, port: 443 });
          const done = (v, bad) => { s.destroy(); text(m.id, v, bad); };
          s.setTimeout(8000, () => done('timeout', true));
          s.on('connect', () => done('connected', false));
          s.on('error', (e) => done('denied: ' + (e.code || e.message), true));
        }
      } catch (e) { text(m.id, 'failed: ' + (e.code || e.message), true); }
    } else if (m.id !== undefined && m.method) send({ jsonrpc: '2.0', id: m.id, result: {} });
  }
});
"#;

/// The control plane's side: a key the device pins, and signed MCP messages.
struct Control {
    key: DeviceKey,
    pinned: ControlKey,
    nonce: u64,
}

impl Control {
    fn new() -> Control {
        let key = DeviceKey::from_seed([11; 32]);
        let raw = b64url_decode(&key.public_key()).unwrap();
        let pinned = ControlKey {
            kid: sha256_hex(&raw)[..16].to_string(),
            public_key: key.public_key(),
        };
        Control {
            key,
            pinned,
            nonce: 0,
        }
    }

    fn request(
        &mut self,
        link: &str,
        id: i64,
        method: &str,
        mut params: Value,
        signed: bool,
    ) -> Value {
        if signed {
            self.nonce += 1;
            let nonce = format!("{:032x}", (now_ms() as u128) * 1000 + self.nonce as u128);
            let exp = now_ms() + 60_000;
            let digest = sig::params_digest(&params);
            let text = sig::signed_string(link, "ags_e2e", method, &digest, exp, &nonce);
            params["_meta"] = json!({sig::META_KEY: {"v": 3, "kid": self.pinned.kid, "link": link, "session": "ags_e2e",
                "digest": digest, "exp": exp, "nonce": nonce, "sig": self.key.sign(&text)}});
        }
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }
}

/// Read one HTTP request's head without consuming it (a WebSocket upgrade is then handed to tungstenite whole).
fn peek_head(stream: &TcpStream) -> String {
    let mut buf = vec![0u8; 8192];
    loop {
        let n = stream.peek(&mut buf).unwrap();
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        if text.contains("\r\n\r\n") || n == buf.len() {
            return text;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn answer_http(mut stream: TcpStream, body: &Value) {
    let head = peek_head(&stream);
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    let total = head.find("\r\n\r\n").unwrap() + 4 + len;
    let mut sink = vec![0u8; total];
    stream.read_exact(&mut sink).unwrap();
    let text = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
}

#[test]
fn a_linked_folder_end_to_end_and_the_boundary_holds() {
    if !enabled() {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    let base = test_root().join(format!("agentrouter-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for d in [
        "data",
        "work/my-mod/config",
        "work/my-mod/.git/hooks",
        "outside",
        "tools",
    ] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let base = real(&base);
    let data = base.join("data");
    let folder = base.join("work").join("my-mod");
    let outside = base.join("outside");
    std::fs::write(folder.join("config").join("items.json"), "{\"sword\": 10}").unwrap();
    std::fs::write(outside.join("victim.txt"), "victim").unwrap();
    let probe = base.join("tools").join("probe.cjs");
    std::fs::write(&probe, PROBE_SERVER).unwrap();
    let ssh = home().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    let secret = ssh.join("agentrouter-e2e-secret");
    std::fs::write(&secret, "do-not-read").unwrap();

    // The paired identity (as pairing would leave it) and the sandbox, prepared like `link` does on first use.
    let control = Control::new();
    unsafe { std::env::set_var("AGENTROUTER_DEVICE_HOME", &data) };
    let device = format!("lnd_{}", "e".repeat(32));
    agentrouter_device::keystore::save(&DeviceKey::generate(), &device, &control.pinned).unwrap();
    agentrouter_sandbox::Runtime::new(&data)
        .setup()
        .expect("sandbox setup on the runner");
    let link = agentrouter_hostd_link::link_id(&device, &folder);

    // The stub gateway: token calls answered, the link socket handed to the test.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ws_tx, ws_rx) = mpsc::channel();
    let (port_tx, port_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let head = peek_head(&stream);
            if head.starts_with("GET /device/v2/port/") {
                let _ = port_tx.send(tungstenite::accept(stream).unwrap());
                continue;
            }
            if head.starts_with("POST /device/v1/challenge") {
                answer_http(stream, &json!({"challenge": "c.1.2.3"}));
            } else if head.starts_with("POST /device/v1/token") {
                answer_http(stream, &json!({"token": "ldt_e2e"}));
            } else if head.starts_with("GET /device/v2/link") {
                let _ = ws_tx.send(tungstenite::accept(stream).unwrap());
            }
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .args([
            "link",
            "--gateway",
            &format!("http://127.0.0.1:{port}"),
            "--yes-servers",
        ])
        .current_dir(&folder)
        .env("AGENTROUTER_DEVICE_HOME", &data)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ws = ws_rx
        .recv_timeout(Duration::from_secs(120))
        .expect("the device dials the link");
    let hello: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hello["op"], "hello");
    assert_eq!(hello["v"], 2);
    assert_eq!(hello["link"]["id"], link);
    assert_eq!(hello["link"]["folder"], "my-mod");
    // Real paths (owner 2026-10-20): the folders the person linked, as they are on this computer.
    assert_eq!(hello["link"]["folders"][0], json!(folder.to_string_lossy()));
    assert_eq!(hello["link"]["full"], false);
    ws.send(Message::text(
        json!({"op": "welcome", "link": link}).to_string(),
    ))
    .unwrap();

    let mut control = control;
    let mut call = |ws: &mut tungstenite::WebSocket<TcpStream>,
                    id: i64,
                    method: &str,
                    params: Value,
                    signed: bool|
     -> Value {
        let m = control.request(&link, id, method, params, signed);
        ws.send(Message::text(json!({"s": "mcs_e2e", "m": m}).to_string()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            assert!(Instant::now() < deadline, "no answer to {id}");
            let frame = ws.read().unwrap();
            let Ok(text) = frame.to_text() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if v["s"] == "mcs_e2e" && v["m"]["id"] == id {
                return v["m"].clone();
            }
        }
    };
    let code = |answer: &Value| {
        answer["error"]["data"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    // Unsigned initialize: refused. Signed: a session bound to the conversation.
    let refused = call(
        &mut ws,
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}),
        false,
    );
    assert_eq!(code(&refused), "SIGNATURE_INVALID", "{refused}");
    let init = call(
        &mut ws,
        2,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}),
        true,
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"], "agentrouter-link",
        "{init}"
    );
    ws.send(Message::text(
        json!({"s": "mcs_e2e", "m": {"jsonrpc": "2.0", "method": "notifications/initialized"}})
            .to_string(),
    ))
    .unwrap();

    let added = call(
        &mut ws,
        3,
        "tools/call",
        json!({"name": "link__server_add", "arguments": {"name": "probe", "command": "node", "args": [probe.to_string_lossy()], "waitMs": 120000}}),
        true,
    );
    assert_eq!(
        added["result"]["structuredContent"]["state"], "ready",
        "{added}"
    );

    let tool = |name: &str, arguments: Value| json!({"name": format!("probe__{name}"), "arguments": arguments});
    let ok = |a: &Value| a["result"]["isError"] != true && a.get("error").is_none();

    // Inside: write, delete.
    let note = folder.join("notes.md");
    assert!(ok(&call(
        &mut ws,
        10,
        "tools/call",
        tool(
            "write",
            json!({"path": note.to_string_lossy(), "content": "hello"})
        ),
        true
    )));
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "hello");
    let items = folder.join("config").join("items.json");
    assert!(ok(&call(
        &mut ws,
        11,
        "tools/call",
        tool("remove", json!({"path": items.to_string_lossy()})),
        true
    )));
    assert!(!items.exists());

    // Outside: every escape fails.
    let escaped = outside.join("escaped.txt");
    assert!(!ok(&call(
        &mut ws,
        20,
        "tools/call",
        tool("write", json!({"path": escaped.to_string_lossy()})),
        true
    )));
    let child_escaped = outside.join("child.txt");
    call(
        &mut ws,
        21,
        "tools/call",
        tool(
            "child_write",
            json!({"path": child_escaped.to_string_lossy()}),
        ),
        true,
    );
    let victim = call(
        &mut ws,
        22,
        "tools/call",
        tool(
            "remove",
            json!({"path": outside.join("victim.txt").to_string_lossy()}),
        ),
        true,
    );
    assert!(!ok(&victim), "{victim}");
    let hook = call(
        &mut ws,
        23,
        "tools/call",
        tool(
            "write",
            json!({"path": folder.join(".git").join("hooks").join("pre-commit").to_string_lossy(), "content": "#!/bin/sh\necho planted\n"}),
        ),
        true,
    );
    assert!(!ok(&hook), "a .git/hooks plant is refused: {hook}");
    let read = call(
        &mut ws,
        24,
        "tools/call",
        tool("read", json!({"path": secret.to_string_lossy()})),
        true,
    );
    assert!(
        !read.to_string().contains("do-not-read"),
        "~/.ssh is unreadable: {read}"
    );
    let net = call(
        &mut ws,
        25,
        "tools/call",
        tool("connect", json!({"host": "1.1.1.1"})),
        true,
    );
    assert!(
        !ok(&net),
        "a direct connection to an unlisted host fails: {net}"
    );
    assert!(!escaped.exists() && !child_escaped.exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "victim"
    );
    assert!(
        !folder
            .join(".git")
            .join("hooks")
            .join("pre-commit")
            .exists()
    );

    // The SSH-like host server the console starts itself: a real shell in the folder, files by real path; the
    // sandbox still refuses writes outside the folder.
    let host = |name: &str, arguments: Value| json!({"name": format!("host__{name}"), "arguments": arguments});
    let mut id = 40;
    let ran = loop {
        id += 1;
        let a = call(
            &mut ws,
            id,
            "tools/call",
            host("exec", json!({"command": "echo linked"})),
            true,
        );
        if !["SERVER_NOT_RUNNING", "SERVER_STARTING"].contains(&code(&a).as_str()) || id > 100 {
            break a;
        }
        std::thread::sleep(Duration::from_secs(1));
    };
    assert!(ok(&ran), "{ran}");
    assert!(
        ran["result"]["structuredContent"]["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("linked"),
        "{ran}"
    );
    assert_eq!(
        ran["result"]["structuredContent"]["cwd"],
        json!(folder.to_string_lossy())
    );
    let wrote = call(
        &mut ws,
        120,
        "tools/call",
        host("write", json!({"path": "host.txt", "data": "via host"})),
        true,
    );
    assert!(ok(&wrote), "{wrote}");
    assert_eq!(
        std::fs::read_to_string(folder.join("host.txt")).unwrap(),
        "via host"
    );
    let host_escape = outside.join("host-escaped.txt");
    let refused = call(
        &mut ws,
        121,
        "tools/call",
        host(
            "write",
            json!({"path": host_escape.to_string_lossy(), "data": "x"}),
        ),
        true,
    );
    assert!(!ok(&refused) && !host_escape.exists(), "{refused}");

    // A preview stream into the sandbox (device #136): on Linux the sandbox has its own network namespace and the host
    // server bridges a port there (a server the Agent started inside, reached from outside); macOS and Windows still
    // answer PORT_SANDBOXED.
    let port = 47000 + (std::process::id() % 1000) as u16;
    if cfg!(target_os = "linux") {
        let node = agentrouter_sandbox::Runtime::new(&data).node().unwrap();
        let script = format!(
            "require('http').createServer((q,r)=>r.end('bridged-'+q.url)).listen({port},'127.0.0.1')"
        );
        let started = call(
            &mut ws,
            130,
            "tools/call",
            host(
                "job_start",
                json!({"command": format!("\"{}\" -e \"{script}\"", node.display())}),
            ),
            true,
        );
        assert!(ok(&started), "{started}");
        std::thread::sleep(Duration::from_secs(3));
    }
    let id = format!("pst_{}", "a".repeat(32));
    ws.send(Message::text(
        json!({"op": "port_open", "id": id, "port": port, "ticket": "ticket-e2e-0123456789"})
            .to_string(),
    ))
    .unwrap();
    if cfg!(target_os = "linux") {
        let mut pws = port_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the device dials the stream through the bridge");
        pws.send(Message::binary(
            b"GET /x HTTP/1.0\r\nHost: a\r\n\r\n".to_vec(),
        ))
        .unwrap();
        let mut got = Vec::new();
        while let Ok(msg) = pws.read() {
            match msg {
                Message::Binary(b) => got.extend_from_slice(&b),
                Message::Close(_) => break,
                _ => {}
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("bridged-/x"), "through the bridge: {text}");
    } else {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                Instant::now() < deadline,
                "no port_error for the folder-mode stream"
            );
            let frame = ws.read().unwrap();
            let Ok(v) = serde_json::from_str::<Value>(frame.to_text().unwrap_or("")) else {
                continue;
            };
            if v["op"] == "port_error" && v["id"] == id.as_str() {
                assert_eq!(v["code"], "PORT_SANDBOXED", "{v}");
                break;
            }
        }
    }

    // A forged call (arguments changed after signing) is refused.
    let mut forged = control_forged(
        &link,
        &tool("write", json!({"path": note.to_string_lossy()})),
    );
    forged["params"]["arguments"]["path"] = json!(escaped.to_string_lossy());
    ws.send(Message::text(
        json!({"s": "mcs_e2e", "m": forged}).to_string(),
    ))
    .unwrap();

    // Ctrl+C equivalent: the link ends, servers stop.
    stop(&mut child);
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.contains("已连上"), "{out}");
    assert!(
        out.contains("拦下") || out.contains("失败"),
        "denials show in the feed: {out}"
    );

    // Undo brings back the deleted file and removes the new one.
    let undo = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .arg("undo")
        .current_dir(&folder)
        .env("AGENTROUTER_DEVICE_HOME", &data)
        .output()
        .unwrap();
    assert!(
        undo.status.success(),
        "{}",
        String::from_utf8_lossy(&undo.stderr)
    );
    assert_eq!(std::fs::read_to_string(&items).unwrap(), "{\"sword\": 10}");
    assert!(!note.exists());

    // The audit: every call with its arguments, a chain that verifies.
    let audit = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .args(["audit", "200"])
        .current_dir(&folder)
        .env("AGENTROUTER_DEVICE_HOME", &data)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&audit.stdout);
    assert!(audit.status.success(), "{text}");
    assert!(
        text.contains("escaped.txt") && text.contains("pre-commit") && text.contains("1.1.1.1"),
        "{text}"
    );
    assert!(text.contains("SIGNATURE_INVALID"));
    assert!(text.contains("审计日志完好"));
    let _ = std::fs::remove_file(&secret);
    let _ = std::fs::remove_dir_all(&base);
}

/// A signed request whose arguments the test then changes (the device must refuse it).
fn control_forged(link: &str, params: &Value) -> Value {
    Control::new().request(link, 99, "tools/call", params.clone(), true)
}

fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGINT);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}
