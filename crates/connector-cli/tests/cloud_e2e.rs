//! `hostd` in a cloud sandbox (issue #82, docs/HOSTD-MCP.md §13.1), as the sandbox entrypoint starts it: the real
//! binary with only a platform-issued host token, the control plane's key and a mounted folder, against a stub gateway
//! on this computer. It must dial `/device/v2/link` without pairing (no `/device/v1/*` call), announce
//! `placement: "cloud"`, answer `host__exec` streamed and `host__write` with versions, refuse folder adds, never
//! prompt, and keep the host token out of the commands it runs.
//!
//! Needs no OS sandbox, so it runs in the plain `cargo test` of the Linux job (Unix shell commands).
#![cfg(unix)]

use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agentrouter_aggregator::sig;
use agentrouter_device::protocol::{DeviceKey, sha256_hex};
use agentrouter_device::util::{b64url_decode, now_ms};
use serde_json::{Value, json};
use tungstenite::Message;

const SESSION: &str = "mcs_cloud";

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

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

struct Cloud {
    ws: tungstenite::WebSocket<TcpStream>,
    key: DeviceKey,
    kid: String,
    link: String,
    nonce: u64,
    next_id: i64,
}

impl Cloud {
    /// One signed request; returns the answer and the notifications that came before it.
    fn call(&mut self, method: &str, mut params: Value, meta: Value) -> (Value, Vec<Value>) {
        self.nonce += 1;
        self.next_id += 1;
        let id = self.next_id;
        let nonce = format!("{:032x}", (now_ms() as u128) * 1000 + self.nonce as u128);
        let exp = now_ms() + 60_000;
        let digest = sig::params_digest(&params);
        let text = sig::signed_string(&self.link, "ags_cloud", method, &digest, exp, &nonce);
        let mut m = meta.as_object().cloned().unwrap_or_default();
        m.insert(
            sig::META_KEY.into(),
            json!({"v": 3, "kid": self.kid, "link": self.link, "session": "ags_cloud", "digest": digest,
                   "exp": exp, "nonce": nonce, "sig": self.key.sign(&text)}),
        );
        params["_meta"] = Value::Object(m);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.ws
            .send(Message::text(json!({"s": SESSION, "m": msg}).to_string()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut notes = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "no answer to {method}");
            let frame = self.ws.read().unwrap();
            let Ok(text) = frame.to_text() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if v["s"] != SESSION {
                continue;
            }
            if v["m"]["id"] == id {
                return (v["m"].clone(), notes);
            }
            if v["m"].get("method").is_some() {
                notes.push(v["m"].clone());
            }
        }
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        self.call(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            json!({}),
        )
        .0
    }
}

#[test]
fn hostd_in_a_cloud_sandbox() {
    let base = std::env::temp_dir().join(format!("agentrouter-cloud-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for d in ["data", "workspace/site", "other"] {
        std::fs::create_dir_all(base.join(d)).unwrap();
    }
    let base = real(&base);
    let data = base.join("data");
    let folder = base.join("workspace").join("site");
    let token_file = base.join("host-token");
    std::fs::write(&token_file, "hst_from_file\n").unwrap();

    let control = DeviceKey::from_seed([13; 32]);
    let raw = b64url_decode(&control.public_key()).unwrap();
    let kid = sha256_hex(&raw)[..16].to_string();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ws_tx, ws_rx) = mpsc::channel();
    let (other_tx, other_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let head = peek_head(&stream);
            if head.starts_with("GET /device/v2/link") {
                let _ = ws_tx.send(tungstenite::accept(stream).unwrap());
            } else {
                let _ = other_tx.send(head.lines().next().unwrap_or("").to_string());
            }
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .args([
            "link",
            "--placement",
            "cloud",
            &folder.to_string_lossy(),
            "--gateway",
            &format!("http://127.0.0.1:{port}"),
        ])
        .current_dir(&base)
        .env("AGENTROUTER_DEVICE_HOME", &data)
        .env("AGENTROUTER_HOST_TOKEN_FILE", &token_file)
        .env("AGENTROUTER_HOST_TOKEN", "hst_from_env_secret")
        .env("AGENTROUTER_CONTROL_KEY", control.public_key())
        .env("AGENTROUTER_HOST_ID", "sbx_e2e")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut ws = ws_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("hostd dials the link");
    let hello: Value = serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hello["op"], "hello");
    assert_eq!(hello["token"], "hst_from_file", "the token file wins");
    assert_eq!(hello["link"]["placement"], "cloud");
    assert_eq!(hello["link"]["app"], "hostd");
    assert_eq!(hello["link"]["full"], true);
    assert_eq!(hello["link"]["sandbox"], "");
    assert_eq!(hello["link"]["folders"][0], json!(folder.to_string_lossy()));
    let link = hello["link"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        link,
        agentrouter_hostd_link::link_id("cloud:sbx_e2e", &folder),
        "stable per sandbox and folder"
    );
    assert!(
        other_rx.try_recv().is_err(),
        "no pairing or token exchange in the cloud"
    );
    ws.send(Message::text(
        json!({"op": "welcome", "link": link}).to_string(),
    ))
    .unwrap();

    let mut cloud = Cloud {
        ws,
        key: control,
        kid,
        link,
        nonce: 0,
        next_id: 0,
    };
    let (init, _) = cloud.call(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}),
        json!({}),
    );
    assert_eq!(
        init["result"]["serverInfo"]["name"], "agentrouter-link",
        "{init}"
    );
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap_or("")
            .contains("cloud sandbox"),
        "{init}"
    );
    cloud
        .ws
        .send(Message::text(
            json!({"s": SESSION, "m": {"jsonrpc": "2.0", "method": "notifications/initialized"}})
                .to_string(),
        ))
        .unwrap();

    // The host server comes up on its own (no `y`, no OS sandbox).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (tools, _) = cloud.call("tools/list", json!({}), json!({}));
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .map(|t| t.iter().filter_map(|t| t["name"].as_str()).collect())
            .unwrap_or_default();
        if names.contains(&"host__exec") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "host tools never appeared: {tools}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // host__exec, streamed; the host token is not in the command's environment.
    let (exec, notes) = cloud.call(
        "tools/call",
        json!({"name": "host__exec", "arguments": {"command": "echo streamed-$((40+2)); echo token=[${AGENTROUTER_HOST_TOKEN:-}]"}}),
        json!({"progressToken": "p1"}),
    );
    let out = &exec["result"]["structuredContent"];
    assert_eq!(out["exit_code"], 0, "{exec}");
    let stdout = out["stdout"].as_str().unwrap_or("");
    assert!(stdout.contains("streamed-42"), "{exec}");
    assert!(stdout.contains("token=[]"), "token leaked: {exec}");
    assert!(
        notes.iter().any(|n| n["method"] == "notifications/progress"
            && n["params"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("streamed-42")),
        "streamed: {notes:?}"
    );
    assert_eq!(out["cwd"], json!(folder.to_string_lossy()));

    // host__write with versions: a guarded write against a stale version writes nothing.
    let page = folder.join("index.html");
    let first = cloud.tool(
        "host__write",
        json!({"path": page.to_string_lossy(), "data": "<h1>v1</h1>", "expected": {"absent": true}}),
    );
    let version = first["result"]["structuredContent"]["version"]
        .as_str()
        .unwrap_or_else(|| panic!("{first}"))
        .to_string();
    let stale = cloud.tool(
        "host__write",
        json!({"path": page.to_string_lossy(), "data": "<h1>lost</h1>", "expected": {"version": "stale"}}),
    );
    assert_eq!(stale["result"]["isError"], true, "{stale}");
    assert_eq!(
        stale["result"]["structuredContent"]["code"], "STALE_VERSION",
        "{stale}"
    );
    let second = cloud.tool(
        "host__write",
        json!({"path": page.to_string_lossy(), "data": "<h1>v2</h1>", "expected": {"version": version}}),
    );
    assert_ne!(second["result"]["isError"], true, "{second}");
    assert_eq!(std::fs::read_to_string(&page).unwrap(), "<h1>v2</h1>");

    // The folders are the mounted views: no more, and nobody is asked.
    let added = cloud.tool(
        "link__folder_add",
        json!({"path": base.join("other").to_string_lossy(), "reason": "e2e"}),
    );
    assert_eq!(added["error"]["data"]["code"], "DENIED_BY_USER", "{added}");

    // No checkpoints in the cloud: nothing under the data folder's shadow repositories.
    assert!(!data.join("checkpoints").exists());

    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.contains("link up"), "{out}");
    assert!(!out.contains("y/N"), "never prompts: {out}");
    let _ = std::fs::remove_dir_all(&base);
}
