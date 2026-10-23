//! The environment's resource limits hold on a server's tree (device #127): a command that keeps allocating memory is
//! stopped near the limit, a process that keeps starting processes gets no more than the limit, and the limit is
//! reported, on the confined tree through the real sandbox launcher (srt) on all three OSes. On Windows srt-win starts
//! that tree through Secondary Logon, out of hostd's reach, so the Job object is made named before the start and the
//! sandbox entry joins it from inside (device #129).
//!
//! Runs only with `AGENTROUTER_SANDBOX_TESTS=1` (the `Sandbox (…)` CI jobs on GitHub-hosted runners): it provisions the
//! sandbox and starts hungry processes. Never run it on a person's computer.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentrouter_device::limits::{self, Enforced, Limits};
use agentrouter_sandbox::{Launch, Runtime};

fn enabled() -> bool {
    std::env::var("AGENTROUTER_SANDBOX_TESTS").is_ok_and(|v| v == "1")
}

fn test_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

fn real(path: &Path) -> PathBuf {
    let p = std::fs::canonicalize(path).unwrap();
    let text = p.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_string())
}

/// `memory`: a child allocates 64 MB every 500 ms up to 2 GB, printing each step (slow enough for a 2 s sampler); the
/// parent stays 4 s after it ends.
/// `processes`: start 80 node children (each waits 20 s), then print how many are alive. `pid`: print its own pid, wait 5 s.
const PROBE: &str = r#"
const { spawn } = require('child_process');
const mode = process.argv[2];
if (mode === 'pid') {
  console.log(JSON.stringify({ pid: process.pid, ppid: process.ppid }));
  setTimeout(() => {}, 5000);
} else if (mode === 'memory') {
  // As with the host server, the hungry process is a command it started; the server stays and says how it ended.
  const k = spawn(process.execPath, [__filename, 'memory-child'], { stdio: 'inherit' });
  k.on('exit', (code, signal) => {
    console.log('child-exit ' + code + ' ' + signal);
    setTimeout(() => process.exit(0), 4000);
  });
} else if (mode === 'memory-child') {
  const keep = [];
  let mb = 0;
  const step = () => {
    try { keep.push(Buffer.alloc(64 << 20, 1)); } catch (e) { console.log('alloc-failed ' + mb + ' ' + (e.code || e.message)); process.exit(3); }
    mb += 64;
    console.log('allocated ' + mb);
    if (mb >= 2048) { console.log('done'); process.exit(0); }
    setTimeout(step, 500);
  };
  step();
} else if (mode === 'processes') {
  const kids = [];
  let failed = 0;
  let i = 0;
  const next = () => {
    if (i++ >= 80) {
      setTimeout(() => {
        const alive = kids.filter(k => k.exitCode === null && k.signalCode === null).length;
        console.log(JSON.stringify({ alive, failed }));
        for (const k of kids) { try { k.kill(); } catch {} }
        process.exit(0);
      }, 4000);
      return;
    }
    try {
      const k = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 20000)'], { stdio: 'ignore' });
      k.on('error', () => { failed++; });
      kids.push(k);
    } catch (e) { failed++; }
    setTimeout(next, 50);
  };
  next();
}
"#;

struct Run {
    stdout: String,
    stderr: String,
    events: Vec<String>,
}

fn run(rt: &Runtime, node: &Path, folder: &Path, mode: &str, l: Limits, confined: bool) -> Run {
    let probe = folder.join("limits-probe.js");
    std::fs::write(&probe, PROBE).unwrap();
    let args = vec![probe.to_string_lossy().into_owned(), mode.to_string()];
    let env = vec![(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )];
    let folders = vec![folder.to_path_buf()];
    let key = format!("limits-{mode}");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let on_limit: limits::OnLimit = Arc::new(move |line: String| sink.lock().unwrap().push(line));
    // Windows folder mode: the named job, limits set, before the start; the entry joins it (#129).
    #[cfg(windows)]
    let named = confined.then(|| {
        let sid = rt.windows_sid().expect("the sandbox account");
        Enforced::named(&sid, l, on_limit.clone()).expect("the named job")
    });
    #[cfg(not(windows))]
    let named: Option<(Enforced, String)> = None;
    // The device program that joins it (CI builds the CLI first and names it).
    let joiner = std::env::var_os("AGENTROUTER_DEVICE_EXE").map(PathBuf::from);
    let launch = Launch {
        key: &key,
        program: node,
        args: &args,
        env: &env,
        cwd: folder,
        folders: &folders,
        network: &[],
        readable: &[],
        read_only: false,
        open_web: false,
        job: named
            .as_ref()
            .map(|(_, name)| agentrouter_sandbox::JobJoin {
                name,
                program: joiner.as_deref().expect("AGENTROUTER_DEVICE_EXE"),
            }),
    };
    let unit = format!("agentrouter-test-{mode}-{}", std::process::id());
    #[cfg_attr(not(unix), allow(unused_mut))]
    let base = if confined {
        rt.command(&launch).expect("confined command")
    } else {
        let mut c = std::process::Command::new(node);
        c.args(&args).current_dir(folder);
        c
    };
    let mut cmd = limits::command(base, &unit, &l);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let enforced = match named {
        Some((enforced, _)) => enforced,
        None => Enforced::start(child.id(), &unit, l, on_limit).expect("limits enforced"),
    };
    let out = child.wait_with_output().expect("wait");
    // The watcher reports within one sample.
    std::thread::sleep(limits::SAMPLE + Duration::from_millis(500));
    drop(enforced);
    let r = Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        events: events.lock().unwrap().clone(),
    };
    eprintln!(
        "== {mode} under {} ({}, confined {confined}): exit {:?}\nstdout: {}\nstderr: {}\nevents: {:?}",
        l.describe(),
        limits::mechanism().name(),
        out.status,
        r.stdout.trim(),
        r.stderr.trim(),
        r.events
    );
    r
}

#[test]
fn limits_hold_on_a_confined_tree() {
    if !enabled() {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    eprintln!("mechanism: {:?}", limits::mechanism());
    let base = test_root().join(format!("agentrouter-limits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("data")).unwrap();
    std::fs::create_dir_all(base.join("work")).unwrap();
    let base = real(&base);
    let rt = Runtime::new(&base.join("data"));
    rt.setup().expect("sandbox setup on the runner");
    let node = rt.node().expect("the device's node");
    let folder = base.join("work");

    // Inside the folder sandbox on every OS (Windows through the named job, #129).
    let confined = true;

    let memory = Limits {
        memory_mb: Some(512),
        ..Limits::default()
    };
    let r = run(&rt, &node, &folder, "memory", memory, confined);
    let reached = r
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("allocated ")?.trim().parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    // The probe ran at all: a confined start that failed (srt's own fence check, Cloud #2383) is named as that, not as
    // a limit that went unreported.
    assert!(
        reached > 0,
        "the confined probe never started: {}",
        r.stderr.trim()
    );
    assert!(
        !r.stdout.contains("done"),
        "2 GB allocated under a 512 MB limit"
    );
    assert!(
        reached < 1024,
        "allocated {reached} MB under a 512 MB limit"
    );
    assert!(
        !r.events.is_empty(),
        "reaching the memory limit is reported: {:?}",
        r.events
    );

    let processes = Limits {
        processes: Some(12),
        ..Limits::default()
    };
    let r = run(&rt, &node, &folder, "processes", processes, confined);
    let line = r.stdout.lines().find(|l| l.starts_with('{'));
    if let Some(line) = line {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let alive = v["alive"].as_u64().unwrap();
        assert!(
            alive < 80,
            "all 80 children alive under a 12-process limit: {v}"
        );
    }
    assert!(
        !r.events.is_empty(),
        "reaching the process limit is reported: {:?}",
        r.events
    );
    let _ = std::fs::remove_dir_all(&base);
}
