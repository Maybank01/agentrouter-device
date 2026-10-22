//! The feed runner end to end against a real stdio MCP server (Node, no dependencies; unconfined in 完全访问, so no
//! sandbox is needed): items from `structuredContent` become rows of the feed's table in the sink with its columns only, a key
//! seen before is not written again, an error carrying a declared back-off code pauses that feed (Cloud #2739 step 2).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentrouter_aggregator::feeds::{EventSink, FeedRow};
use agentrouter_aggregator::{Aggregator, Config, Failure, Hooks, servers::ServerDef};
use agentrouter_device::protocol::{ControlKey, DeviceKey, sha256_hex};
use agentrouter_device::util::b64url_decode;
use serde_json::{Value, json};

const FEEDER: &str = r#"
const send = (m) => process.stdout.write(JSON.stringify(m) + '\n');
let buf = '';
process.stdin.on('data', (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i); buf = buf.slice(i + 1);
    let m; try { m = JSON.parse(line); } catch { continue; }
    if (m.method === 'initialize') send({ jsonrpc: '2.0', id: m.id, result: { protocolVersion: m.params.protocolVersion, capabilities: { tools: {} }, serverInfo: { name: 'feeder', version: '1' } } });
    else if (m.method === 'tools/list') send({ jsonrpc: '2.0', id: m.id, result: { tools: [
      { name: 'following', inputSchema: { type: 'object' }, annotations: { readOnlyHint: true } },
      { name: 'blocked', inputSchema: { type: 'object' }, annotations: { readOnlyHint: true } },
    ] } });
    else if (m.method === 'tools/call' && m.params.name === 'following') send({ jsonrpc: '2.0', id: m.id, result: {
      content: [{ type: 'text', text: 'two items' }],
      structuredContent: { items: [{ id: 1, title: 'old', extra: { x: 1 } }, { id: 2, title: 'new', extra: { x: 2 } }, { title: 'no id' }] } } });
    else if (m.method === 'tools/call') send({ jsonrpc: '2.0', id: m.id, result: { content: [{ type: 'text', text: 'request failed: code -412 (risk control)' }], isError: true } });
    else if (m.id !== undefined && m.method) send({ jsonrpc: '2.0', id: m.id, result: {} });
  }
});
"#;

#[derive(Default)]
struct Sink {
    seen: Mutex<BTreeSet<(String, String)>>,
    rows: Mutex<Vec<FeedRow>>,
}

impl EventSink for Sink {
    fn append_new(&self, scope: &str, row: &FeedRow) -> Result<bool, Failure> {
        if !self
            .seen
            .lock()
            .unwrap()
            .insert((scope.into(), row.key.clone()))
        {
            return Ok(false);
        }
        self.rows.lock().unwrap().push(row.clone());
        Ok(true)
    }
    fn nearly_full(&self) -> bool {
        false
    }
}

struct TestHooks(Arc<Sink>);

impl Hooks for TestHooks {
    fn events(&self) -> Option<Arc<dyn EventSink>> {
        Some(self.0.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feed_writes_new_items_once_and_backs_off_on_declared_codes() {
    let base = std::env::temp_dir().join(format!("agentrouter-feeds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("work")).unwrap();
    let base = PathBuf::from(
        std::fs::canonicalize(&base)
            .unwrap()
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string(),
    );
    let script = base.join("feeder.cjs");
    std::fs::write(&script, FEEDER).unwrap();
    let key = DeviceKey::from_seed([7; 32]);
    let raw = b64url_decode(&key.public_key()).unwrap();
    let sink = Arc::new(Sink::default());
    // A row of an earlier run of this feed (the persisted seen-set): never written again.
    sink.seen
        .lock()
        .unwrap()
        .insert(("spc_1/关注动态".into(), "关注动态:1".into()));
    let agg = Aggregator::new(
        Config {
            link: "lnk_00000000000000000000000000000002".into(),
            folders: vec![base.join("work")],
            folder_name: "work".into(),
            read_only: false,
            full: true,
            data_dir: base.join("data"),
            control: ControlKey {
                kid: sha256_hex(&raw)[..16].to_string(),
                public_key: key.public_key(),
            },
            yes_servers: false,
            cloud: false,
            limits: Default::default(),
            device_program: None,
        },
        Arc::new(TestHooks(sink.clone())),
        Arc::new(|_: &str, _: Value| {}),
    );
    // The sandbox job hides the runner's Node.js (#116): there the device provisions its own, as on people's PCs.
    if std::env::var("AGENTROUTER_SANDBOX_TESTS").ok().as_deref() == Some("1") {
        agentrouter_sandbox::Runtime::new(&base.join("data"))
            .setup()
            .expect("sandbox setup on the runner");
    }
    let def = ServerDef::parse(
        &json!({"name": "feeder", "command": "node", "args": [script.to_string_lossy()]}),
    )
    .unwrap();
    agg.add_server(def, Duration::from_secs(60))
        .await
        .expect("the feeder server starts");

    let answer = agg
        .set_feeds(&json!({"instance": "ins_1", "space": "spc_1", "server": "feeder", "feeds": [
            {"name": "关注动态", "tool": "following", "everyMs": 900_000, "key": "id", "items": "$.items", "into": "feed",
             "columns": {"id": "text", "title": "text"}},
            {"name": "风控", "tool": "blocked", "everyMs": 60_000, "key": "id", "into": "feed",
             "columns": {"id": "text"}, "backoff": {"on": [412], "waitMs": 1_800_000}}
        ]}))
        .expect("the feeds are taken");
    assert_eq!(answer["instance"], "ins_1");

    let status = |name: &str| -> Value {
        agg.servers_view()["feeds"][0]["feeds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .cloned()
            .unwrap()
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while (status("关注动态")["runs"] == 0 || status("风控")["runs"] == 0)
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let rows = sink.rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1, "only the unseen item with a key: {rows:?}");
    assert_eq!(rows[0].key, "关注动态:2");
    assert_eq!(rows[0].table, "feed");
    assert_eq!(rows[0].instance, "ins_1");
    assert_eq!(rows[0].payload, json!({"id": "2", "title": "new"}));
    assert!(rows[0].cause.starts_with("feed:ins_1/关注动态@"));
    let feed = status("关注动态");
    assert_eq!(feed["state"], "running", "{feed}");
    assert_eq!(feed["written"], 1);
    assert_eq!(feed["error"]["code"], "FEED_ITEM_NO_KEY", "{feed}");
    let blocked = status("风控");
    assert_eq!(blocked["state"], "backing_off", "{blocked}");
    assert_eq!(blocked["error"]["code"], "FEED_BACKING_OFF");
    assert!(blocked["until"].as_i64().unwrap() > agentrouter_device::util::now_ms() + 1_700_000);

    // Stopping: feeds [] ends them.
    let stopped = agg
        .set_feeds(&json!({"instance": "ins_1", "space": "spc_1", "server": "feeder", "feeds": []}))
        .unwrap();
    assert_eq!(stopped["feeds"], json!([]));
    assert_eq!(agg.servers_view()["feeds"], json!([]));
    agg.shutdown().await;
    let _ = std::fs::remove_dir_all(&base);
}
