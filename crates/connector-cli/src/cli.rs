//! `agentrouter` (also installed as `agentrouter-device`): the linked-device helper on the command
//! line. Run `agentrouter link` in a project folder: the first time it pairs this computer (a code to
//! confirm on the web), later it adds the folder to the same device; then it stays in the foreground,
//! asking in the terminal before anything that needs a yes. Ctrl+C stops every job it started and
//! disconnects. The desktop app (`app/`) embeds the same core with a tray and the cloud web UI.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentrouter_device::config::{Access, Config};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::net::{self, Conn, Runtime};
use agentrouter_device::util::{data_dir, home_dir, log};
use agentrouter_device::{audit, checkpoint, consent, gate, keystore, link};

use crate::prompt;

/// Set by Ctrl+C (one handler for the whole run: pairing, then serving).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "AgentRouter 小助手（agentrouter）：让你在云端对话里用的 AI 在这台电脑的文件夹里干活

用法：
  agentrouter link [选项]       在项目文件夹里运行。第一次：显示配对码，在网页上确认后链接这台电脑；
                                之后：把当前文件夹加到同一台设备。然后在前台运行（Ctrl+C 断开）
      --access folders|confirm|readonly|full
                                访问级别（默认 folders：在链接的文件夹里读写、运行命令都不用问；
                                明显越界的命令直接拦下；每轮改动前自动打检查点，可以撤销）
      --folder <路径>           链接这个文件夹，而不是当前文件夹（可写多次）
      --name <名字>  --gateway <网址>
      --no-run                  只链接，不在前台运行
      --unattended              无人值守（只用于服务器和 CI）：这台机器的管理员现在预先同意命令，
                                运行时不再逐条确认，所有操作照样记审计
  agentrouter [run] [--background]
                                连上网关，等云端对话的请求。在终端里运行时确认就在终端里问；
                                --background（或没有终端）时 Windows 弹窗问，macOS / Linux 一律拒绝
  agentrouter status            链接、文件夹、访问级别、是否在运行、审计日志
  agentrouter unlink [文件夹]   写了文件夹（. 是当前文件夹）：只取消链接这个文件夹；
                                不写：在本机删除这台设备的身份
  agentrouter access <级别> [--folder <路径>]... [--readonly-commands on|off] [--yes]
  agentrouter undo [检查点] [--list] [--yes]
                                撤销 AI 的改动：不写检查点就撤销当前文件夹最近一轮改动；--list 列出检查点
  agentrouter audit verify      检查审计日志有没有被改过

确认时的选项：y 允许这一次 · a 本对话同类的都允许 · n 拒绝 · d 拒绝并断开
（agentrouter-device 是同一个程序的旧名字，用法一样。）
";

#[derive(Default)]
struct Flags {
    gateway: Option<String>,
    name: Option<String>,
    access: Option<Access>,
    folders: Vec<String>,
    readonly_commands: Option<bool>,
    yes: bool,
    list: bool,
    unattended: bool,
    no_run: bool,
    background: bool,
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
            "--readonly-commands" => {
                let v = value("--readonly-commands")?;
                flags.readonly_commands = Some(match v.as_str() {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    _ => return Err(format!("--readonly-commands 要写 on 或 off，不是 {v}")),
                });
            }
            "--yes" | "-y" => flags.yes = true,
            "--list" => flags.list = true,
            "--unattended" => flags.unattended = true,
            "--no-run" => flags.no_run = true,
            "--background" => flags.background = true,
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

fn same_folder(a: &str, b: &str) -> bool {
    gate::inside(Path::new(a), Path::new(b)) && gate::inside(Path::new(b), Path::new(a))
}

fn folders_text(folders: &[String]) -> String {
    if folders.is_empty() {
        "没有".to_string()
    } else {
        folders.join("；")
    }
}

/// A yes/no in the terminal (the person typed the command; nothing to ask without a terminal).
fn confirm_in_terminal(text: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    println!("{text}\n\n继续？(y/N)");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

pub fn main() {
    // A process started by a tool that ignores Ctrl+C inherits that, and its handler is then never
    // called: turn normal Ctrl+C processing back on before registering ours.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
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
        "unlink" => unlink(flags),
        "undo" => undo(flags),
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
            println!("agentrouter {}", env!("CARGO_PKG_VERSION"));
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
    let foreground = !flags.background && std::io::stdin().is_terminal();
    let Some(lock) = single_instance() else {
        println!("AgentRouter 小助手已经在运行（另一个终端或后台）。");
        return 0;
    };
    serve(foreground, lock)
}

/// Online until Ctrl+C, a 「断开」 answer, or the device being removed on the web.
fn serve(foreground: bool, _lock: std::fs::File) -> i32 {
    let cfg = Config::load();
    if !keystore::is_linked() {
        println!("这台电脑还没有链接。在项目文件夹里运行 agentrouter link。");
        return 1;
    }
    let confirm = if cfg.unattended && cfg.access != Access::Readonly {
        // Unattended: the operator approved commands when linking this machine (`link --unattended`).
        consent::preapproved()
    } else if foreground {
        prompt::terminal()
    } else if cfg!(windows) {
        consent::native()
    } else {
        consent::unavailable()
    };
    let device = Arc::new(Device::new(Options {
        data_dir: data_dir(),
        access: cfg.access,
        folders: cfg.folders.clone(),
        confirm,
        home: home_dir(),
        shell: Shell::default_for_os(),
    }));
    device.set_readonly_commands(cfg.readonly_commands);
    device.set_foreground(foreground);
    log(&format!(
        "starting {} (gateway {}, access {}, {} folder(s), {})",
        env!("CARGO_PKG_VERSION"),
        cfg.gateway,
        cfg.access,
        cfg.folders.len(),
        if foreground {
            "foreground"
        } else {
            "background"
        }
    ));
    if foreground {
        device.set_echo(Some(Arc::new(|line: &str| println!("  · {line}"))));
        println!(
            "\nAgentRouter 小助手在前台运行：{}（{}{}）",
            cfg.name,
            cfg.access.label(),
            if cfg.access == Access::Confirm && cfg.readonly_commands {
                "，只读命令直接执行"
            } else {
                ""
            }
        );
        println!("链接的文件夹：{}", folders_text(&cfg.folders));
        println!(
            "{}按 Ctrl+C 断开（这次起的命令都会结束）。\n",
            match cfg.access {
                Access::Folders => {
                    "AI 在这些文件夹里干活不用问你，每轮改动前自动打检查点（agentrouter undo 撤销）；要越界时会在这里问你一次。"
                }
                Access::Readonly => "AI 只能读这些文件夹。",
                _ => "需要你同意的操作会在这里问你。",
            }
        );
    }
    let rt = Runtime::new(device.clone(), cfg);
    let net_rt = rt.clone();
    let net = std::thread::spawn(move || net::run(net_rt));
    let watch_rt = rt.clone();
    std::thread::spawn(move || net::watch_config(watch_rt));
    let mut shown = Conn::NotLinked;
    let mut code = 0;
    while !rt.stop.load(Ordering::SeqCst) && !INTERRUPTED.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
        if device.take_disconnect_request() {
            println!("\n你选了断开。");
            break;
        }
        if rt.revoked_notice.swap(false, Ordering::SeqCst) {
            println!("\n这台设备在网页上被移除了。要再用，运行 agentrouter link 重新链接。");
            code = 1;
            break;
        }
        let conn = rt.conn();
        if foreground && conn != shown {
            match &conn {
                Conn::Online => println!("已连上 AgentRouter，等云端对话的请求……"),
                Conn::Offline(why) if matches!(shown, Conn::Online) => {
                    println!("连接断开（{why}），正在重连……")
                }
                _ => {}
            }
            shown = conn;
        }
    }
    rt.stop.store(true, Ordering::SeqCst);
    let running = device.jobs.running();
    device.stop_all("quit");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !net.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    log("stopped");
    if foreground {
        println!(
            "已断开。{}",
            if running > 0 {
                format!("结束了 {running} 个还在运行的命令。")
            } else {
                String::new()
            }
        );
    }
    code
}

fn cmd_link(flags: Flags) -> i32 {
    let mut cfg = Config::load();
    if let Some(g) = flags.gateway {
        cfg.gateway = g;
    }
    if let Some(n) = flags.name {
        cfg.name = n;
    }
    let wanted = if flags.folders.is_empty() {
        match std::env::current_dir() {
            Ok(dir) => vec![dir.display().to_string()],
            Err(e) => {
                eprintln!("看不到当前文件夹：{e}");
                return 2;
            }
        }
    } else {
        flags.folders.clone()
    };
    let folders = match canonical_folders(&wanted) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if keystore::is_linked() {
        let mut added = Vec::new();
        for f in &folders {
            if !cfg.folders.iter().any(|x| same_folder(x, f)) {
                cfg.folders.push(f.clone());
                added.push(f.clone());
            }
        }
        if let Some(a) = flags.access {
            cfg.access = a;
        }
        if let Some(on) = flags.readonly_commands {
            cfg.readonly_commands = on;
        }
        if let Err(e) = cfg.save() {
            eprintln!("设置没有保存：{e}");
            return 1;
        }
        audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
            "event": "folders", "added": added, "access": cfg.access.as_str(), "folders": cfg.folders,
        }));
        if added.is_empty() {
            println!("这个文件夹已经链接过了：{}", folders_text(&folders));
        } else {
            println!("已把文件夹加到这台设备：{}", added.join("；"));
        }
    } else {
        cfg.access = flags.access.unwrap_or(Access::Folders);
        cfg.folders = folders;
        if let Some(on) = flags.readonly_commands {
            cfg.readonly_commands = on;
        }
        cfg.paused = false;
        cfg.disconnected = false;
        cfg.unattended = flags.unattended && cfg.access != Access::Readonly;
        println!(
            "把这台电脑链接到 AgentRouter：{}\n访问级别：{}\n{}\n文件夹：{}\n",
            cfg.name,
            cfg.access.label(),
            cfg.access.explain(),
            folders_text(&cfg.folders)
        );
        if let Err(e) = cfg.save() {
            eprintln!("设置没有保存：{e}");
            return 1;
        }
        let result = link::link(&cfg, &INTERRUPTED, |pairing| {
            println!("{}", link::code_text(pairing));
        });
        match result {
            Ok(device) => {
                audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
                    "event": "linked", "device": device, "access": cfg.access.as_str(), "folders": cfg.folders,
                }));
                println!("已链接（{device}）。");
            }
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    }
    if flags.no_run {
        println!("运行 agentrouter 让它上线。");
        return 0;
    }
    let Some(lock) = single_instance() else {
        println!("小助手已经在另一个终端里运行，新的设置马上生效。");
        return 0;
    };
    serve(!flags.background && std::io::stdin().is_terminal(), lock)
}

fn unlink(flags: Flags) -> i32 {
    let Some(folder) = flags.positional.first() else {
        keystore::forget();
        audit::Audit::open(&data_dir().join("audit.log"))
            .record(serde_json::json!({"event": "unlinked"}));
        println!(
            "已在本机删除这台设备的身份。网页「我的设备」里的记录请在那里移除。\n只想取消一个文件夹：agentrouter unlink <文件夹>（. 是当前文件夹）"
        );
        return 0;
    };
    let target = std::fs::canonicalize(folder)
        .map(|p| gate::display(&p))
        .unwrap_or_else(|_| folder.clone());
    let mut cfg = Config::load();
    let before = cfg.folders.len();
    cfg.folders.retain(|f| !same_folder(f, &target));
    if cfg.folders.len() == before {
        println!("这个文件夹没有链接：{target}");
        return 1;
    }
    if let Err(e) = cfg.save() {
        eprintln!("设置没有保存：{e}");
        return 1;
    }
    // The unlinked folder gets its write-boundary label back.
    agentrouter_device::confine::release(&data_dir(), &gate::resolve_folders(&cfg.folders));
    audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
        "event": "folders", "removed": [target], "folders": cfg.folders,
    }));
    println!(
        "已取消链接：{target}\n还链接着：{}",
        folders_text(&cfg.folders)
    );
    0
}

fn cmd_access(flags: Flags) -> i32 {
    let mut cfg = Config::load();
    let level = match flags.positional.first() {
        Some(l) => match Access::parse(l) {
            Some(level) => level,
            None => {
                eprintln!("要写访问级别：readonly、folders、confirm 或 full");
                return 2;
            }
        },
        None if flags.readonly_commands.is_some() => cfg.access,
        None => {
            eprintln!("要写访问级别：readonly、folders、confirm 或 full");
            return 2;
        }
    };
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
    if !flags.yes && !confirm_in_terminal(&link::change_question(level, &folders)) {
        println!("没有改（没有终端时加 --yes）。");
        return 1;
    }
    cfg.access = level;
    cfg.folders = folders;
    if let Some(on) = flags.readonly_commands {
        cfg.readonly_commands = on;
    }
    match cfg.save() {
        Ok(()) => {
            audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
                "event": "access", "access": cfg.access.as_str(), "folders": cfg.folders,
                "readonlyCommands": cfg.readonly_commands,
            }));
            println!(
                "访问级别：{}；文件夹：{}；只读命令直接执行：{}",
                cfg.access.label(),
                folders_text(&cfg.folders),
                if cfg.readonly_commands { "开" } else { "关" }
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
    let running = single_instance().is_none();
    println!("小助手：{}", if running { "在运行" } else { "没有运行" });
    println!("网关：{}", cfg.gateway);
    println!("名字：{}", cfg.name);
    println!("访问级别：{}", cfg.access.label());
    if cfg.access == Access::Confirm {
        println!(
            "只读命令直接执行：{}",
            if cfg.readonly_commands { "开" } else { "关" }
        );
    }
    println!("文件夹：{}", folders_text(&cfg.folders));
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

/// `agentrouter undo`: put a folder back to a checkpoint (DEVICE-PROTOCOL.md §5.7).
fn undo(flags: Flags) -> i32 {
    let data = data_dir();
    let all = checkpoint::list(&data);
    let here = std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::canonicalize(d).ok())
        // canonicalize gives `\?\F:\…` on Windows; checkpoints store the plain form.
        .map(|p| std::path::PathBuf::from(gate::display(&p)));
    let mine = |m: &&checkpoint::Meta| {
        here.as_deref().is_some_and(|h| {
            gate::inside(h, Path::new(&m.folder)) || gate::inside(Path::new(&m.folder), h)
        })
    };
    if flags.list {
        let shown: Vec<&checkpoint::Meta> = all.iter().filter(mine).collect();
        let shown = if shown.is_empty() {
            all.iter().collect()
        } else {
            shown
        };
        if shown.is_empty() {
            println!("还没有检查点。");
        }
        for m in shown.iter().take(20) {
            println!(
                "{}  {}  {}{}  {}",
                m.id,
                m.at,
                if m.kind == "git" {
                    "git"
                } else {
                    "文件备份"
                },
                if m.partial { "（部分）" } else { "" },
                m.folder
            );
        }
        return 0;
    }
    // Without an id: the newest checkpoint of this folder that has something to undo (a later round
    // that only ran commands also leaves a checkpoint, and undoing that would change nothing).
    let plan = match flags.positional.first() {
        Some(id) => checkpoint::plan(&data, id),
        None => {
            let candidates: Vec<&checkpoint::Meta> = all.iter().filter(mine).collect();
            if candidates.is_empty() {
                println!("当前文件夹没有检查点（agentrouter undo --list 看全部）。");
                return 1;
            }
            let mut found = None;
            for m in candidates.iter().take(20) {
                match checkpoint::plan(&data, &m.id) {
                    Ok(p) if !p.changes.is_empty() => {
                        found = Some(Ok(p));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        found = Some(Err(e));
                        break;
                    }
                }
            }
            match found {
                Some(p) => p,
                None => {
                    println!("最近几轮之后文件夹没有变化，不用撤销。");
                    return 0;
                }
            }
        }
    };
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if plan.changes.is_empty() {
        println!("{} 之后没有改动，不用撤销。", plan.meta.id);
        return 0;
    }
    let mut text = format!(
        "撤销到检查点 {}（{}，{}）会：",
        plan.meta.id, plan.meta.at, plan.meta.folder
    );
    for (what, path) in plan.changes.iter().take(30) {
        text.push_str(&format!(
            "
  {what} {path}"
        ));
    }
    if plan.changes.len() > 30 {
        text.push_str(&format!(
            "
  …共 {} 个文件",
            plan.changes.len()
        ));
    }
    if plan.meta.partial {
        text.push_str(
            "
（这个检查点不完整：太大的文件没有备份，它们不会被恢复）",
        );
    }
    if !flags.yes && !confirm_in_terminal(&text) {
        println!("没有撤销。");
        return 1;
    }
    match checkpoint::undo(&data, &plan) {
        Ok(Some(saved)) => {
            println!("已撤销。撤销前的状态存成了检查点 {saved}，想反悔：agentrouter undo {saved}");
            0
        }
        Ok(None) => {
            println!("已撤销。");
            0
        }
        Err(e) => {
            eprintln!("撤销没有完成：{e}");
            1
        }
    }
}
