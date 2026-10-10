//! The device contract (DEVICE-PROTOCOL.md §5): signature checks first, then the local gate, then the
//! action. Mirrors what agentrouter-cloud's reference device is tested for.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use agentrouter_device::config::Access;
use agentrouter_device::consent::deny_all;
use agentrouter_device::util::now_ms;
use common::*;
use serde_json::json;

fn code(r: Result<serde_json::Value, (String, String)>) -> String {
    r.expect_err("expected a refusal").0
}

#[test]
fn signature_checks_come_first() {
    let cp = ControlPlane::new();
    let data = temp_dir("sig");
    let d = device(&data, Access::Full, &[], deny_all(), &cp);
    let args = json!({});
    let cancel = AtomicBool::new(false);

    // Signed by another key.
    let other = ControlPlane::new();
    let mut forged = other.sign("ags_x", "info", &args);
    forged["kid"] = json!(cp.control_key().kid);
    assert_eq!(
        d.serve(&forged, &args, &cancel).unwrap_err().code,
        "SIGNATURE_INVALID"
    );
    // Unknown kid.
    assert_eq!(
        d.serve(&other.sign("ags_x", "info", &args), &args, &cancel)
            .unwrap_err()
            .code,
        "SIGNATURE_INVALID"
    );
    // Another device.
    let wrong = cp.sign_at(
        "lnd_ffffffffffffffffffffffffffffffff",
        "ags_x",
        "info",
        &args,
        now_ms() + 30_000,
    );
    assert_eq!(
        d.serve(&wrong, &args, &cancel).unwrap_err().code,
        "WRONG_DEVICE"
    );
    // Arguments changed after signing.
    let signed = cp.sign("ags_x", "exec", &json!({"command": "dir", "timeout": 1}));
    assert_eq!(
        d.serve(
            &signed,
            &json!({"command": "del /q *", "timeout": 1}),
            &cancel
        )
        .unwrap_err()
        .code,
        "SIGNATURE_INVALID"
    );
    // Expired, and living too long.
    let old = cp.sign_at(DEVICE_ID, "ags_x", "info", &args, now_ms() - 1);
    assert_eq!(
        d.serve(&old, &args, &cancel).unwrap_err().code,
        "REQUEST_EXPIRED"
    );
    let long = cp.sign_at(DEVICE_ID, "ags_x", "info", &args, now_ms() + 10 * 60_000);
    assert_eq!(
        d.serve(&long, &args, &cancel).unwrap_err().code,
        "REQUEST_EXPIRED"
    );
    // Replay.
    let once = cp.sign("ags_x", "info", &args);
    assert!(d.serve(&once, &args, &cancel).is_ok());
    assert_eq!(d.serve(&once, &args, &cancel).unwrap_err().code, "REPLAYED");
    // Unknown action (signed): refused after the checks.
    assert_eq!(
        code(call(&d, &cp, "format_disk", json!({}))),
        "UNKNOWN_ACTION"
    );
    // Every one of them is in the audit log, and the chain holds.
    let entries = agentrouter_device::audit::verify(&d.audit_path()).unwrap();
    assert!(entries >= 9, "{entries}");
}

#[test]
fn info_reports_the_local_choices() {
    let cp = ControlPlane::new();
    let data = temp_dir("info");
    let folder = temp_dir("info-folder");
    let d = device(&data, Access::Folders, &[&folder], deny_all(), &cp);
    let v = call(&d, &cp, "info", json!({})).unwrap();
    assert_eq!(v["device"]["access"], "folders");
    assert_eq!(v["device"]["maxJobs"], 4);
    assert_eq!(
        v["device"]["shell"],
        if cfg!(windows) { "powershell" } else { "sh" }
    );
    assert_eq!(v["device"]["folders"].as_array().unwrap().len(), 1);
    assert!(v["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn full_access_runs_commands_and_files_anywhere_but_the_app_data() {
    let cp = ControlPlane::new();
    let data = temp_dir("full");
    let elsewhere = temp_dir("full-elsewhere");
    let asked = Arc::new(AtomicUsize::new(0));
    let d = device(
        &data,
        Access::Full,
        &[],
        confirm_with(true, asked.clone()),
        &cp,
    );
    let v = call(&d, &cp, "exec", json!({"command": echo("你好 device"), "cwd": elsewhere.display().to_string(), "timeout": 30})).unwrap();
    assert_eq!(v["status"], "exited", "{v}");
    assert_eq!(v["exitCode"], 0);
    assert!(v["output"].as_str().unwrap().contains("你好 device"), "{v}");
    // Full access asks once per conversation, then trusts it.
    call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("again"), "timeout": 30}),
    )
    .unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    let file = elsewhere.join("note.txt");
    let w = call(&d, &cp, "write_file", json!({"path": file.display().to_string(), "content": "hello", "encoding": "utf8", "append": false})).unwrap();
    assert_eq!(w["size"], 5);
    let r = call(&d, &cp, "read_file", json!({"path": file.display().to_string(), "offset": 0, "limit": 262144, "encoding": "utf8"})).unwrap();
    assert_eq!(r["content"], "hello");
    assert_eq!(r["eof"], true);
    // The app's own data is never reachable through the file tools, even with full access.
    let audit = d.audit_path();
    assert_eq!(
        code(call(
            &d,
            &cp,
            "read_file",
            json!({"path": audit.display().to_string(), "offset": 0, "limit": 100, "encoding": "utf8"})
        )),
        "DENIED"
    );
}

#[test]
fn folders_confine_files_and_run_commands_without_asking() {
    let cp = ControlPlane::new();
    let data = temp_dir("folders");
    let folder = temp_dir("folders-allowed");
    let outside = temp_dir("folders-outside");
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let d = device(
        &data,
        Access::Folders,
        &[&folder],
        confirm_with(false, asked.clone()),
        &cp,
    );
    let read = |p: &std::path::Path| json!({"path": p.display().to_string(), "offset": 0, "limit": 1000, "encoding": "utf8"});

    // Inside: fine, no question for files.
    let inside = folder.join("sub").join("a.txt");
    call(&d, &cp, "write_file", json!({"path": inside.display().to_string(), "content": "x", "encoding": "utf8", "append": false})).unwrap();
    assert_eq!(
        call(&d, &cp, "read_file", read(&inside)).unwrap()["content"],
        "x"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    // Outside, directly and with `..`.
    assert_eq!(
        code(call(
            &d,
            &cp,
            "read_file",
            read(&outside.join("secret.txt"))
        )),
        "DENIED"
    );
    let dotdot = format!(
        "{}{}..{}{}{}secret.txt",
        folder.display(),
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR,
        outside.file_name().unwrap().to_string_lossy(),
        std::path::MAIN_SEPARATOR
    );
    assert_eq!(
        code(call(
            &d,
            &cp,
            "read_file",
            json!({"path": dotdot, "offset": 0, "limit": 10, "encoding": "utf8"})
        )),
        "DENIED"
    );
    // A link inside the folder that points outside does not lead out.
    let link = folder.join("escape");
    let made = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link).is_ok()
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    assert!(made, "could not make the test link");
    assert_eq!(
        code(call(&d, &cp, "read_file", read(&link.join("secret.txt")))),
        "DENIED"
    );
    assert_eq!(
        code(call(
            &d,
            &cp,
            "write_file",
            json!({"path": link.join("planted.txt").display().to_string(), "content": "x", "encoding": "utf8", "append": false})
        )),
        "DENIED"
    );
    assert!(!outside.join("planted.txt").exists());
    // The default level: commands inside the folders run without asking (owner decision 2026-10-10).
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": echo("inside"), "timeout": 30}),
    )
    .unwrap();
    assert!(v["output"].as_str().unwrap().contains("inside"), "{v}");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    // A working folder outside is refused.
    assert_eq!(
        code(call(
            &d,
            &cp,
            "exec",
            json!({"command": echo("hi"), "cwd": outside.display().to_string(), "timeout": 5})
        )),
        "DENIED"
    );
    // A command that clearly writes outside is refused without a question, and nothing ran.
    let target = outside.join("planted2.txt");
    let overreach = if cfg!(windows) {
        format!("Set-Content -Path '{}' -Value x", target.display())
    } else {
        format!("echo x > '{}'", target.display())
    };
    let r = call(
        &d,
        &cp,
        "exec",
        json!({"command": overreach, "timeout": 10}),
    );
    assert_eq!(r.as_ref().unwrap_err().0, "OUT_OF_SCOPE", "{r:?}");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert!(!target.exists());
    // Saying why it must go beyond asks once; declined is DENIED_BY_USER.
    assert_eq!(
        code(call(
            &d,
            &cp,
            "exec",
            json!({"command": overreach, "timeout": 10, "beyondScope": "test"})
        )),
        "DENIED_BY_USER"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(!target.exists());

    // Approved: it runs.
    let yes = Arc::new(AtomicUsize::new(0));
    let d2 = device(
        &temp_dir("folders2"),
        Access::Folders,
        &[&folder],
        confirm_with(true, yes.clone()),
        &cp,
    );
    let v = call(
        &d2,
        &cp,
        "exec",
        json!({"command": overreach, "timeout": 30, "beyondScope": "the user asked for it"}),
    )
    .unwrap();
    assert_eq!(v["status"], "exited", "{v}");
    assert!(v["approval"].as_str().unwrap().starts_with("apv_"), "{v}");
    assert_eq!(yes.load(Ordering::SeqCst), 1);
    assert!(target.exists());
}

#[test]
fn readonly_reads_only() {
    let cp = ControlPlane::new();
    let folder = temp_dir("ro-folder");
    std::fs::write(folder.join("a.txt"), "read me").unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let d = device(
        &temp_dir("ro"),
        Access::Readonly,
        &[&folder],
        confirm_with(true, asked.clone()),
        &cp,
    );
    let p = folder.join("a.txt").display().to_string();
    assert_eq!(
        call(
            &d,
            &cp,
            "read_file",
            json!({"path": p, "offset": 5, "limit": 100, "encoding": "utf8"})
        )
        .unwrap()["content"],
        "me"
    );
    assert_eq!(
        code(call(
            &d,
            &cp,
            "write_file",
            json!({"path": p, "content": "x", "encoding": "utf8", "append": false})
        )),
        "DENIED"
    );
    assert_eq!(
        code(call(
            &d,
            &cp,
            "exec",
            json!({"command": echo("hi"), "timeout": 5})
        )),
        "DENIED"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 0, "read-only never even asks");
    assert_eq!(
        std::fs::read_to_string(folder.join("a.txt")).unwrap(),
        "read me"
    );
}

#[test]
fn confirm_asks_for_every_write() {
    let cp = ControlPlane::new();
    let folder = temp_dir("confirm-folder");
    let asked = Arc::new(AtomicUsize::new(0));
    let d = device(
        &temp_dir("confirm"),
        Access::Confirm,
        &[&folder],
        confirm_with(false, asked.clone()),
        &cp,
    );
    let p = folder.join("b.txt").display().to_string();
    assert_eq!(
        code(call(
            &d,
            &cp,
            "write_file",
            json!({"path": p, "content": "x", "encoding": "utf8", "append": false})
        )),
        "DENIED_BY_USER"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(!folder.join("b.txt").exists());
}

#[cfg(windows)]
#[test]
fn windows_path_tricks_are_refused() {
    let cp = ControlPlane::new();
    let folder = temp_dir("tricks");
    std::fs::write(folder.join("f.txt"), "x").unwrap();
    let d = device(
        &temp_dir("tricks-data"),
        Access::Folders,
        &[&folder],
        deny_all(),
        &cp,
    );
    let base = folder.display().to_string();
    for raw in [
        format!(r"\\?\{base}\f.txt"),
        format!(r"\\.\{base}\f.txt"),
        r"\\localhost\C$\Windows\win.ini".to_string(),
        format!(r"{base}\f.txt:stream"),
        format!(r"{base}\NUL"),
        format!(r"{base}\con.txt"),
        format!(r"{base}\f.txt."),
        format!(r"{base}\f.txt "),
    ] {
        let r = call(
            &d,
            &cp,
            "read_file",
            json!({"path": raw, "offset": 0, "limit": 10, "encoding": "utf8"}),
        );
        assert_eq!(code(r), "DENIED", "{raw}");
    }
    // A short (8.3) or differently cased name of the allowed folder is still the allowed folder.
    let upper = base.to_uppercase();
    assert!(
        call(
            &d,
            &cp,
            "read_file",
            json!({"path": format!(r"{upper}\F.TXT"), "offset": 0, "limit": 10, "encoding": "utf8"})
        )
        .is_ok()
    );
}

#[test]
fn background_jobs_wait_and_die_as_a_tree() {
    let cp = ControlPlane::new();
    let d = device(
        &temp_dir("jobs"),
        Access::Full,
        &[],
        confirm_with(true, Arc::new(AtomicUsize::new(0))),
        &cp,
    );
    let started = std::time::Instant::now();
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": long_tree(60), "timeout": 0}),
    )
    .unwrap();
    assert_eq!(v["status"], "running", "{v}");
    let job = v["job"].as_str().unwrap().to_string();
    assert!(started.elapsed().as_secs() < 10);
    // It is listed, and a short wait returns while it still runs.
    let info = call(&d, &cp, "info", json!({})).unwrap();
    assert_eq!(info["jobs"][0]["job"], job.as_str());
    assert_eq!(info["jobs"][0]["mine"], true);
    let w = call(
        &d,
        &cp,
        "job",
        json!({"job": job, "action": "wait", "timeout": 1}),
    )
    .unwrap();
    assert_eq!(w["status"], "running");
    // Kill ends the whole tree promptly.
    let k = call(
        &d,
        &cp,
        "job",
        json!({"job": job, "action": "kill", "timeout": 0}),
    )
    .unwrap();
    assert_eq!(k["status"], "killed", "{k}");
    assert!(started.elapsed().as_secs() < 30);
    assert_eq!(
        code(call(
            &d,
            &cp,
            "job",
            json!({"job": "job_nope", "action": "output", "timeout": 0})
        )),
        "JOB_NOT_FOUND"
    );
}

#[test]
fn at_most_four_jobs_run_at_once() {
    let cp = ControlPlane::new();
    let d = device(
        &temp_dir("busy"),
        Access::Full,
        &[],
        confirm_with(true, Arc::new(AtomicUsize::new(0))),
        &cp,
    );
    let sleep = if cfg!(windows) {
        "Start-Sleep 30"
    } else {
        "sleep 30"
    };
    for _ in 0..4 {
        assert_eq!(
            call(&d, &cp, "exec", json!({"command": sleep, "timeout": 0})).unwrap()["status"],
            "running"
        );
    }
    assert_eq!(
        code(call(
            &d,
            &cp,
            "exec",
            json!({"command": sleep, "timeout": 0})
        )),
        "BUSY"
    );
    d.stop_all("test");
}

#[test]
fn output_is_read_back_in_pieces() {
    let cp = ControlPlane::new();
    let d = device(
        &temp_dir("output"),
        Access::Full,
        &[],
        confirm_with(true, Arc::new(AtomicUsize::new(0))),
        &cp,
    );
    let cmd = if cfg!(windows) {
        "1..3000 | ForEach-Object { \"line $_ 中文\" }"
    } else {
        "i=1; while [ $i -le 3000 ]; do echo \"line $i 中文\"; i=$((i+1)); done"
    };
    let v = call(&d, &cp, "exec", json!({"command": cmd, "timeout": 30})).unwrap();
    assert_eq!(v["status"], "exited", "{v}");
    let mut all = v["output"].as_str().unwrap().to_string();
    let mut offset = v["offset"].as_u64().unwrap();
    loop {
        let more = call(
            &d,
            &cp,
            "job",
            json!({"job": v["job"], "action": "output", "offset": offset, "timeout": 0}),
        )
        .unwrap();
        let chunk = more["output"].as_str().unwrap();
        if chunk.is_empty() {
            break;
        }
        all.push_str(chunk);
        offset = more["offset"].as_u64().unwrap();
    }
    assert!(all.contains("line 1 中文") && all.contains("line 3000 中文"));
    assert!(!all.contains('\u{FFFD}'), "a character was cut in half");
}
