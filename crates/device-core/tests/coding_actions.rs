//! The coding actions, pending approvals and checkpoints (DEVICE-PROTOCOL.md §5.4–§5.7).

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use agentrouter_device::checkpoint;
use agentrouter_device::config::Access;
use agentrouter_device::consent::{Ask, Confirm, Decision, deny_all};
use common::*;
use serde_json::{Value, json};

fn write_files(root: &Path, files: &Value) {
    for (rel, content) in files.as_object().unwrap() {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content.as_str().unwrap()).unwrap();
    }
}

fn fill(value: &Value, root: &str) -> Value {
    match value {
        Value::String(s) => json!(s.replace("{root}", root)),
        Value::Array(a) => Value::Array(a.iter().map(|v| fill(v, root)).collect()),
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), fill(v, root))).collect())
        }
        other => other.clone(),
    }
}

#[test]
fn edit_and_patch_vectors() {
    let vectors: Value =
        serde_json::from_str(include_str!("fixtures/action-vectors.json")).unwrap();
    for case in vectors["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let cp = ControlPlane::new();
        let folder = temp_dir("vec");
        write_files(&folder, &case["files"]);
        let d = device(
            &temp_dir("vec-data"),
            Access::Folders,
            &[&folder],
            deny_all(),
            &cp,
        );
        let args = fill(&case["args"], &folder.display().to_string());
        let result = call(&d, &cp, case["action"].as_str().unwrap(), args);
        if let Some(code) = case.get("error").and_then(Value::as_str) {
            assert_eq!(
                result.as_ref().map_err(|e| e.0.as_str()).unwrap_err(),
                code,
                "{name}: {result:?}"
            );
            for (rel, content) in case["files"].as_object().unwrap() {
                assert_eq!(
                    std::fs::read_to_string(folder.join(rel)).unwrap(),
                    content.as_str().unwrap(),
                    "{name}: {rel} changed"
                );
            }
            continue;
        }
        let value = result.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        for (rel, want) in case["result"].as_object().unwrap() {
            let path = folder.join(rel);
            match want.as_str() {
                Some(text) => assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    text,
                    "{name}: {rel}"
                ),
                None => assert!(!path.exists(), "{name}: {rel} should be gone"),
            }
        }
        if let Some(n) = case.get("replacements") {
            assert_eq!(&value["replacements"], n, "{name}");
        }
        assert!(
            value["checkpoint"].as_str().unwrap().starts_with("cp_"),
            "{name}: {value}"
        );
    }
}

#[test]
fn read_search_and_list() {
    let cp = ControlPlane::new();
    let folder = temp_dir("search");
    write_files(
        &folder,
        &json!({
            "src/main.rs": "fn main() {\n    // TODO: greet\n    println!(\"hi\");\n}\n",
            "src/lib.rs": "pub fn todo() {}\n",
            "ignored/skip.rs": "TODO hidden by gitignore\n",
            ".gitignore": "ignored/\n",
            "data.bin": "\u{0}\u{1}TODO",
        }),
    );
    let d = device(
        &temp_dir("search-data"),
        Access::Folders,
        &[&folder],
        deny_all(),
        &cp,
    );
    let f = |rel: &str| folder.join(rel).display().to_string();

    let v = call(&d, &cp, "search", json!({"pattern": "TODO", "context": 1})).unwrap();
    let matches = v["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{v}");
    assert_eq!(matches[0]["line"], 2);
    assert_eq!(matches[0]["before"], json!(["fn main() {"]));
    assert_eq!(matches[0]["after"], json!(["    println!(\"hi\");"]));
    // Smart case: lower case pattern matches both.
    let v = call(
        &d,
        &cp,
        "search",
        json!({"pattern": "todo", "filesOnly": true}),
    )
    .unwrap();
    assert_eq!(v["files"].as_array().unwrap().len(), 2, "{v}");
    let v = call(
        &d,
        &cp,
        "search",
        json!({"pattern": "fn (", "literal": true, "glob": ["*.rs"]}),
    )
    .unwrap();
    assert_eq!(v["matches"].as_array().unwrap().len(), 0, "{v}");
    let outside = temp_dir("search-outside");
    assert_eq!(
        call(
            &d,
            &cp,
            "search",
            json!({"pattern": "x", "path": outside.display().to_string()})
        )
        .unwrap_err()
        .0,
        "DENIED"
    );
    // A refusal tells the model what to do instead.
    let args = json!({"path": outside.join("x").display().to_string(), "offset": 0, "limit": 10, "encoding": "utf8"});
    let e = d
        .serve(
            &cp.sign("ags_x", "read_file", &args),
            &args,
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(e.code, "DENIED");
    assert!(
        e.next.as_deref().unwrap_or("").contains("agentrouter link"),
        "{e:?}"
    );

    let v = call(
        &d,
        &cp,
        "read_files",
        json!({"paths": [f("src/main.rs"), f("data.bin"), f("nope.txt")]}),
    )
    .unwrap();
    let files = v["files"].as_array().unwrap();
    assert!(files[0]["content"].as_str().unwrap().contains("TODO"));
    assert_eq!(files[0]["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(files[1]["binary"], true);
    assert_eq!(files[2]["error"]["code"], "NOT_FOUND");

    let v = call(
        &d,
        &cp,
        "list_dir",
        json!({"path": folder.display().to_string()}),
    )
    .unwrap();
    let names: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(names[..2], ["ignored", "src"], "{names:?}");
    assert!(names.contains(&".gitignore"));
    let v = call(
        &d,
        &cp,
        "list_dir",
        json!({"path": folder.display().to_string(), "depth": 3}),
    )
    .unwrap();
    let names: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"src/main.rs"), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("ignored")), "{names:?}");

    // write_file with a stale base hash.
    let r = call(
        &d,
        &cp,
        "write_file",
        json!({"path": f("src/lib.rs"), "content": "x", "encoding": "utf8", "append": false, "baseHash": "00"}),
    );
    assert_eq!(r.unwrap_err().0, "CONFLICT");
}

/// A confirmation the test answers later (and counts).
fn held(answer: Arc<std::sync::Mutex<Option<Decision>>>, asked: Arc<AtomicUsize>) -> Confirm {
    Arc::new(move |_: &Ask, cancel: &AtomicBool| {
        asked.fetch_add(1, Ordering::SeqCst);
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Decision::Deny;
            }
            if let Some(d) = *answer.lock().unwrap() {
                return d;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })
}

#[test]
fn approvals_wait_on_the_device() {
    let cp = ControlPlane::new();
    let folder = temp_dir("apv");
    let answer = Arc::new(std::sync::Mutex::new(None));
    let asked = Arc::new(AtomicUsize::new(0));
    let d = device(
        &temp_dir("apv-data"),
        Access::Confirm,
        &[&folder],
        held(answer.clone(), asked.clone()),
        &cp,
    );
    let args = json!({"command": echo("later"), "timeout": 30, "approvalWait": 0});
    let v = call(&d, &cp, "exec", args.clone()).unwrap();
    assert_eq!(v["status"], "awaiting_approval", "{v}");
    let id = v["approval"].as_str().unwrap().to_string();
    assert!(id.starts_with("apv_"));
    // The same request again joins the open question: no second prompt.
    let again = call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("later"), "timeout": 5, "approvalWait": 0}),
    )
    .unwrap();
    assert_eq!(again["approval"], id.as_str());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    let info = call(&d, &cp, "info", json!({})).unwrap();
    assert_eq!(info["approvals"][0]["approval"], id.as_str());
    // Another conversation cannot see it.
    let other = cp.sign("ags_other", "job", &json!({"job": id, "action": "output"}));
    assert_eq!(
        d.serve(
            &other,
            &json!({"job": id, "action": "output"}),
            &AtomicBool::new(false)
        )
        .unwrap_err()
        .code,
        "JOB_NOT_FOUND"
    );
    // Approved: it runs, and `job wait` on the approval returns the command's result.
    *answer.lock().unwrap() = Some(Decision::Session);
    let done = call(
        &d,
        &cp,
        "job",
        json!({"job": id, "action": "wait", "timeout": 30}),
    )
    .unwrap();
    assert_eq!(done["status"], "exited", "{done}");
    assert!(done["job"].as_str().unwrap().starts_with("job_"));
    assert!(done["output"].as_str().unwrap().contains("later"), "{done}");
    // 本对话同类允许: a single-word command only repeats exactly, without asking again.
    *answer.lock().unwrap() = None;
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("later"), "timeout": 30}),
    )
    .unwrap();
    assert_eq!(v["status"], "exited", "{v}");
    assert_eq!(asked.load(Ordering::SeqCst), 1);

    // Withdrawn with `job kill`.
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("never"), "timeout": 30, "approvalWait": 0}),
    )
    .unwrap();
    let id = v["approval"].as_str().unwrap().to_string();
    let w = call(&d, &cp, "job", json!({"job": id, "action": "kill"})).unwrap();
    assert_eq!(w["status"], "withdrawn", "{w}");

    // Declined later: DENIED_BY_USER from `job`.
    let v = call(&d, &cp, "write_file", json!({"path": folder.join("x.txt").display().to_string(), "content": "x", "encoding": "utf8", "append": false, "approvalWait": 0})).unwrap();
    let id = v["approval"].as_str().unwrap().to_string();
    *answer.lock().unwrap() = Some(Decision::Deny);
    let r = call(
        &d,
        &cp,
        "job",
        json!({"job": id, "action": "wait", "timeout": 10}),
    );
    assert_eq!(r.unwrap_err().0, "DENIED_BY_USER");
    assert!(!folder.join("x.txt").exists());
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?}");
}

#[test]
fn checkpoints_undo_a_turn_in_a_git_repo() {
    let cp = ControlPlane::new();
    let repo = temp_dir("cp-repo");
    write_files(
        &repo,
        &json!({"a.txt": "original\n", "keep.txt": "keep\n", ".gitignore": "out/\n"}),
    );
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    write_files(&repo, &json!({"draft.txt": "uncommitted work\n"}));
    let data = temp_dir("cp-data");
    let d = device(&data, Access::Folders, &[&repo], deny_all(), &cp);
    let f = |rel: &str| repo.join(rel).display().to_string();
    let v = call(
        &d,
        &cp,
        "edit_file",
        json!({"path": f("a.txt"), "edits": [{"old": "original", "new": "changed"}], "turn": "t1"}),
    )
    .unwrap();
    let id = v["checkpoint"].as_str().unwrap().to_string();
    let w = call(&d, &cp, "write_file", json!({"path": f("new.txt"), "content": "new", "encoding": "utf8", "append": false, "turn": "t1"})).unwrap();
    assert_eq!(w["checkpoint"], id.as_str(), "same turn, same checkpoint");
    std::fs::remove_file(repo.join("draft.txt")).unwrap();
    // The person's branch, index and HEAD are untouched by the checkpoint.
    let meta = checkpoint::list(&data)
        .into_iter()
        .find(|m| m.id == id)
        .unwrap();
    assert_eq!(meta.kind, "git");
    let plan = checkpoint::plan(&data, &id).unwrap();
    let paths: Vec<&str> = plan.changes.iter().map(|(_, p)| p.as_str()).collect();
    assert!(
        paths.contains(&"a.txt") && paths.contains(&"new.txt") && paths.contains(&"draft.txt"),
        "{paths:?}"
    );
    checkpoint::undo(&data, &plan).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "original\n"
    );
    assert!(!repo.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("draft.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "uncommitted work\n"
    );
    // A new turn gets a new checkpoint.
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("x"), "timeout": 30, "turn": "t2"}),
    )
    .unwrap();
    assert_ne!(v["checkpoint"], id.as_str(), "{v}");
}

#[test]
fn checkpoints_without_git_back_up_files() {
    let cp = ControlPlane::new();
    let folder = temp_dir("cp-plain");
    write_files(&folder, &json!({"a.txt": "before\n"}));
    let data = temp_dir("cp-plain-data");
    let d = device(&data, Access::Folders, &[&folder], deny_all(), &cp);
    let p = folder.join("a.txt").display().to_string();
    let v = call(
        &d,
        &cp,
        "write_file",
        json!({"path": p, "content": "after\n", "encoding": "utf8", "append": false, "turn": "t1"}),
    )
    .unwrap();
    let id = v["checkpoint"].as_str().unwrap().to_string();
    let n = folder.join("b.txt").display().to_string();
    call(
        &d,
        &cp,
        "edit_file",
        json!({"path": n, "edits": [{"old": "", "new": "made\n"}], "turn": "t1"}),
    )
    .unwrap();
    let plan = checkpoint::plan(&data, &id).unwrap();
    checkpoint::undo(&data, &plan).unwrap();
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(!folder.join("b.txt").exists());
}
