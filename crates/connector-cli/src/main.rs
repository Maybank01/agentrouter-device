//! `agentrouter-device`: the linked-device connector on the command line (servers, headless machines,
//! CI). The desktop app (`app/`) embeds the same core with a tray and the cloud web UI.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentrouter_device::config::{Access, Config};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::net::{self, Runtime};
use agentrouter_device::util::{data_dir, home_dir, log};
use agentrouter_device::{audit, consent, gate, keystore, link};

const USAGE: &str = "AgentRouter 设备 (agentrouter-device)

用法：
  agentrouter-device [run]                   连上网关，等云端对话的请求（Ctrl+C 退出）
  agentrouter-device link [选项]             链接这台电脑（显示配对码）
      --access readonly|folders|confirm|full   访问级别（默认 readonly）
      --folder <路径>                        允许的文件夹，可写多次
      --name <名字>  --gateway <网址>  --yes（已在命令行里确认，不再弹窗）
      --unattended                           无人值守：这台机器的管理员在这里预先同意命令，
                                             运行时不再逐条确认（只用于服务器和 CI，所有操作照样记审计）
  agentrouter-device access <级别> [--folder <路径>]... [--yes]
  agentrouter-device status                  查看链接、访问级别和审计日志
  agentrouter-device unlink                  在本机删除这台设备的身份
  agentrouter-device audit verify            检查审计日志有没有被改过
";

#[derive(Default)]
struct Flags {
    gateway: Option<String>,
    name: Option<String>,
    access: Option<Access>,
    folders: Vec<String>,
    yes: bool,
    unattended: bool,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} 后面要跟一个值"));
        match arg.as_str() {
            "--gateway" => flags.gateway = Some(value("--gateway")?),
            "--name" => flags.name = Some(value("--name")?),
            "--access" => {
                let v = value("--access")?;
                flags.access = Some(Access::parse(&v).ok_or(format!("不认识的访问级别：{v}"))?);
            }
            "--folder" => flags.folders.push(value("--folder")?),
            "--yes" | "-y" => flags.yes = true,
            "--unattended" => flags.unattended = true,
            other if other.starts_with("--") => return Err(format!("不认识的选项：{other}")),
            other => flags.positional.push(other.to_string()),
        }
    }
    Ok(flags)
}

fn canonical_folders(folders: &[String]) -> Result<Vec<String>, String> {
    folders
        .iter()
        .map(|f| match std::fs::canonicalize(f) {
            Ok(p) if p.is_dir() => Ok(gate::display(&p)),
            _ => Err(format!("没有这个文件夹：{f}")),
        })
        .collect()
}

/// Ask on this computer: a native dialog on Windows, the terminal elsewhere.
fn confirm_locally(text: &str) -> bool {
    if cfg!(windows) {
        return consent::question(consent::TITLE, text, &AtomicBool::new(false));
    }
    println!("{text}\n\n继续？(y/N)");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.first() {
        Some(c) if !c.starts_with("--") => (c.as_str(), &args[1..]),
        _ => ("run", &args[..]),
    };
    let flags = match parse(rest) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let code = match command {
        "run" => run(flags),
        "link" => cmd_link(flags),
        "access" => cmd_access(flags),
        "status" => status(),
        "unlink" => {
            keystore::forget();
            println!("已在本机删除这台设备的身份。网页「我的设备」里的记录请在那里移除。");
            0
        }
        "audit" => match audit::verify(&data_dir().join("audit.log")) {
            Ok(n) => {
                println!("审计日志完好，共 {n} 条。");
                0
            }
            Err((line, why)) => {
                println!("审计日志第 {line} 行有问题：{why}");
                1
            }
        },
        "version" | "--version" | "-V" => {
            println!("agentrouter-device {}", env!("CARGO_PKG_VERSION"));
            0
        }
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("不认识的命令：{other}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

/// One running copy per user: a lock file held open for the app's lifetime.
fn single_instance() -> Option<std::fs::File> {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("run.lock");
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .share_mode(0)
            .open(path)
            .ok()
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        // SAFETY: flock on a descriptor we own.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        locked.then_some(file)
    }
}

fn run(flags: Flags) -> i32 {
    let Some(_lock) = single_instance() else {
        println!("AgentRouter 设备已经在运行。");
        return 0;
    };
    let _ = flags;
    let cfg = Config::load();
    let device = Arc::new(Device::new(Options {
        data_dir: data_dir(),
        access: cfg.access,
        folders: cfg.folders.clone(),
        confirm: if cfg!(windows) {
            consent::native()
        } else {
            consent::deny_all()
        },
        home: home_dir(),
        shell: Shell::default_for_os(),
    }));
    log(&format!(
        "starting {} (gateway {}, access {}, {} folder(s))",
        env!("CARGO_PKG_VERSION"),
        cfg.gateway,
        cfg.access,
        cfg.folders.len()
    ));
    let rt = Runtime::new(device.clone(), cfg);
    let net_rt = rt.clone();
    let net = std::thread::spawn(move || net::run(net_rt));
    let watch_rt = rt.clone();
    std::thread::spawn(move || net::watch_config(watch_rt));
    while !rt.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
    }
    device.stop_all("quit");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !net.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    log("stopped");
    0
}

fn cmd_link(flags: Flags) -> i32 {
    if keystore::is_linked() {
        eprintln!("这台电脑已经链接了。要换账号，先运行 agentrouter-device unlink。");
        return 1;
    }
    let mut cfg = Config::load();
    if let Some(g) = flags.gateway {
        cfg.gateway = g;
    }
    if let Some(n) = flags.name {
        cfg.name = n;
    }
    if let Some(a) = flags.access {
        cfg.access = a;
    }
    if !flags.folders.is_empty() {
        match canonical_folders(&flags.folders) {
            Ok(f) => cfg.folders = f,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
    }
    if cfg.access.needs_folders() && cfg.folders.is_empty() {
        eprintln!("「{}」要先选文件夹：--folder <路径>", cfg.access.label());
        return 2;
    }
    if !flags.yes && !confirm_locally(&link::link_question(&cfg)) {
        println!("没有链接。");
        return 1;
    }
    cfg.paused = false;
    cfg.disconnected = false;
    cfg.unattended = flags.unattended;
    if cfg.unattended && cfg.access == Access::Readonly {
        cfg.unattended = false;
    }
    if let Err(e) = cfg.save() {
        eprintln!("设置没有保存：{e}");
        return 1;
    }
    let stop = AtomicBool::new(false);
    let result = link::link(&cfg, &stop, |pairing| {
        println!("{}", link::code_text(pairing));
    });
    match result {
        Ok(device) => {
            audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
                "event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders,
            }));
            println!("已链接（{device}）。运行 agentrouter-device run 让它上线。");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn cmd_access(flags: Flags) -> i32 {
    let Some(level) = flags.positional.first().and_then(|l| Access::parse(l)) else {
        eprintln!("要写访问级别：readonly、folders、confirm 或 full");
        return 2;
    };
    let mut cfg = Config::load();
    let folders = if flags.folders.is_empty() {
        cfg.folders.clone()
    } else {
        match canonical_folders(&flags.folders) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
    };
    if level.needs_folders() && folders.is_empty() {
        eprintln!("「{}」要先选文件夹：--folder <路径>", level.label());
        return 2;
    }
    if !flags.yes && !confirm_locally(&link::change_question(level, &folders)) {
        println!("没有改。");
        return 1;
    }
    cfg.access = level;
    cfg.folders = folders;
    match cfg.save() {
        Ok(()) => {
            audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
                "event": "access", "access": cfg.access.as_str(), "folders": cfg.folders,
            }));
            println!(
                "访问级别：{}；文件夹：{}",
                cfg.access.label(),
                cfg.folders.join("；")
            );
            0
        }
        Err(e) => {
            eprintln!("设置没有保存：{e}");
            1
        }
    }
}

fn status() -> i32 {
    let cfg = Config::load();
    match keystore::load() {
        Some(identity) => println!(
            "已链接：{}（控制面密钥 {}）",
            identity.device, identity.control.kid
        ),
        None => println!("未链接"),
    }
    println!("网关：{}", cfg.gateway);
    println!("名字：{}", cfg.name);
    println!("访问级别：{}", cfg.access.label());
    println!(
        "文件夹：{}",
        if cfg.folders.is_empty() {
            "没有".to_string()
        } else {
            cfg.folders.join("；")
        }
    );
    if cfg.paused {
        println!("已暂停");
    }
    if cfg.disconnected {
        println!("已断开（在托盘里点「重新连接」）");
    }
    if cfg.unattended {
        println!("无人值守：命令不逐条确认（链接时由本机管理员预先同意）");
    }
    let log = data_dir().join("audit.log");
    match audit::verify(&log) {
        Ok(n) => println!("审计日志：{}（{n} 条，完好）", log.display()),
        Err((line, why)) => println!("审计日志：{}（第 {line} 行有问题：{why}）", log.display()),
    }
    0
}
