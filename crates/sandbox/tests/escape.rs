//! The OS boundary itself (issue #42): a confined process — and a child process it starts — cannot write, delete
//! or plant anything outside the linked folder, cannot read `~/.ssh`, and cannot open a direct connection.
//!
//! Runs only with `AGENTROUTER_SANDBOX_TESTS=1` (the `Sandbox (…)` CI jobs on GitHub-hosted Windows, macOS and
//! Linux runners): it installs the pinned srt with npm, provisions the Windows sandbox account (elevated) and
//! writes probe files into the runner's home. Never run it on a person's computer.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use agentrouter_sandbox::{Launch, Runtime};
use serde_json::{Value, json};

/// Where the device keeps its data on a person's PC: on Windows `%LOCALAPPDATA%`, which srt's profile read grant
/// passes through (the temp folder it leaves unreadable to the sandbox account, device #66); elsewhere the temp folder.
fn test_root() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local);
    }
    std::env::temp_dir()
}

fn enabled() -> bool {
    std::env::var("AGENTROUTER_SANDBOX_TESTS").is_ok_and(|v| v == "1")
}

fn real(path: &Path) -> PathBuf {
    let p = std::fs::canonicalize(path).unwrap();
    let text = p.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_string())
}

fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    PathBuf::from(std::env::var_os(var).expect("home"))
}

const PROBE: &str = r#"
const fs = require('fs');
const path = require('path');
const net = require('net');
const { spawnSync } = require('child_process');
const c = JSON.parse(process.argv[2]);
const r = {};
function attempt(name, f) {
  try { f(); r[name] = 'ok'; } catch (e) { r[name] = 'denied: ' + (e.code || e.message); }
}
attempt('insideWrite', () => fs.writeFileSync(path.join(c.folder, 'inside.txt'), 'written in the sandbox'));
// The temp folder a confined process is handed is writable (the host server keeps terminal command files there, #130).
attempt('tmpWrite', () => { const d = path.join(require('os').tmpdir(), 'agentrouter-run'); fs.mkdirSync(d, { recursive: true }); fs.writeFileSync(path.join(d, 'probe.txt'), 'x'); });
r.tmpdir = require('os').tmpdir();
attempt('insideDelete', () => fs.unlinkSync(path.join(c.folder, 'delete-me.txt')));
attempt('outsideWrite', () => fs.writeFileSync(path.join(c.outside, 'new.txt'), 'escaped'));
attempt('outsideOverwrite', () => fs.writeFileSync(path.join(c.outside, 'victim.txt'), 'escaped'));
attempt('outsideDelete', () => fs.unlinkSync(path.join(c.outside, 'victim.txt')));
attempt('outsideRmTree', () => fs.rmSync(path.join(c.outside, 'dir'), { recursive: true, force: false }));
attempt('outsideRename', () => fs.renameSync(path.join(c.outside, 'victim.txt'), path.join(c.folder, 'stolen.txt')));
attempt('homeDotDelete', () => fs.rmSync(c.homeDot, { recursive: true, force: false }));
attempt('homeDotWrite', () => fs.writeFileSync(path.join(c.homeDot, 'new.txt'), 'escaped'));
attempt('hookPlant', () => fs.writeFileSync(path.join(c.folder, '.git', 'hooks', 'pre-commit'), '#!/bin/sh\necho planted\n'));
attempt('gitConfig', () => fs.appendFileSync(path.join(c.folder, '.git', 'config'), '[core]\n\thooksPath = /tmp\n'));
attempt('dataWrite', () => fs.writeFileSync(path.join(c.data, 'config.json'), '{"unattended":true}'));
let ssh = 'unread';
try { ssh = fs.readFileSync(c.ssh, 'utf8'); } catch (e) { ssh = 'denied: ' + (e.code || e.message); }
r.sshRead = ssh === c.secret ? 'read' : ssh;
const child = path.join(c.outside, 'child.txt');
const sh = process.platform === 'win32'
  ? spawnSync(process.env.ComSpec || 'cmd.exe', ['/d', '/c', `echo escaped> "${child}"`])
  : spawnSync('/bin/sh', ['-c', `echo escaped > '${child}'`]);
r.childShell = sh.status;
const s = net.connect({ host: '1.1.1.1', port: 443 });
const done = v => { r.directConnect = v; s.destroy(); console.log(JSON.stringify(r)); process.exit(0); };
s.setTimeout(8000, () => done('timeout'));
s.on('connect', () => done('connected'));
s.on('error', e => done('denied: ' + (e.code || e.message)));
"#;

/// Asks srt's proxy (the only way out) to open tunnels and reports each answer: 200 = connected, 403 = refused.
const NET_PROBE: &str = r#"
const http = require('http');
const targets = JSON.parse(process.argv[2]);
const raw = process.env.HTTPS_PROXY || process.env.https_proxy || process.env.HTTP_PROXY || process.env.http_proxy;
const proxy = new URL(raw);
const headers = {};
if (proxy.username) {
  const id = decodeURIComponent(proxy.username) + ':' + decodeURIComponent(proxy.password);
  headers['Proxy-Authorization'] = 'Basic ' + Buffer.from(id).toString('base64');
}
const ask = target => new Promise(done => {
  const req = http.request({ host: proxy.hostname, port: proxy.port, method: 'CONNECT', path: target, headers, timeout: 20000 });
  req.on('connect', (res, socket) => { socket.destroy(); done(res.statusCode); });
  req.on('response', res => { res.resume(); done(res.statusCode); });
  req.on('error', e => done('error: ' + (e.code || e.message)));
  req.on('timeout', () => { req.destroy(); done('timeout'); });
  req.end();
});
(async () => {
  const r = {};
  for (const t of targets) r[t] = await ask(t);
  console.log(JSON.stringify(r));
})();
"#;

/// Owner 2026-10-20 (browser presets): the open web is any public host, never this computer, its LAN or the cloud
/// metadata endpoint; a launch without it still reaches only its list.
fn the_open_web_stays_off_this_computer_and_the_lan(rt: &Runtime, node: &Path, folder: &Path) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || for _ in listener.incoming() {});
    std::fs::write(folder.join("net-probe.js"), NET_PROBE).unwrap();
    let public = "example.com:443";
    let closed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
        "169.254.169.254:80".to_string(),
        "2852039166:80".to_string(),
        "10.0.0.1:80".to_string(),
        "192.168.0.1:80".to_string(),
    ];
    let mut targets = vec![public.to_string()];
    targets.extend(closed.iter().cloned());
    let args = vec![
        folder.join("net-probe.js").to_string_lossy().into_owned(),
        serde_json::to_string(&targets).unwrap(),
    ];
    let env = vec![(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )];
    let folders = vec![folder.to_path_buf()];
    let network = vec!["*".to_string()];
    for open_web in [true, false] {
        let launch = Launch {
            key: if open_web { "net-open" } else { "net-closed" },
            program: node,
            args: &args,
            env: &env,
            cwd: folder,
            folders: &folders,
            network: &network,
            readable: &[],
            read_only: false,
            open_web,
            job: None,
        };
        let out = rt
            .command(&launch)
            .expect("confined command")
            .stdin(Stdio::null())
            .output()
            .expect("srt runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        eprintln!(
            "open_web={open_web} exit {:?}\nstdout: {stdout}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let r: Value = stdout
            .lines()
            .rev()
            .find_map(|l| serde_json::from_str(l).ok())
            .expect("the network probe reported");
        if open_web {
            assert_eq!(r[public], 200, "the open web is reachable: {r}");
        } else {
            assert_ne!(r[public], 200, "without the open web only the list is: {r}");
        }
        for t in &closed {
            assert_ne!(r[t.as_str()], 200, "{t} must stay closed: {r}");
        }
    }
}

#[test]
fn a_confined_process_cannot_touch_anything_outside_the_folder() {
    if !enabled() {
        eprintln!("skipped: set AGENTROUTER_SANDBOX_TESTS=1 (CI runners only)");
        return;
    }
    let base = test_root().join(format!("agentrouter-sbx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let base = real(&base);
    let data = base.join("data");
    let folder = base.join("work");
    let outside = base.join("outside");
    for d in [&data, &folder, &outside, &outside.join("dir")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(folder.join(".git").join("hooks")).unwrap();
    std::fs::write(folder.join(".git").join("config"), "[core]\n").unwrap();
    std::fs::write(folder.join("delete-me.txt"), "x").unwrap();
    std::fs::write(outside.join("victim.txt"), "keep").unwrap();
    std::fs::write(outside.join("dir").join("keep.txt"), "keep").unwrap();
    std::fs::write(data.join("config.json"), "{}").unwrap();
    // The 2026-10-18 incident: dot-folders in the home directory deleted by an unconfined process.
    let home_dot = home().join(format!(".agentrouter-sbx-{}", std::process::id()));
    std::fs::create_dir_all(&home_dot).unwrap();
    std::fs::write(home_dot.join("keep.txt"), "keep").unwrap();
    let ssh_dir = home().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    let ssh = ssh_dir.join(format!("agentrouter-sbx-{}", std::process::id()));
    let secret = format!("secret-{}", std::process::id());
    std::fs::write(&ssh, &secret).unwrap();
    std::fs::write(folder.join("probe.js"), PROBE).unwrap();

    let rt = Runtime::new(&data);
    let done = rt.setup().expect("agentrouter sandbox setup");
    eprintln!("setup: {done:?}");
    let status = rt.status();
    assert!(status.ready, "sandbox not ready: {:?}", status.problems);

    // The device's own Node (#116): the runners have none on PATH.
    let node = rt.node().expect("the device's Node after setup");
    let config = json!({
        "folder": folder, "outside": outside, "homeDot": home_dot, "ssh": ssh, "secret": secret, "data": data,
    })
    .to_string();
    let args = vec![
        folder.join("probe.js").to_string_lossy().into_owned(),
        config,
    ];
    let path = std::env::var("PATH").unwrap_or_default();
    let env = vec![("PATH".to_string(), path)];
    let folders = vec![folder.clone()];
    let launch = Launch {
        key: "escape-test",
        program: &node,
        args: &args,
        env: &env,
        cwd: &folder,
        folders: &folders,
        network: &[],
        readable: &[],
        read_only: false,
        open_web: false,
        job: None,
    };
    eprintln!(
        "policy: {}",
        serde_json::to_string_pretty(&rt.policy(&launch).unwrap()).unwrap()
    );
    let out = rt
        .command(&launch)
        .expect("confined command")
        .stdin(Stdio::null())
        .output()
        .expect("srt runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let r: Value = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .expect("the probe reported");

    // The sandbox ran the probe and the folder is usable.
    assert_eq!(r["insideWrite"], "ok", "{r}");
    assert_eq!(
        r["tmpWrite"], "ok",
        "the temp folder the sandbox hands out is writable: {r}"
    );
    assert_eq!(r["insideDelete"], "ok", "{r}");
    assert!(folder.join("inside.txt").is_file());
    assert!(!folder.join("delete-me.txt").exists());
    // Everything outside is untouched — checked from outside the sandbox, not from what the probe says.
    assert_eq!(
        std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "keep",
        "{r}"
    );
    assert!(!outside.join("new.txt").exists(), "{r}");
    assert!(outside.join("dir").join("keep.txt").is_file(), "{r}");
    assert!(!folder.join("stolen.txt").exists(), "{r}");
    assert!(
        home_dot.join("keep.txt").is_file(),
        "home dot-folder deleted: {r}"
    );
    assert!(!home_dot.join("new.txt").exists(), "{r}");
    assert!(
        !outside.join("child.txt").exists(),
        "a child shell escaped: {r}"
    );
    assert!(
        !folder
            .join(".git")
            .join("hooks")
            .join("pre-commit")
            .exists(),
        "hook planted: {r}"
    );
    assert_eq!(
        std::fs::read_to_string(folder.join(".git").join("config")).unwrap(),
        "[core]\n",
        "{r}"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("config.json")).unwrap(),
        "{}",
        "{r}"
    );
    assert_ne!(r["sshRead"], "read", "~/.ssh was readable: {r}");
    assert_ne!(
        r["directConnect"], "connected",
        "a direct connection got out: {r}"
    );
    for k in [
        "outsideWrite",
        "outsideOverwrite",
        "outsideDelete",
        "outsideRmTree",
        "homeDotDelete",
        "hookPlant",
    ] {
        assert_ne!(r[k], "ok", "{k}: {r}");
    }

    the_open_web_stays_off_this_computer_and_the_lan(&rt, &node, &folder);

    let _ = std::fs::remove_file(&ssh);
    let _ = std::fs::remove_dir_all(&home_dot);
    let _ = std::fs::remove_dir_all(&base);
}
