//! `agentrouter` (also installed as `agentrouter-device`): the linked-device helper on the command
//! line. Run `agentrouter link` in a project folder: the first time it pairs this computer (a code to
//! confirm on the web), later it adds the folder to the same device; then it stays in the foreground and
//! relays the cloud conversations' calls to the local MCP servers it runs here (LINKED-DEVICES.md §23.8).
//! Ctrl+C stops every server and disconnects. The desktop app (`app/`) embeds the same core with a tray.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentrouter_device::config::{Access, Config};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::net::{self, Conn, Runtime};
use agentrouter_device::util::{data_dir, home_dir, log};
use agentrouter_device::{audit, checkpoint, gate, keystore, link, mcp, workspace, worktree};

/// Set by Ctrl+C (one handler for the whole run: pairing, then serving).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "AgentRouter 小助手（agentrouter）：把这台电脑链接到 AgentRouter，云端对话里的 AI 通过电脑上的本机工具（MCP 服务）干活

用法：
  agentrouter link [选项]       在项目文件夹里运行。第一次：显示配对码，在网页上确认后链接这台电脑；
                                之后：把当前文件夹加到同一台设备。然后在前台运行（Ctrl+C 断开）
      --access folders|full|readonly|confirm
                                访问级别（默认 folders：本机工具在这台电脑上为 AI 干活，不用问你；
                                会改文件的调用之前自动打检查点，可以撤销；readonly 和 confirm 不运行本机工具）
      --folder <路径>           链接这个文件夹，而不是当前文件夹（可写多次）
      --name <名字>  --gateway <网址>
      --no-run                  只链接，不在前台运行
      --screen                  同时打开屏幕功能（见 agentrouter screen）
      --mcp                     同时打开本机工具（默认开；见 agentrouter mcp）
  agentrouter [run] [--background]
                                连上网关，等云端对话的请求
  agentrouter status            链接、文件夹、访问级别、是否在运行、审计日志
  agentrouter unlink [文件夹]   写了文件夹（. 是当前文件夹）：只取消链接这个文件夹；
                                不写：在本机删除这台设备的身份
  agentrouter access <级别> [--folder <路径>]... [--yes]
  agentrouter screen on|off     屏幕功能（目前只有 Windows）：打开后默认装上 Windows 操作的本机工具
                                （Windows-MCP），AI 可以截图、操作鼠标键盘、打开和关闭程序；
                                屏幕上方有提示条，锁屏和 UAC 上不操作
  agentrouter mcp on|off       本机工具（MCP，默认开）：云端对话在这台电脑上运行的 MCP 服务
                                （文件、Git、终端、代码理解，屏幕打开时加 Windows 操作，也可以装别的），
                                它们的工具给云端的 AI 用，不再逐条问你；只在「只限这些文件夹」和「完全访问」级别；
                                断开、Ctrl+C 或 off 时全部停止
  agentrouter workspace init [--root <路径>] [--yes]
                                建工作区（默认 %USERPROFILE%\\Workspaces）并打开：AI 的项目都放在这里；
                                游戏目录只能作只读引用/部署目标，不能当项目
  agentrouter workspace project add <名字> | list | baseline <名字>
                                在工作区里建项目（目录在根下）/ 列出 / 建 git 基线
  agentrouter workspace worktree create <项目> <会话 id> | list [项目]
  agentrouter workspace worktree remove <项目> <会话 id> [--delete-branch]
  agentrouter workspace worktree prune <项目> [--older-hours N] [--delete-branch]
                                每个会话一个 git worktree；移除默认保留分支（改动记录）
  agentrouter undo [检查点] [--list] [--yes]
                                撤销 AI 的改动：不写检查点就撤销当前文件夹最近一轮改动；--list 列出检查点
  agentrouter audit verify      检查审计日志有没有被改过

（agentrouter-device 是同一个程序的旧名字，用法一样。）
";

#[derive(Default)]
struct Flags {
    gateway: Option<String>,
    name: Option<String>,
    access: Option<Access>,
    folders: Vec<String>,
    yes: bool,
    list: bool,
    unattended: bool,
    no_run: bool,
    background: bool,
    screen: Option<bool>,
    mcp: Option<bool>,
    root: Option<String>,
    delete_branch: bool,
    older_hours: Option<u64>,
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
            "--list" => flags.list = true,
            "--unattended" => flags.unattended = true,
            "--no-run" => flags.no_run = true,
            "--background" => flags.background = true,
            "--screen" => flags.screen = Some(true),
            "--mcp" => flags.mcp = Some(true),
            "--root" => flags.root = Some(value("--root")?),
            "--delete-branch" => flags.delete_branch = true,
            "--older-hours" => {
                let v = value("--older-hours")?;
                flags.older_hours = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--older-hours 要写小时数：{v}"))?,
                );
            }
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
        "screen" => cmd_screen(flags),
        "mcp" => cmd_mcp(flags),
        "workspace" => cmd_workspace(flags),
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
    // The workspace MCP service is installed by the cloud as `command: "agentrouter"`; the shim makes
    // that name resolve to this exact binary (see `mcp::install_self_shim`).
    if let Err(e) = mcp::install_self_shim(&data_dir()) {
        log(&format!("could not write the agentrouter shim: {e}"));
    }
    let device = Arc::new(Device::new(Options {
        data_dir: data_dir(),
        access: cfg.access,
        folders: cfg.folders.clone(),
        home: home_dir(),
        workspaces: cfg.workspaces,
    }));
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
            "\nAgentRouter 小助手在前台运行：{}（{}）",
            cfg.name,
            cfg.access.label()
        );
        println!("链接的文件夹：{}", folders_text(&cfg.folders));
        if cfg.screen {
            println!("{}（AI 操作时屏幕上方有提示条）", screen_text(&cfg));
        }
        if cfg.mcp {
            println!("{}", mcp_text(&cfg));
        }
        println!(
            "{}按 Ctrl+C 断开（本机工具都会停下）。\n",
            match cfg.access {
                Access::Folders | Access::Full => {
                    "AI 用这台电脑上的本机工具干活，不用问你；会改文件的调用之前自动打检查点（agentrouter undo 撤销）。"
                }
                _ => "这个访问级别下本机工具不运行（agentrouter access folders 打开）。",
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
    device.stop_all("quit");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !net.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    log("stopped");
    if foreground {
        println!("已断开。");
    }
    code
}

fn cmd_link(flags: Flags) -> i32 {
    let mut cfg = Config::load();
    if let Some(g) = flags.gateway.clone() {
        cfg.gateway = g;
    }
    if let Some(n) = flags.name.clone() {
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
        if flags.unattended {
            cfg.unattended = cfg.access != Access::Readonly;
        }
        apply_screen(&mut cfg, &flags);
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
        cfg.paused = false;
        cfg.disconnected = false;
        cfg.unattended = flags.unattended && cfg.access != Access::Readonly;
        apply_screen(&mut cfg, &flags);
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

fn apply_screen(cfg: &mut Config, flags: &Flags) {
    if let Some(on) = flags.screen {
        cfg.screen = on;
    }
    if let Some(on) = flags.mcp {
        cfg.mcp = on;
    }
}

fn screen_text(cfg: &Config) -> String {
    format!(
        "屏幕功能：{}",
        if cfg.screen {
            "开（Windows 操作的本机工具：截图、鼠标键盘、打开和关闭程序）"
        } else {
            "关"
        }
    )
}

fn mcp_text(cfg: &Config) -> String {
    format!(
        "本机工具（MCP）：{}",
        if cfg.mcp {
            "开（云端对话装的 MCP 服务在这台电脑上运行）"
        } else {
            "关"
        }
    )
}

/// The workspace line for `status`: switch, root, projects.
fn workspace_text(cfg: &Config) -> String {
    let switch = if cfg.workspaces {
        "开"
    } else {
        "关（agentrouter workspace init）"
    };
    match workspace::listing(&data_dir(), &home_dir()) {
        Ok(registry) => format!(
            "{}（根 {}，{} 个项目）",
            switch,
            registry.root,
            registry.projects.len()
        ),
        Err(e) => format!("{}（登记文件有问题：{}）", switch, e.code),
    }
}

fn print_device_error(e: &agentrouter_device::protocol::DeviceError) {
    eprintln!("{}：{}", e.code, e.message);
    if let Some(next) = &e.next {
        eprintln!("{next}");
    }
}

/// `agentrouter mcp on|off`: let conversations run local MCP servers here (LINKED-DEVICES.md §23).
fn cmd_mcp(mut flags: Flags) -> i32 {
    let mut cfg = Config::load();
    match flags.positional.first().map(String::as_str) {
        Some("on") => flags.mcp = Some(true),
        Some("off") => flags.mcp = Some(false),
        None | Some("status") => {}
        Some(other) => {
            eprintln!("agentrouter mcp 后面写 on 或 off，不是 {other}");
            return 2;
        }
    }
    if let Some(on) = flags.mcp {
        cfg.mcp = on;
    }
    if let Err(e) = cfg.save() {
        eprintln!("设置没有保存：{e}");
        return 1;
    }
    audit::Audit::open(&data_dir().join("audit.log"))
        .record(serde_json::json!({"event": "mcp", "on": cfg.mcp}));
    println!("{}", mcp_text(&cfg));
    if cfg.mcp && matches!(cfg.access, Access::Readonly | Access::Confirm) {
        println!(
            "访问级别是{}：本机工具不能运行；运行 agentrouter access folders 后才可以。",
            cfg.access.label()
        );
    }
    0
}

/// `agentrouter screen on|off`: let conversations look at the screen and operate it (LINKED-DEVICES.md §20).
fn cmd_screen(mut flags: Flags) -> i32 {
    let mut cfg = Config::load();
    match flags.positional.first().map(String::as_str) {
        Some("on") => flags.screen = Some(true),
        Some("off") => flags.screen = Some(false),
        None | Some("status") => {}
        Some(other) => {
            eprintln!("agentrouter screen 后面写 on 或 off，不是 {other}");
            return 2;
        }
    }
    if !agentrouter_device::screen::supported() && flags.screen == Some(true) {
        eprintln!("这台电脑的系统还不支持屏幕功能（目前只有 Windows）。");
        return 1;
    }
    apply_screen(&mut cfg, &flags);
    if let Err(e) = cfg.save() {
        eprintln!("设置没有保存：{e}");
        return 1;
    }
    audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
        "event": "screen", "on": cfg.screen,
    }));
    println!("{}", screen_text(&cfg));
    if cfg.screen && matches!(cfg.access, Access::Readonly | Access::Confirm) {
        println!(
            "访问级别是{}：本机工具不能运行；运行 agentrouter access folders 后才可以。",
            cfg.access.label()
        );
    }
    0
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
    let _ = agentrouter_device::confine::release(&data_dir(), &gate::resolve_folders(&cfg.folders));
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
    match cfg.save() {
        Ok(()) => {
            audit::Audit::open(&data_dir().join("audit.log")).record(serde_json::json!({
                "event": "access", "access": cfg.access.as_str(), "folders": cfg.folders,
            }));
            println!(
                "访问级别：{}；文件夹：{}",
                cfg.access.label(),
                folders_text(&cfg.folders)
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
    println!("文件夹：{}", folders_text(&cfg.folders));
    if cfg.paused {
        println!("已暂停");
    }
    if cfg.disconnected {
        println!("已断开（在托盘里点「重新连接」）");
    }
    println!("{}", screen_text(&cfg));
    println!("{}", mcp_text(&cfg));
    println!("工作区：{}", workspace_text(&cfg));
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

/// `agentrouter workspace ...`: the workspace root and the project registry (workspace spec §1–3; M0).
fn cmd_workspace(flags: Flags) -> i32 {
    let mut cfg = Config::load();
    let data = data_dir();
    let home = home_dir();
    match flags.positional.first().map(String::as_str) {
        Some("init") => {
            let before = match workspace::load(&data) {
                Ok(registry) => registry,
                Err(e) => {
                    print_device_error(&e);
                    return 1;
                }
            };
            if let Some(raw) = flags.root.as_deref()
                && !before.projects.is_empty()
                && !before.root.is_empty()
            {
                let wanted = std::fs::canonicalize(raw)
                    .map(|p| gate::display(&p))
                    .unwrap_or_else(|_| raw.trim().to_string());
                if !same_folder(&before.root, &wanted) {
                    eprintln!(
                        "工作区根已经是 {}；还有 {} 个项目，换根要先迁移项目",
                        before.root,
                        before.projects.len()
                    );
                    return 1;
                }
            }
            let root_text = flags
                .root
                .clone()
                .unwrap_or_else(|| gate::display(&workspace::default_root(&home)));
            if !flags.yes
                && !confirm_in_terminal(&format!(
                    "在这台电脑上建工作区？\n\n位置：{root_text}\n\nAI 的项目都放在这里；游戏目录只能作只读引用/部署目标，不能当项目。"
                ))
            {
                println!("没有建（没有终端时加 --yes）。");
                return 1;
            }
            let linked = gate::resolve_folders(&cfg.folders);
            match workspace::ensure_root(&data, &home, flags.root.as_deref(), &linked) {
                Ok(root) => {
                    cfg.workspaces = true;
                    if let Err(e) = cfg.save() {
                        eprintln!("设置没有保存：{e}");
                        return 1;
                    }
                    audit::Audit::open(&data.join("audit.log")).record(serde_json::json!({
                        "event": "workspace", "action": "init", "root": gate::display(&root),
                    }));
                    println!("工作区：{}（已打开）", gate::display(&root));
                    println!("项目：agentrouter workspace project add <名字>");
                    0
                }
                Err(e) => {
                    print_device_error(&e);
                    1
                }
            }
        }
        Some("project") => match flags.positional.get(1).map(String::as_str) {
            Some("add") | Some("create") => {
                let Some(name) = flags.positional.get(2) else {
                    eprintln!("要写项目名：agentrouter workspace project add <名字>");
                    return 2;
                };
                if !cfg.workspaces {
                    eprintln!("工作区没有打开。先运行 agentrouter workspace init。");
                    return 1;
                }
                let linked = gate::resolve_folders(&cfg.folders);
                match workspace::ensure_project(&data, &home, &linked, name, flags.root.as_deref())
                {
                    Ok(ensured) => {
                        audit::Audit::open(&data.join("audit.log")).record(serde_json::json!({
                            "event": "workspace", "action": "project", "project": ensured.project,
                            "slug": ensured.slug, "path": ensured.project_dir, "created": ensured.created,
                        }));
                        println!("项目：{}  {}", ensured.project, ensured.project_dir);
                        if !ensured.created {
                            println!("（之前就有，路径没变）");
                        }
                        println!("worktrees：{}", ensured.worktrees_dir);
                        0
                    }
                    Err(e) => {
                        print_device_error(&e);
                        1
                    }
                }
            }
            Some("list") | None => workspace_listing(&cfg),
            Some("baseline") => {
                let Some(name) = flags.positional.get(2) else {
                    eprintln!("要写项目名：agentrouter workspace project baseline <名字>");
                    return 2;
                };
                if !cfg.workspaces {
                    eprintln!("工作区没有打开。先运行 agentrouter workspace init。");
                    return 1;
                }
                match worktree::baseline(&data, name) {
                    Ok(value) => {
                        println!(
                            "基线：{}（{}）{}",
                            value["project"].as_str().unwrap_or(name),
                            value["commit"].as_str().unwrap_or(""),
                            if value["created"] == true {
                                " · 新建仓库"
                            } else {
                                " · 已有仓库，只读使用"
                            }
                        );
                        0
                    }
                    Err(e) => {
                        print_device_error(&e);
                        1
                    }
                }
            }
            Some(other) => {
                eprintln!("不认识的 workspace project 子命令：{other}（add|list|baseline）");
                2
            }
        },
        Some("worktree") => {
            if !cfg.workspaces {
                eprintln!("工作区没有打开。先运行 agentrouter workspace init。");
                return 1;
            }
            match flags.positional.get(1).map(String::as_str) {
                Some("create") => {
                    let (Some(project), Some(session)) =
                        (flags.positional.get(2), flags.positional.get(3))
                    else {
                        eprintln!("用法：agentrouter workspace worktree create <项目> <会话 id>");
                        return 2;
                    };
                    match worktree::create(&data, project, session) {
                        Ok(value) => {
                            println!(
                                "worktree：{}（分支 {}，base {}）{}",
                                value["worktreeDir"].as_str().unwrap_or(""),
                                value["branch"].as_str().unwrap_or(""),
                                value["baseCommit"].as_str().unwrap_or(""),
                                if value["reused"] == true {
                                    " · 复用"
                                } else {
                                    ""
                                }
                            );
                            audit::Audit::open(&data.join("audit.log")).record(serde_json::json!({
                                "event": "workspace", "action": "worktree",
                                "worktreeDir": value["worktreeDir"], "session": value["session"],
                            }));
                            0
                        }
                        Err(e) => {
                            print_device_error(&e);
                            1
                        }
                    }
                }
                Some("list") | None => {
                    let project = flags.positional.get(2).map(String::as_str);
                    match worktree::list(&data, project) {
                        Ok(value) => {
                            let list = value["worktrees"].as_array().cloned().unwrap_or_default();
                            if list.is_empty() {
                                println!("worktree：还没有");
                            }
                            for entry in list {
                                println!(
                                    "  {}  {}  {}  {}{}",
                                    entry["project"].as_str().unwrap_or(""),
                                    entry["session"].as_str().unwrap_or(""),
                                    entry["path"].as_str().unwrap_or(""),
                                    entry["branch"].as_str().unwrap_or(""),
                                    if entry["exists"] == true {
                                        ""
                                    } else {
                                        "（目录不在）"
                                    }
                                );
                            }
                            0
                        }
                        Err(e) => {
                            print_device_error(&e);
                            1
                        }
                    }
                }
                Some("remove") => {
                    let (Some(project), Some(session)) =
                        (flags.positional.get(2), flags.positional.get(3))
                    else {
                        eprintln!("用法：agentrouter workspace worktree remove <项目> <会话 id>");
                        return 2;
                    };
                    match worktree::remove(&data, project, session, !flags.delete_branch) {
                        Ok(value) => {
                            println!(
                                "worktree {}：{}（分支{}保留）",
                                value["session"].as_str().unwrap_or(session),
                                if value["removed"] == true {
                                    "已移除"
                                } else {
                                    "本来就不存在"
                                },
                                if value["branchKept"] == true {
                                    ""
                                } else {
                                    "不"
                                }
                            );
                            0
                        }
                        Err(e) => {
                            print_device_error(&e);
                            1
                        }
                    }
                }
                Some("prune") => {
                    let Some(project) = flags.positional.get(2) else {
                        eprintln!(
                            "用法：agentrouter workspace worktree prune <项目> [--older-hours N]"
                        );
                        return 2;
                    };
                    match worktree::prune(&data, project, flags.older_hours, flags.delete_branch) {
                        Ok(value) => {
                            println!(
                                "清理：移除 {} 个，保留 {} 个，删除分支 {} 个",
                                value["removed"].as_array().map(Vec::len).unwrap_or(0),
                                value["kept"].as_u64().unwrap_or(0),
                                value["deletedBranches"]
                                    .as_array()
                                    .map(Vec::len)
                                    .unwrap_or(0)
                            );
                            0
                        }
                        Err(e) => {
                            print_device_error(&e);
                            1
                        }
                    }
                }
                Some(other) => {
                    eprintln!(
                        "不认识的 workspace worktree 子命令：{other}（create|list|remove|prune）"
                    );
                    2
                }
            }
        }
        None | Some("status") => workspace_listing(&cfg),
        Some(other) => {
            eprintln!("不认识的 workspace 子命令：{other}（init|project|worktree）");
            2
        }
    }
}

fn workspace_listing(cfg: &Config) -> i32 {
    match workspace::listing(&data_dir(), &home_dir()) {
        Ok(registry) => {
            println!("{}", workspace_text(cfg));
            if registry.projects.is_empty() {
                println!("项目：还没有（agentrouter workspace project add <名字>）");
            } else {
                println!("项目：");
                for project in &registry.projects {
                    println!(
                        "  {}  {}  {}",
                        project.name, project.path, project.created_at
                    );
                }
            }
            if let Ok(value) = worktree::list(&data_dir(), None) {
                let count = value["worktrees"].as_array().map(Vec::len).unwrap_or(0);
                println!("worktree：{count} 个（agentrouter workspace worktree list）");
            }
            0
        }
        Err(e) => {
            print_device_error(&e);
            1
        }
    }
}
