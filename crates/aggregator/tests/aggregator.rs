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
            || preset["error"]["data"]["next"]
                .as_str()
                .unwrap()
                .contains("Node.js")
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
      { name: 'write', inputSchema: { type: 'object' } },
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
    let base = std::env::temp_dir().join(format!("agentrouter-agg-{}", std::process::id()));
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
    assert_eq!(
        added["result"]["structuredContent"]["state"], "ready",
        "{added}"
    );
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

    let audit = h.audit();
    assert!(
        audit.contains("escaped.txt"),
        "the refused write is in the audit with its arguments"
    );
    assert!(audit.contains("\"checkpoint\":\"cp1\""));
    assert!(audit.contains("\"kind\":\"server\",") && audit.contains("\"policy\""));

    h.agg.shutdown().await;
    let _ = std::fs::remove_dir_all(&base);
}
