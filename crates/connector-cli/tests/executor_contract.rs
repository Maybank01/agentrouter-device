//! hostd against the executor protocol it implements (AgentCompute contracts/executor-protocol/v0, vendored byte for
//! byte under `vendor/agentcompute/contracts/executor-protocol/v0/`). v0 describes the interface as it works today: a
//! host tool renamed, added or dropped, a readOnly hint flipped, a signing string or a link constant changed fails here,
//! so the change goes with a new contract release instead of being found by the control plane.
//!
//! The host server's tool list is private to the `agentrouter` binary (`host::tools()`), so this asks the real server
//! for it — `agentrouter mcp-serve host <folder>`
//! over stdio, `initialize` then `tools/list` — the way the aggregator does.

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use agentrouter_aggregator::sig;
use agentrouter_device::protocol::{REQUEST_MAX_AHEAD_MS, pair_string, token_string};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

fn contract(file: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/agentcompute/contracts/executor-protocol/v0")
        .join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The host server's `tools/list` answer, from the real binary.
fn listed_tools() -> Vec<Value> {
    let root = std::env::temp_dir().join(format!("agentrouter-contract-{}", std::process::id()));
    let folder = root.join("work");
    std::fs::create_dir_all(&folder).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentrouter"))
        .args(["mcp-serve", "host"])
        .arg(&folder)
        .env("AGENTROUTER_DEVICE_HOME", root.join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "contract-test", "version": "1"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    stdin.flush().unwrap();
    let mut tools = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let answer: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if answer["id"] == json!(2) {
            tools = Some(
                answer["result"]["tools"]
                    .as_array()
                    .cloned()
                    .expect("tools/list answers tools"),
            );
            break;
        }
    }
    drop(stdin); // input closed: the server ends
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
    tools.expect("the host server answered tools/list")
}

fn names(list: &Value) -> Vec<(String, Option<bool>)> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["readOnly"].as_bool(),
            )
        })
        .collect()
}

#[test]
fn the_host_server_lists_exactly_the_contracts_tools() {
    let tools = contract("tools.json");
    assert_eq!(tools["version"], "agentcompute.executor-protocol/v0");
    let listed = listed_tools();
    let got: Vec<(String, Option<bool>)> = listed
        .iter()
        .filter(|t| !t["name"].as_str().unwrap().starts_with('_'))
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_string(),
                t["annotations"]["readOnlyHint"].as_bool(),
            )
        })
        .collect();
    assert_eq!(
        got,
        names(&tools["servers"]["host"]),
        "public host tools (name, readOnly), in order"
    );
    for t in tools["servers"]["host"].as_array().unwrap() {
        assert_eq!(
            t["wireName"],
            json!(format!("host__{}", t["name"].as_str().unwrap()))
        );
    }
    // hostd's own `_` tools: listed by the server (the aggregator hides them); `_port_bridge` exists only on Unix.
    let internal: Vec<String> = listed
        .iter()
        .filter_map(|t| {
            t["name"]
                .as_str()
                .filter(|n| n.starts_with('_'))
                .map(String::from)
        })
        .collect();
    let expected: Vec<String> = tools["internal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .filter(|n| cfg!(unix) || n != "_port_bridge")
        .collect();
    assert_eq!(internal, expected, "hostd-internal tools, in order");
}

#[test]
fn the_input_schemas_are_the_contracts() {
    let tools = contract("tools.json");
    let listed = listed_tools();
    for t in tools["servers"]["host"]
        .as_array()
        .unwrap()
        .iter()
        .chain(tools["internal"].as_array().unwrap())
    {
        let name = t["name"].as_str().unwrap();
        let Some(served) = listed.iter().find(|l| l["name"] == name) else {
            continue;
        }; // `_port_bridge` off Unix
        assert_eq!(
            served["inputSchema"], t["inputSchema"],
            "{name} inputSchema"
        );
    }
}

#[test]
fn the_signing_strings_and_link_constants_are_the_contracts() {
    let link = contract("link.json");
    assert_eq!(link["version"], "agentcompute.executor-protocol/v0");
    let prefixes: Vec<&str> = link["signing"]["strings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["prefix"].as_str().unwrap())
        .collect();
    let first = |text: String| text.lines().next().unwrap().to_string();
    for ours in [
        first(pair_string("k", "n", "linux", "x64")),
        first(token_string("lnd_x", "c")),
        first(sig::signed_string("lnk_x", "s", "tools/call", "d", 1, "n")),
        first(sig::owner_string("lnk_x", "1")),
    ] {
        assert!(
            prefixes.contains(&ours.as_str()),
            "{ours} is not in link.json signing.strings"
        );
    }
    assert_eq!(link["signing"]["linkSignature"]["metaKey"], sig::META_KEY);
    assert_eq!(
        link["protocol"]["linkSignatureVersion"],
        json!(sig::VERSION)
    );
    assert!(
        link["signing"]["linkSignature"]["signedTopLevelParams"]
            .get(sig::ACCOUNT_PARAM)
            .is_some()
    );
    assert_eq!(
        link["protocol"]["linkVersion"],
        json!(agentrouter_hostd_link::VERSION)
    );
    assert_eq!(
        link["limits"]["frameMaxBytes"],
        json!(agentrouter_hostd_link::FRAME_MAX)
    );
    assert_eq!(
        link["limits"]["requestMaxAheadMs"],
        json!(REQUEST_MAX_AHEAD_MS)
    );
    assert_eq!(
        link["gatewayPaths"]["linkWebSocket"]["path"],
        agentrouter_hostd_link::PATH
    );
}

#[test]
fn the_vendored_vectors_are_the_ones_the_crates_check() {
    let vendored = |file: &str| {
        std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../vendor/agentcompute/contracts/executor-protocol/v0/vectors")
                .join(file),
        )
        .unwrap()
    };
    assert_eq!(
        vendored("link-sig-v3-vectors.json"),
        include_bytes!("../../aggregator/tests/link-sig-v3-vectors.json").to_vec()
    );
    assert_eq!(
        vendored("protocol-vectors.json"),
        include_bytes!("../../device-core/tests/fixtures/protocol-vectors.json").to_vec()
    );
}

/// The vendored files are the provider's bytes: each matches its line in `vendor/agentcompute/SHA256SUMS` (C4).
#[test]
fn the_vendored_contract_files_match_their_checksums() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/agentcompute");
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let mut checked = 0;
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let (want, file) = line.split_once("  ").expect("<sha256>  <path>");
        let got: String = Sha256::digest(std::fs::read(dir.join(file)).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(got, want, "{file}");
        checked += 1;
    }
    assert!(checked >= 2, "SHA256SUMS lists the vendored files");
}
