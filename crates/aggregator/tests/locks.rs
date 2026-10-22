//! Catalog servers install from their locks (Cloud #2740) and run confined: a real npm lock (the files server, 104
//! packages) and a real Python lock (the git server, 34 wheels on a uv-managed Python), fetched from the official
//! registries with every checksum checked; a tampered checksum fails with `MCP_DIGEST_MISMATCH` and installs nothing.
//! The time of each first install is printed (`lock timing` lines).
//!
//! Runs only with `AGENTROUTER_SANDBOX_TESTS=1` (the `Sandbox (…)` CI jobs on GitHub-hosted runners). With
//! `AGENTROUTER_MIRROR_TIMING=1` as well (CI `workflow_dispatch` input) it also times the same first installs with the
//! mirrors first (empty caches).
//!
//! Its own test binary: it points npm's and uv's caches at empty folders between phases (process environment).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentrouter_aggregator::{Aggregator, BoxFuture, Config, Feed, Hooks, servers::ServerDef, sig};
use agentrouter_device::protocol::{ControlKey, DeviceKey, sha256_hex};
use agentrouter_device::util::{b64url_decode, now_ms};
use agentrouter_sandbox::Runtime;
use agentrouter_sandbox::locks::Lock;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::timeout;

const LINK: &str = "lnk_00000000000000000000000000000002";
const FILES_DIGEST: &str = "sha512-2fN6qhpsiPSBAe0ev1qDPk4sCWdULCZQTuYc4ZY7+BDWQ1XCxLB+7dqStF2ZflgG4G/9vTwQsjxh7pjFqrrpcg==";
const GIT_DIGEST: &str = "sha256:5bd3dc5736d67ab01b664363055001b2ce84e945c17af98875e5e04b5823d335";
const NPM_INDEXES: [&str; 2] = [
    "https://registry.npmjs.org/",
    "https://registry.npmmirror.com/",
];
const UV_INDEXES: [&str; 3] = [
    "https://pypi.org/simple",
    "https://pypi.tuna.tsinghua.edu.cn/simple",
    "https://mirrors.aliyun.com/pypi/simple",
];

fn test_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("locks")
            .join(path),
    )
    .unwrap()
}

/// The `link__server_add` arguments Control sends for a pinned catalog entry with its lock.
fn files_args(indexes: &[&str], lock_json: String) -> Value {
    json!({
        "name": "files", "title": "文件", "command": "npx", "preset": "files",
        "args": ["-y", "-p", "{{package}}", "mcp-server-filesystem", "{{folders}}"],
        "pin": {"file": "server-filesystem-1.0.0.tgz", "digest": FILES_DIGEST,
                "package": "@modelcontextprotocol/server-filesystem", "version": "1.0.0"},
        "lock": {"kind": "npm", "indexes": indexes,
                 "files": {"package.json": fixture("files/package.json"), "package-lock.json": lock_json},
                 "package": "@modelcontextprotocol/server-filesystem", "version": "1.0.0",
                 "bin": "mcp-server-filesystem", "args": ["{{folders}}"]},
        "waitMs": 120000,
    })
}

fn git_args(indexes: &[&str], requirements: String) -> Value {
    json!({
        "name": "git", "title": "Git", "command": "uvx", "preset": "git",
        "args": ["--from", "{{package}}", "mcp-server-git", "--repository", "{{folder}}"],
        "pin": {"file": "mcp_server_git-2026.10.10-py3-none-any.whl", "digest": GIT_DIGEST,
                "package": "mcp-server-git", "version": "2026.10.10"},
        "lock": {"kind": "uv", "indexes": indexes,
                 "files": {"requirements.txt": requirements, ".python-version": fixture("git/.python-version")},
                 "package": "mcp-server-git", "version": "2026.10.10",
                 "bin": "mcp-server-git", "args": ["--repository", "{{folder}}"]},
        "waitMs": 120000,
    })
}

/// One dependency's checksum changed (the pinned package's own stays right, so only the install can catch it).
fn tampered_npm() -> String {
    let mut tree: Value = serde_json::from_str(&fixture("files/package-lock.json")).unwrap();
    let dep = tree["packages"]
        .as_object()
        .unwrap()
        .keys()
        .find(|k| k.starts_with("node_modules/") && !k.ends_with("server-filesystem"))
        .unwrap()
        .clone();
    tree["packages"][&dep]["integrity"] = json!(format!("sha512-{}", "A".repeat(86) + "=="));
    tree.to_string()
}

fn tampered_uv() -> String {
    let text = fixture("git/requirements.txt");
    // The first hash in the file is a dependency's (annotated-types), never the pinned package's.
    let at = text.find("--hash=sha256:").unwrap() + "--hash=sha256:".len();
    format!("{}{}{}", &text[..at], "0".repeat(64), &text[at + 64..])
}

fn lock_of(args: &Value) -> Lock {
    Lock::parse(args).unwrap().unwrap()
}

struct NoHooks;
impl Hooks for NoHooks {
    fn feed(&self, _event: Feed) {}
    fn confirm_server(&self, _def: &ServerDef) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
    fn checkpoint(&self, _label: &str) -> Option<String> {
        None
    }
}

struct Link {
    agg: Aggregator,
    rx: mpsc::UnboundedReceiver<(String, Value)>,
    key: DeviceKey,
    kid: String,
    nonce: u64,
    next_id: i64,
}

impl Link {
    fn new(data: &Path, folder: &Path) -> Link {
        let key = DeviceKey::from_seed([7; 32]);
        let raw = b64url_decode(&key.public_key()).unwrap();
        let control = ControlKey {
            kid: sha256_hex(&raw)[..16].to_string(),
            public_key: key.public_key(),
        };
        let kid = control.kid.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let agg = Aggregator::new(
            Config {
                link: LINK.into(),
                folders: vec![folder.to_path_buf()],
                folder_name: "work".into(),
                read_only: false,
                full: false,
                data_dir: data.to_path_buf(),
                control,
                yes_servers: false,
                cloud: false,
                limits: Default::default(),
                device_program: std::env::var_os("AGENTROUTER_DEVICE_EXE").map(PathBuf::from),
            },
            std::sync::Arc::new(NoHooks),
            std::sync::Arc::new(move |s: &str, m: Value| {
                let _ = tx.send((s.to_string(), m));
            }),
        );
        Link {
            agg,
            rx,
            key,
            kid,
            nonce: 0,
            next_id: 1,
        }
    }

    async fn request(&mut self, method: &str, mut params: Value) -> Value {
        self.nonce += 1;
        self.next_id += 1;
        let id = self.next_id;
        let nonce = format!("{:032x}", self.nonce + (now_ms() as u64) * 1000);
        let exp = now_ms() + 60_000;
        let digest = sig::params_digest(&params);
        let text = sig::signed_string(LINK, "ags_l", method, &digest, exp, &nonce);
        let signature = self.key.sign(&text);
        params["_meta"] = json!({sig::META_KEY: {"v": 3, "kid": self.kid, "link": LINK, "session": "ags_l",
            "digest": digest, "exp": exp, "nonce": nonce, "sig": signature}});
        self.agg.on_message(
            "mcs_l",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        );
        loop {
            let (s, m) = timeout(Duration::from_secs(600), self.rx.recv())
                .await
                .expect("an answer in time")
                .unwrap();
            if s == "mcs_l" && m.get("id") == Some(&json!(id)) && m.get("method").is_none() {
                return m;
            }
        }
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }
}

fn timing(what: &str, took: Duration, from: Option<&str>) {
    eprintln!(
        "lock timing | {} | {} | {what} | {:.1}s | from {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        took.as_secs_f64(),
        from.unwrap_or("(already installed)")
    );
}

/// Point npm's and uv's caches at empty folders, so the next install downloads everything.
fn fresh_caches(base: &Path, tag: &str) {
    let npm = base.join(format!("npm-cache-{tag}"));
    let uv = base.join(format!("uv-cache-{tag}"));
    // SAFETY: this test binary has one test and sets these before any install runs; nothing reads them concurrently.
    unsafe {
        std::env::set_var("npm_config_cache", &npm);
        std::env::set_var("UV_CACHE_DIR", &uv);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_servers_install_from_their_locks_and_run_confined() {
    if std::env::var("AGENTROUTER_SANDBOX_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    let base = test_root().join(format!("agentrouter-locks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("work")).unwrap();
    let base = PathBuf::from(
        std::fs::canonicalize(&base)
            .unwrap()
            .to_string_lossy()
            .trim_start_matches(r"\\?\"),
    );
    let folder = base.join("work");
    let git_init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&folder)
        .status();
    assert!(
        git_init.is_ok_and(|s| s.success()),
        "git init in the linked folder"
    );
    std::fs::write(folder.join("hello.txt"), "hi").unwrap();
    fresh_caches(&base, "official");

    let data = base.join("data");
    let rt = Runtime::new(&data);
    rt.setup().expect("sandbox setup on the runner");

    // 1. First installs, official sources first, timed.
    let files = files_args(&NPM_INDEXES, fixture("files/package-lock.json"));
    let git = git_args(&UV_INDEXES, fixture("git/requirements.txt"));
    for (what, args) in [
        ("npm files (104 packages)", &files),
        ("uv git (34 wheels + Python)", &git),
    ] {
        let lock = lock_of(args);
        let started = Instant::now();
        let installed = tokio::task::spawn_blocking({
            let rt = rt.clone();
            move || rt.install_lock(&lock)
        })
        .await
        .unwrap()
        .unwrap_or_else(|u| panic!("{what}: {} {} ({})", u.code, u.message, u.next));
        timing(what, started.elapsed(), installed.from.as_deref());
        assert_eq!(
            installed.from.as_deref(),
            Some(args["lock"]["indexes"][0].as_str().unwrap()),
            "official first"
        );
        let again = rt.install_lock(&lock_of(args)).unwrap();
        assert!(
            again.from.is_none(),
            "{what}: the second time it is already there"
        );
    }

    // 2. Both run confined from what was installed: tools listed, a call works.
    let mut link = Link::new(&data, &folder);
    link.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}))
        .await;
    link.agg.on_message(
        "mcs_l",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    for (name, args) in [("files", &files), ("git", &git)] {
        let added = link.call("link__server_add", args.clone()).await;
        if added["result"]["structuredContent"]["state"] != "ready" {
            let logs = link.call("link__server_logs", json!({"name": name})).await;
            panic!("{name} not ready: {added}\nlogs: {logs}");
        }
    }
    let listed = link.request("tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&"files__list_directory") && names.contains(&"git__git_status"),
        "{names:?}"
    );
    let dir = link
        .call(
            "files__list_directory",
            json!({"path": folder.to_string_lossy()}),
        )
        .await;
    assert!(dir.to_string().contains("hello.txt"), "{dir}");
    let status = link
        .call(
            "git__git_status",
            json!({"repo_path": folder.to_string_lossy()}),
        )
        .await;
    assert!(status.to_string().contains("hello.txt"), "{status}");
    link.call("link__server_remove", json!({"name": "files"}))
        .await;
    link.call("link__server_remove", json!({"name": "git"}))
        .await;

    // 3. A tampered checksum: refused with the catalog code, nothing installed, never started.
    for (what, args) in [
        ("npm", files_args(&NPM_INDEXES[..1], tampered_npm())),
        ("uv", git_args(&UV_INDEXES[..1], tampered_uv())),
    ] {
        let lock = lock_of(&args);
        let refused = rt
            .install_lock(&lock)
            .expect_err("a tampered checksum never installs");
        assert_eq!(
            refused.code, "MCP_DIGEST_MISMATCH",
            "{what}: {} {}",
            refused.message, refused.next
        );
        assert!(
            !rt.lock_dir(&lock).join(".agentrouter-lock").exists(),
            "{what}: no marker"
        );
        let mut args = args;
        args["name"] = json!(format!("bad{what}"));
        let added = link.call("link__server_add", args).await;
        assert_eq!(
            added["error"]["data"]["code"], "MCP_DIGEST_MISMATCH",
            "{what}: {added}"
        );
    }

    // 4. Optional: the same first installs with the mirrors first (empty caches, a new data folder), timed.
    if std::env::var("AGENTROUTER_MIRROR_TIMING").ok().as_deref() == Some("1") {
        fresh_caches(&base, "mirrors");
        let data = base.join("data-mirrors");
        let rt = Runtime::new(&data);
        std::fs::create_dir_all(rt.runtimes_dir()).unwrap();
        agentrouter_sandbox::runtimes::set_prefer_mirrors(&rt.runtimes_dir());
        rt.provision(false).expect("runtimes from the mirrors");
        for (what, args) in [
            ("npm files, mirrors first", &files),
            ("uv git + Python, mirrors first", &git),
        ] {
            let lock = lock_of(args);
            let started = Instant::now();
            let rt2 = rt.clone();
            match tokio::task::spawn_blocking(move || rt2.install_lock(&lock))
                .await
                .unwrap()
            {
                Ok(i) => timing(what, started.elapsed(), i.from.as_deref()),
                // A measurement: a mirror far from the runner may be slow or refuse; say so, do not fail.
                Err(u) => eprintln!(
                    "lock timing | {what} | failed after {:.1}s: {} {}",
                    started.elapsed().as_secs_f64(),
                    u.code,
                    u.message
                ),
            }
        }
    }
    drop(link);
    let _ = std::fs::remove_dir_all(&base);
}
