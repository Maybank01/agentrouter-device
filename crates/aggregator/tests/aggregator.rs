//! The aggregator as the cloud sees it through the link: signed `initialize` binds a session, unsigned or forged
//! requests are refused (and audited with their arguments), custom servers need the person's y, and nothing runs
//! without the sandbox. With `AGENTROUTER_SANDBOX_TESTS=1` (the `Sandbox (…)` CI jobs only) the whole chain runs:
//! a real MCP server inside the sandbox, tools listed as `<server>__<tool>`, a write inside the folder works (after a
//! checkpoint), a write outside it fails in the kernel, `~/.ssh` is unreadable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentrouter_aggregator::{
    Aggregator, BoxFuture, Config, Feed, Hooks, audit_path, servers::ServerDef, sig,
};
use agentrouter_device::protocol::{ControlKey, DeviceKey, sha256_hex};
use agentrouter_device::util::{b64url_decode, now_ms};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Where the device keeps its data on a person's PC: on Windows `%LOCALAPPDATA%`, which srt's profile read grant
/// passes through (the temp folder it leaves unreadable to the sandbox account, device #66); elsewhere the temp folder.
fn test_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

const LINK: &str = "lnk_00000000000000000000000000000001";

#[derive(Default)]
struct TestHooks {
    feed: Mutex<Vec<Feed>>,
    checkpoints: AtomicUsize,
    yes: bool,
}

impl Hooks for TestHooks {
    fn feed(&self, event: Feed) {
        self.feed.lock().unwrap().push(event);
    }
    fn confirm_server(&self, _def: &ServerDef) -> BoxFuture<bool> {
        let yes = self.yes;
        Box::pin(async move { yes })
    }
    fn checkpoint(&self, _label: &str) -> Option<String> {
        let n = self.checkpoints.fetch_add(1, Ordering::SeqCst) + 1;
        Some(format!("cp{n}"))
    }
}

struct Harness {
    agg: Aggregator,
    rx: mpsc::UnboundedReceiver<(String, Value)>,
    key: DeviceKey,
    control: ControlKey,
    hooks: Arc<TestHooks>,
    nonce: u64,
    data: PathBuf,
}

fn control_of(key: &DeviceKey) -> ControlKey {
    let raw = b64url_decode(&key.public_key()).unwrap();
    ControlKey {
        kid: sha256_hex(&raw)[..16].to_string(),
        public_key: key.public_key(),
    }
}

impl Harness {
    fn new(dir: &Path, folder: &Path, hooks: TestHooks, yes_servers: bool) -> Harness {
        let key = DeviceKey::from_seed([9; 32]);
        let control = control_of(&key);
        let (tx, rx) = mpsc::unbounded_channel();
        let hooks = Arc::new(hooks);
        let data = dir.join("data");
        let agg = Aggregator::new(
            Config {
                link: LINK.into(),
                folders: vec![folder.to_path_buf()],
                folder_name: "work".into(),
                read_only: false,
                full: false,
                data_dir: data.clone(),
                control: control.clone(),
                yes_servers,
                cloud: false,
                limits: Default::default(),
                // CI builds the CLI first (Sandbox jobs): Windows folder mode joins the limits' job with it (#129).
                device_program: std::env::var_os("AGENTROUTER_DEVICE_EXE").map(PathBuf::from),
            },
            hooks.clone(),
            Arc::new(move |s: &str, m: Value| {
                let _ = tx.send((s.to_string(), m));
            }),
        );
        Harness {
            agg,
            rx,
            key,
            control,
            hooks,
            nonce: 0,
            data,
        }
    }

    fn sign(&mut self, method: &str, mut params: Value, session: &str) -> Value {
        self.nonce += 1;
        let nonce = format!("{:032x}", self.nonce + (now_ms() as u64) * 1000);
        let exp = now_ms() + 60_000;
        let digest = sig::params_digest(&params);
        let text = sig::signed_string(LINK, session, method, &digest, exp, &nonce);
        let signature = self.key.sign(&text);
        params["_meta"] = json!({sig::META_KEY: {"v": 3, "kid": self.control.kid, "link": LINK, "session": session,
            "digest": digest, "exp": exp, "nonce": nonce, "sig": signature}});
        params
    }

    async fn answer(&mut self, s: &str, id: i64) -> Value {
        loop {
            let (got_s, m) = timeout(Duration::from_secs(240), self.rx.recv())
                .await
                .expect("an answer in time")
                .expect("the aggregator is running");
            if got_s == s && m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                return m;
            }
        }
    }

    async fn request(&mut self, s: &str, id: i64, method: &str, params: Value) -> Value {
        self.agg.on_message(
            s,
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        self.answer(s, id).await
    }

    async fn open(&mut self, s: &str, conversation: &str) -> Value {
        let params = self.sign(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"},
                   "_meta": {"agentrouter.top/title": "改 mod 配置"}}),
            conversation,
        );
        // The title lives in _meta beside the signature (unsigned): put it back after signing.
        let mut params = params;
        params["_meta"]["agentrouter.top/title"] = json!("改 mod 配置");
        let answer = self.request(s, 1, "initialize", params).await;
        self.agg.on_message(
            s,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        answer
    }

    async fn call(
        &mut self,
        s: &str,
        id: i64,
        conversation: &str,
        name: &str,
        arguments: Value,
    ) -> Value {
        let params = self.sign(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            conversation,
        );
        self.request(s, id, "tools/call", params).await
    }

    fn audit(&self) -> String {
        std::fs::read_to_string(audit_path(&self.data, LINK)).unwrap_or_default()
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("aggregator-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("work")).unwrap();
    dir
}

fn code_of(answer: &Value) -> &str {
    answer["error"]["data"]["code"].as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_protocol_gate_audit_and_no_unconfined_fallback() {
    let dir = scratch("gate");
    let folder = dir.join("work");
    let mut h = Harness::new(&dir, &folder, TestHooks::default(), false);

    // An unsigned initialize is refused; nothing else works without a session.
    let refused = h
        .request("mcs_1", 1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "x", "version": "1"}}))
        .await;
    assert_eq!(code_of(&refused), "SIGNATURE_INVALID", "{refused}");
    let listed = h.request("mcs_1", 2, "tools/list", json!({})).await;
    assert_eq!(code_of(&listed), "SESSION_UNKNOWN", "{listed}");

    // A signed initialize binds the session to the conversation.
    let init = h.open("mcs_1", "ags_a").await;
    assert_eq!(
        init["result"]["serverInfo"]["name"], "agentrouter-link",
        "{init}"
    );
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // Without a sandbox there are only the management tools.
    let listed = h.request("mcs_1", 3, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "link__servers",
            "link__server_add",
            "link__server_remove",
            "link__server_logs",
            "link__folder_add",
            "link__limits",
            "link__feeds",
            "link__package_put"
        ]
    );

    // tools/call: unsigned, signed for another conversation, replayed, tampered — all refused.
    let unsigned = h
        .request(
            "mcs_1",
            4,
            "tools/call",
            json!({"name": "files__write_file", "arguments": {"path": "a.txt", "content": "x"}}),
        )
        .await;
    assert_eq!(code_of(&unsigned), "SIGNATURE_INVALID");
    let other = h
        .call("mcs_1", 5, "ags_other", "link__servers", json!({}))
        .await;
    assert_eq!(code_of(&other), "SIGNATURE_INVALID", "{other}");
    let params = h.sign(
        "tools/call",
        json!({"name": "link__servers", "arguments": {}}),
        "ags_a",
    );
    let first = h.request("mcs_1", 6, "tools/call", params.clone()).await;
    assert!(first["result"].is_object(), "{first}");
    let again = h.request("mcs_1", 7, "tools/call", params).await;
    assert_eq!(code_of(&again), "REPLAYED");
    let mut tampered = h.sign(
        "tools/call",
        json!({"name": "files__write_file", "arguments": {"path": "a.txt"}}),
        "ags_a",
    );
    tampered["arguments"]["path"] = json!("../../.ssh/authorized_keys");
    let tampered = h.request("mcs_1", 8, "tools/call", tampered).await;
    assert_eq!(code_of(&tampered), "SIGNATURE_INVALID");

    // A server that is not running; a custom server the person declines; a preset without the sandbox.
    let missing = h
        .call(
            "mcs_1",
            9,
            "ags_a",
            "files__read_file",
            json!({"path": "a.txt"}),
        )
        .await;
    assert_eq!(code_of(&missing), "SERVER_NOT_RUNNING", "{missing}");
    let custom = h
        .call(
            "mcs_1",
            10,
            "ags_a",
            "link__server_add",
            json!({"name": "mine", "command": "node", "args": ["server.js"]}),
        )
        .await;
    assert_eq!(code_of(&custom), "DENIED_BY_USER", "{custom}");
    let reserved = h
        .call(
            "mcs_1",
            11,
            "ags_a",
            "link__server_add",
            json!({"name": "mine", "command": "node", "env": {"AGENTROUTER_CONSENT": "allow"}}),
        )
        .await;
    assert_eq!(code_of(&reserved), "ENV_RESERVED", "{reserved}");
    // The screen (device #97): a catalog preset that asks for the display is asked too; the cloud cannot grant it.
    let display = h
        .call("mcs_1", 13, "ags_a", "link__server_add", json!({
            "name": "screen", "command": "npx", "args": ["-y", "-p", "{{package}}", "screen-mcp"], "display": true,
            "preset": "screen", "pin": {"file": "screen-1.0.0.tgz", "digest": "sha512-2fN6qhpsiPSBAe0ev1qDPk4sCWdULCZQTuYc4ZY7+BDWQ1XCxLB+7dqStF2ZflgG4G/9vTwQsjxh7pjFqrrpcg==", "package": "screen-mcp", "version": "1.0.0"},
        }))
        .await;
    assert_eq!(code_of(&display), "DENIED_BY_USER", "{display}");
    assert!(
        display["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("屏幕"),
        "{display}"
    );
    let preset = h
        .call("mcs_1", 12, "ags_a", "link__server_add", json!({
            "name": "files", "command": "npx", "args": ["-y", "-p", "{{package}}", "mcp-server-filesystem", "{{folder}}"],
            "preset": "files", "pin": {"file": "server-filesystem-1.0.0.tgz", "digest": "sha512-2fN6qhpsiPSBAe0ev1qDPk4sCWdULCZQTuYc4ZY7+BDWQ1XCxLB+7dqStF2ZflgG4G/9vTwQsjxh7pjFqrrpcg==", "package": "@modelcontextprotocol/server-filesystem", "version": "1.0.0"},
            "waitMs": 5000,
        }))
        .await;
    assert_eq!(
        code_of(&preset),
        "SANDBOX_UNAVAILABLE",
        "nothing runs unconfined: {preset}"
    );
    assert!(
        preset["error"]["data"]["next"]
            .as_str()
            .unwrap()
            .contains("agentrouter sandbox setup")
    );

    // The audit has every request with its arguments (and the refusals).
    let audit = h.audit();
    assert!(audit.contains("\"decision\":\"denied\""));
    assert!(
        audit.contains("../../.ssh/authorized_keys"),
        "tampered arguments are recorded"
    );
    assert!(audit.contains("\"session\":\"ags_a\""));
    assert!(
        audit.contains("\"AGENTROUTER_CONSENT\":\"[secret]\""),
        "env values never: {audit}"
    );
    assert_eq!(
        agentrouter_device::audit::verify(&audit_path(&h.data, LINK)).map(|n| n > 5),
        Ok(true)
    );
    let head = h.agg.audit_head();
    assert!(head["seq"].as_u64().unwrap() > 5);

    // A closed session is gone.
    h.agg.on_close("mcs_1");
    let after = h.request("mcs_1", 13, "tools/list", json!({})).await;
    assert_eq!(code_of(&after), "SESSION_UNKNOWN");
    h.agg.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A minimal stdio MCP server (Node, no dependencies): read/write a file, and a read-only tool.
const FAKE: &str = r#"
const fs = require('fs');
const send = (m) => process.stdout.write(JSON.stringify(m) + '\n');
const text = (id, t, isError) => send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: String(t) }], isError: !!isError } });
let buf = '';
process.stdin.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.method === 'initialize') send({ jsonrpc: '2.0', id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '1' } } });
    else if (m.method === 'tools/list') send({ jsonrpc: '2.0', id: m.id, result: { tools: [
      { name: 'read', inputSchema: { type: 'object' }, annotations: { readOnlyHint: true } },
      { name: 'write', description: 'Write a file.', inputSchema: { type: 'object' } },
      { name: '_hidden', inputSchema: { type: 'object' } },
    ] } });
    else if (m.method === 'tools/call') {
      const a = m.params.arguments || {};
      try {
        if (m.params.name === 'read') text(m.id, fs.readFileSync(a.path, 'utf8'));
        else { fs.writeFileSync(a.path, a.content || 'x'); text(m.id, 'written'); }
      } catch (e) { text(m.id, 'failed: ' + (e.code || e.message), true); }
    } else if (m.id !== undefined && m.method) send({ jsonrpc: '2.0', id: m.id, result: {} });
  }
});
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_confined_server_end_to_end() {
    if std::env::var("AGENTROUTER_SANDBOX_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    let base = test_root().join(format!("agentrouter-agg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("work")).unwrap();
    std::fs::create_dir_all(base.join("outside")).unwrap();
    std::fs::create_dir_all(base.join("tools")).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let base = PathBuf::from(
        base.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    let folder = base.join("work");
    let fake = base.join("tools").join("fake.cjs");
    std::fs::write(&fake, FAKE).unwrap();
    let mut h = Harness::new(&base, &folder, TestHooks::default(), true);
    let rt = agentrouter_sandbox::Runtime::new(&h.data);
    rt.setup().expect("sandbox setup on the runner");

    h.open("mcs_1", "ags_a").await;
    let added = h
        .call("mcs_1", 2, "ags_a", "link__server_add", json!({"name": "fake", "command": "node", "args": [fake.to_string_lossy()], "waitMs": 120000}))
        .await;
    if added["result"]["structuredContent"]["state"] != "ready" {
        let logs = h
            .call(
                "mcs_1",
                3,
                "ags_a",
                "link__server_logs",
                json!({"name": "fake"}),
            )
            .await;
        panic!("not ready: {added}\nlogs: {logs}");
    }
    let listed = h.request("mcs_1", 3, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"fake__read") && names.contains(&"fake__write"),
        "{names:?}"
    );

    let inside = folder.join("inside.txt");
    let ok = h
        .call(
            "mcs_1",
            4,
            "ags_a",
            "fake__write",
            json!({"path": inside.to_string_lossy(), "content": "hello"}),
        )
        .await;
    assert_eq!(ok["result"]["content"][0]["text"], "written", "{ok}");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hello");
    assert_eq!(
        h.hooks.checkpoints.load(Ordering::SeqCst),
        1,
        "a checkpoint before the first write of the turn"
    );

    let outside = base.join("outside").join("escaped.txt");
    let denied = h
        .call(
            "mcs_1",
            5,
            "ags_a",
            "fake__write",
            json!({"path": outside.to_string_lossy(), "content": "escaped"}),
        )
        .await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(!outside.exists(), "a write outside the folder never lands");

    let read = h
        .call(
            "mcs_1",
            6,
            "ags_a",
            "fake__read",
            json!({"path": inside.to_string_lossy()}),
        )
        .await;
    assert_eq!(read["result"]["content"][0]["text"], "hello");
    assert_eq!(
        h.hooks.checkpoints.load(Ordering::SeqCst),
        1,
        "read-only tools take no checkpoint"
    );

    // The environment's limits (device #127): set live on the running server, no restart; bad values are named. On
    // Windows the confined server's entry joined the named job hostd made before the start (device #129).
    let set = h
        .call(
            "mcs_1",
            30,
            "ags_a",
            "link__limits",
            json!({"memoryMb": 1024, "processes": 256, "cpuPercent": null}),
        )
        .await;
    eprintln!("link__limits: {set}");
    let view = &set["result"]["structuredContent"];
    assert_eq!(
        view["limits"],
        json!({"memoryMb": 1024, "processes": 256, "cpuPercent": null}),
        "{set}"
    );
    let by = view["enforcedBy"].as_str().unwrap_or("");
    let expected: &[&str] = if cfg!(windows) {
        &["job-object"]
    } else if cfg!(target_os = "macos") {
        &["sampled"]
    } else {
        &["systemd-scope", "sampled"]
    };
    assert!(expected.contains(&by), "{set}");
    let bad = h
        .call("mcs_1", 31, "ags_a", "link__limits", json!({"memoryMb": 1}))
        .await;
    assert!(bad.to_string().contains("LIMITS_INVALID"), "{bad}");
    let servers = h
        .call("mcs_1", 32, "ags_a", "link__servers", json!({}))
        .await;
    let fake_view = servers["result"]["structuredContent"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "fake")
        .cloned()
        .unwrap();
    assert_eq!(
        fake_view["restarts"], 0,
        "limits change without a restart: {servers}"
    );
    let again = h
        .call(
            "mcs_1",
            33,
            "ags_a",
            "fake__read",
            json!({"path": inside.to_string_lossy()}),
        )
        .await;
    assert_eq!(again["result"]["content"][0]["text"], "hello", "{again}");

    let audit = h.audit();
    assert!(
        audit.contains("escaped.txt"),
        "the refused write is in the audit with its arguments"
    );
    assert!(audit.contains("\"checkpoint\":\"cp1\""));
    assert!(audit.contains("\"kind\":\"server\",") && audit.contains("\"policy\""));

    // Device #147: each tool of a display-granted server says so in the list the Agent sees (and in `_meta`); `_…`
    // tools are never listed; the confined server's tools stay as they are.
    let granted = h
        .call("mcs_1", 34, "ags_a", "link__server_add", json!({
            "name": "screen", "command": "node", "args": [fake.to_string_lossy()], "display": true, "waitMs": 120000,
        }))
        .await;
    assert_eq!(
        granted["result"]["structuredContent"]["state"], "ready",
        "{granted}"
    );
    let listed = h.request("mcs_1", 35, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let find = |name: &str| tools.iter().find(|t| t["name"] == name);
    let write = find("screen__write").expect("the granted server's tools are listed");
    let text = write["description"].as_str().unwrap();
    assert!(
        text.starts_with("[Screen access granted by the person")
            && text.ends_with("] Write a file."),
        "{text}"
    );
    assert_eq!(write["_meta"]["agentrouter.top/display"], true, "{write}");
    let read = find("screen__read").unwrap()["description"]
        .as_str()
        .unwrap();
    assert!(
        read.starts_with('[') && read.ends_with(']'),
        "a tool without its own description: {read}"
    );
    assert!(
        find("screen___hidden").is_none() && find("fake___hidden").is_none(),
        "`_…` tools are never listed"
    );
    let confined = find("fake__write").unwrap();
    assert!(
        confined["description"] == "Write a file." && confined.get("_meta").is_none(),
        "{confined}"
    );

    h.agg.shutdown().await;
    let _ = std::fs::remove_dir_all(&base);
}

/// A server with its own login (HOSTD-MCP §13.7): `login` prints an address and a key, `login_check` writes the
/// credential file named by `SITE_COOKIE_FILE`, `whoami` echoes that file (to prove results are blanked).
const FAKE_LOGIN: &str = r#"
const fs = require('fs');
const send = (m) => process.stdout.write(JSON.stringify(m) + '\n');
const text = (id, t, isError) => send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: String(t) }], isError: !!isError } });
let buf = '';
process.stdin.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.method === 'initialize') send({ jsonrpc: '2.0', id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'fake-login', version: '1' } } });
    else if (m.method === 'tools/list') send({ jsonrpc: '2.0', id: m.id, result: { tools: [
      { name: 'login', inputSchema: { type: 'object' } },
      { name: 'login_check', inputSchema: { type: 'object' } },
      { name: 'whoami', inputSchema: { type: 'object' }, annotations: { readOnlyHint: true } },
    ] } });
    else if (m.method === 'tools/call') {
      const a = m.params.arguments || {};
      const file = process.env.SITE_COOKIE_FILE;
      try {
        if (m.params.name === 'login') text(m.id, 'Scan:\nhttps://login.example.test/qr?c=1\n{ "loginKey": "K123" }');
        else if (m.params.name === 'login_check') {
          if (a.loginKey !== 'K123') text(m.id, 'wrong key ' + a.loginKey, true);
          else if (!fs.existsSync(process.env.FAKE_SCANNED)) text(m.id, 'not scanned', true);
          else { fs.writeFileSync(file, JSON.stringify({ sessdata: 's3cr3t-value-1', uid: '7' })); text(m.id, 'logged in'); }
        } else text(m.id, 'cookie: ' + fs.readFileSync(file, 'utf8'));
      } catch (e) { text(m.id, 'failed: ' + (e.code || e.message), true); }
    } else if (m.id !== undefined && m.method) send({ jsonrpc: '2.0', id: m.id, result: {} });
  }
});
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_logs_in_on_this_computer_and_its_credential_stays_here() {
    if std::env::var("AGENTROUTER_SANDBOX_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    let base = test_root().join(format!("agentrouter-agg-login-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("work")).unwrap();
    std::fs::create_dir_all(base.join("tools")).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let base = PathBuf::from(
        base.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    let folder = base.join("work");
    let fake = base.join("tools").join("fake-login.cjs");
    std::fs::write(&fake, FAKE_LOGIN).unwrap();
    let mut h = Harness::new(&base, &folder, TestHooks::default(), true);
    agentrouter_sandbox::Runtime::new(&h.data)
        .setup()
        .expect("sandbox setup on the runner");
    h.open("mcs_1", "ags_a").await;
    let scanned = base.join("scanned");
    let def = json!({"name": "site", "command": "node", "args": [fake.to_string_lossy()], "waitMs": 120000,
        "env": {"FAKE_SCANNED": scanned.to_string_lossy()},
        "credentials": [{"name": "cookie", "fileEnv": "SITE_COOKIE_FILE"}],
        "login": {"credential": "cookie", "start": "login", "url": "https://\\S+",
                  "key": "\"loginKey\": \"([^\"]+)\"", "check": "login_check", "keyArg": "loginKey"}});
    let added = h
        .call("mcs_1", 2, "ags_a", "link__server_add", def.clone())
        .await;
    assert_eq!(
        added["result"]["structuredContent"]["state"], "ready",
        "{added}"
    );
    assert_eq!(
        added["result"]["structuredContent"]["credentials"][0]["state"], "missing",
        "{added}"
    );

    // The login tools are hostd's: not listed, refused by name.
    let listed = h.request("mcs_1", 3, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"site__whoami")
            && !names.contains(&"site__login")
            && !names.contains(&"site__login_check"),
        "{names:?}"
    );
    let refused = h.call("mcs_1", 4, "ags_a", "site__login", json!({})).await;
    assert_eq!(code_of(&refused), "HOST_ONLY_TOOL", "{refused}");

    // The person logs in on this computer: the address is shown here and answered; the check runs in the background.
    std::fs::write(&scanned, "yes").unwrap();
    let login = h
        .call("mcs_1", 5, "ags_a", "link__login", json!({"name": "site"}))
        .await;
    assert_eq!(
        login["result"]["structuredContent"]["url"], "https://login.example.test/qr?c=1",
        "{login}"
    );
    assert!(
        h.hooks
            .feed
            .lock()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Feed::Login { url, .. } if url.contains("login.example.test")))
    );
    let mut state = Value::Null;
    for id in 6..60 {
        let servers = h
            .call("mcs_1", id, "ags_a", "link__servers", json!({}))
            .await;
        state = servers["result"]["structuredContent"]["servers"][0]["credentials"][0].clone();
        if state["state"] == "ok" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(state["state"], "ok", "{state}");
    let kept = agentrouter_device::local_secrets::get(&h.data, "site/cookie")
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&kept).contains("s3cr3t-value-1"));

    // Its results never carry the value; a restart hands the kept credential back through the file.
    let who = h
        .call("mcs_1", 70, "ags_a", "site__whoami", json!({}))
        .await;
    let text = who["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("[secret]") && !text.contains("s3cr3t-value-1"),
        "{who}"
    );
    h.call(
        "mcs_1",
        71,
        "ags_a",
        "link__server_remove",
        json!({"name": "site"}),
    )
    .await;
    let again = h.call("mcs_1", 72, "ags_a", "link__server_add", def).await;
    assert_eq!(
        again["result"]["structuredContent"]["credentials"][0]["state"], "ok",
        "{again}"
    );
    let who = h
        .call("mcs_1", 73, "ags_a", "site__whoami", json!({}))
        .await;
    assert!(
        who["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("[secret]"),
        "{who}"
    );
    assert!(!h.audit().contains("s3cr3t-value-1"), "never in the audit");

    // Logging out forgets it on this computer (nobody scans the login the restart shows).
    std::fs::remove_file(&scanned).unwrap();
    let out = h
        .call(
            "mcs_1",
            74,
            "ags_a",
            "link__logout",
            json!({"name": "site"}),
        )
        .await;
    assert_eq!(
        out["result"]["structuredContent"]["forgotten"],
        json!(["cookie"]),
        "{out}"
    );
    assert_eq!(
        agentrouter_device::local_secrets::get(&h.data, "site/cookie").unwrap(),
        None
    );

    h.agg.shutdown().await;
    let _ = std::fs::remove_dir_all(&base);
}
