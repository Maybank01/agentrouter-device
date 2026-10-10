//! The OS write boundary at the folder levels on Windows (confine.rs): whatever the command text says,
//! a command can write only inside the linked folders (and its scratch folder). Dev check 2026-10-11:
//! the same `[IO.File]::WriteAllText` outside the folders ran once and was refused once, because the
//! text check only saw paths spelled out in the command. Every write here hides its target from the
//! text check, and each is tried twice: the answer must be the same both times.
#![cfg(windows)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use agentrouter_device::config::Access;
use agentrouter_device::confine;
use common::*;
use serde_json::{Value, json};

fn run(d: &agentrouter_device::device::Device, cp: &ControlPlane, command: &str) -> Value {
    let v = call(d, cp, "exec", json!({"command": command, "timeout": 60}))
        .unwrap_or_else(|e| panic!("{command}: {e:?}"));
    assert_eq!(v["status"], "exited", "{command}: {v}");
    v
}

/// A PowerShell single-quoted string.
fn ps(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The 8.3 short name of `path`, if the volume keeps them.
fn short_name(path: &Path) -> Option<String> {
    let out = std::process::Command::new("cmd")
        .args(["/c", "for", "%I", "in", "(\"."])
        .arg(format!("{}\") do @echo %~sI", path.display()))
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (text.contains('~')).then_some(text)
}

fn have(program: &str) -> bool {
    std::process::Command::new("where")
        .arg(program)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn writes_outside_the_folders_fail_every_time_whatever_the_text_says() {
    let cp = ControlPlane::new();
    let base = temp_dir("confine");
    let folder = base.join("linked");
    let outside = base.join("outside-the-linked-folder");
    std::fs::create_dir_all(folder.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "keep").unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let data = temp_dir("confine-data");
    // A runner's work folder may already be Low (inherited): then the app labels nothing and takes nothing back.
    let low_before = confine::labelled(&folder);
    let d = device(
        &data,
        Access::Folders,
        &[&folder],
        confirm_with(false, asked.clone()),
        &cp,
    );

    // Inside: writing, creating folders, deleting and the temporary folder all work, without a question.
    let v = run(
        &d,
        &cp,
        "[IO.File]::WriteAllText((Join-Path (Get-Location) 'in.txt'), 'x'); New-Item -ItemType Directory build | Out-Null; Remove-Item in.txt; [IO.File]::WriteAllText((Join-Path $env:TEMP 'ar-confine-probe.txt'), 'x'); 'inside ok'",
    );
    assert!(v["output"].as_str().unwrap().contains("inside ok"), "{v}");
    assert!(folder.join("build").is_dir());
    assert!(!folder.join("in.txt").exists());
    assert!(confine::labelled(&folder));
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 0);

    // A script file the command only reads (the text check sees no path at all).
    let script = |name: &str, body: &str| {
        std::fs::write(folder.join(name), body).unwrap();
        format!("Invoke-Expression (Get-Content -Raw .\\{name})")
    };
    let target = |n: usize| outside.join(format!("planted{n}.txt"));
    let mut attempts: Vec<(usize, String)> = vec![
        // .NET call to a path built at run time (the Dev check's shape).
        (
            1,
            "[IO.File]::WriteAllText((Join-Path (Split-Path (Get-Location)) 'outside-the-linked-folder\\planted1.txt'), 'x')"
                .to_string(),
        ),
        // Relative.
        (
            2,
            script(
                "rel.txt",
                "[IO.File]::WriteAllText((Join-Path (Get-Location) '..\\outside-the-linked-folder\\planted2.txt'), 'x')",
            ),
        ),
        // Absolute, PowerShell cmdlet.
        (
            3,
            script(
                "abs.txt",
                &format!("Set-Content -Path {} -Value x", ps(&target(3).display().to_string())),
            ),
        ),
    ];
    if let Some(short) = short_name(&outside) {
        attempts.push((
            4,
            script(
                "short.txt",
                &format!(
                    "[IO.File]::WriteAllText({}, 'x')",
                    ps(&format!("{short}\\planted4.txt"))
                ),
            ),
        ));
    }
    // A junction inside the folder that leads outside.
    let junction = folder.join("src").join("door");
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(made, "mklink /J");
    attempts.push((
        5,
        "Set-Content -Path src\\door\\planted5.txt -Value x".to_string(),
    ));
    // A symbolic link, where this account may make one (developer mode or admin).
    if std::os::windows::fs::symlink_dir(&outside, folder.join("sym")).is_ok() {
        attempts.push((
            6,
            "Set-Content -Path sym\\planted6.txt -Value x".to_string(),
        ));
    }
    if have("python") {
        std::fs::write(
            folder.join("w.py"),
            "import os\nopen(os.path.join('..', 'outside-the-linked-folder', 'planted7.txt'), 'w').write('x')\n",
        )
        .unwrap();
        attempts.push((7, "python w.py".to_string()));
    }
    if have("node") {
        std::fs::write(
            folder.join("w.js"),
            "require('fs').writeFileSync(require('path').join('..', 'outside-the-linked-folder', 'planted8.txt'), 'x')\n",
        )
        .unwrap();
        attempts.push((8, "node w.js".to_string()));
    }
    // Deleting and renaming outside.
    attempts.push((
        9,
        script(
            "del.txt",
            &format!(
                "Remove-Item -Force {}; Move-Item {} {}",
                ps(&outside.join("keep.txt").display().to_string()),
                ps(&outside.join("keep.txt").display().to_string()),
                ps(&folder.join("stolen.txt").display().to_string()),
            ),
        ),
    ));
    // The registry outside AppDataLow.
    attempts.push((
        10,
        script(
            "reg.txt",
            "New-Item -Path ('HK' + 'CU:\\Software\\AgentRouterConfineProbe') | Out-Null",
        ),
    ));

    eprintln!(
        "attempts: {:?}",
        attempts.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    for (n, command) in &attempts {
        for round in 1..=2 {
            let v = run(&d, &cp, command);
            let output = v["output"].as_str().unwrap_or("").to_lowercase();
            assert!(
                output.contains("denied")
                    || output.contains("拒绝")
                    || output.contains("eperm")
                    || output.contains("permission"),
                "#{n} round {round} should be refused by Windows: {command}: {v}"
            );
            assert!(
                !target(*n).exists(),
                "#{n} round {round} wrote outside: {command}: {v}"
            );
        }
    }
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep"
    );
    assert!(!folder.join("stolen.txt").exists());
    let probe = std::process::Command::new("reg")
        .args(["query", "HKCU\\Software\\AgentRouterConfineProbe"])
        .output()
        .unwrap();
    assert!(!probe.status.success(), "registry key was created");

    // Typed into a running shell (`job input`), past the text check entirely.
    let v = call(
        &d,
        &cp,
        "exec",
        json!({"command": "powershell -NoProfile -Command -", "timeout": 0}),
    )
    .unwrap();
    let job = v["job"].as_str().unwrap().to_string();
    let typed = format!(
        "[IO.File]::WriteAllText({}, 'x')\n",
        ps(&target(11).display().to_string())
    );
    call(
        &d,
        &cp,
        "job",
        json!({"job": job, "action": "input", "input": typed}),
    )
    .unwrap();
    call(
        &d,
        &cp,
        "job",
        json!({"job": job, "action": "input", "input": "exit\n"}),
    )
    .unwrap();
    let v = call(
        &d,
        &cp,
        "job",
        json!({"job": job, "action": "wait", "timeout": 60}),
    )
    .unwrap();
    assert_ne!(v["status"], "running", "{v}");
    assert!(!target(11).exists(), "job input wrote outside: {v}");

    // Unlinking takes the label back.
    let failed = confine::release(&data, &[]);
    let acl = std::process::Command::new("icacls")
        .arg(&folder)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let record = std::fs::read_to_string(data.join("confined.json")).unwrap_or_default();
    assert_eq!(
        confine::labelled(&folder),
        low_before,
        "label after release; failed: {failed:?}; icacls: {acl}; record: {record}"
    );
    let _ = std::fs::remove_dir(&junction);
}
