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
use agentrouter_device::{
    audit, deploy, gate, keystore, link, local_agent, mcp, workspace, worktree,
};

/// Set by Ctrl+C (one handler for the whole run: pairing, then serving).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "AgentRouter（agentrouter）：在任意文件夹里运行 agentrouter link，云端对话里的 Agent 就能在这个文件夹里干活

用法：
  agentrouter link [--read-only] [--yes-servers]
                                链接当前文件夹（第一次会显示配对码，在网页上确认）。命令开着就一直连着，Ctrl+C 断开。
                                云端 Agent 只能在这个文件夹里干活：每个本机工具都在系统沙箱里，文件夹外的写、删直接被拒绝。
      --read-only               这个文件夹只读
      --yes-servers             不在目录里的本机工具不用逐个问（无人值守的电脑）
      --gateway <网址>  --name <这台电脑的名字>
  agentrouter undo [--list] [检查点]
                                撤销 Agent 在这个文件夹里最近一轮的改动（再运行一次就再往前一轮）；--list 列出检查点
  agentrouter audit [verify] [条数]
                                这个文件夹的审计记录（每次调用连参数一起记，密钥不记）；verify 检查有没有被改过
  agentrouter sandbox setup|status|remove
                                本机工具的隔离环境（Windows 会弹一次管理员确认）
  agentrouter status            账号、网关、沙箱、当前文件夹
  agentrouter logout            这台电脑退出链接（删掉本机的链接身份）
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
    read_only: bool,
    yes_servers: bool,
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
            "--read-only" => flags.read_only = true,
            "--yes-servers" => flags.yes_servers = true,
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
        "link" => crate::console::link(
            crate::console::LinkOptions {
                read_only: flags.read_only,
                yes_servers: flags.yes_servers,
                gateway: flags.gateway.clone(),
                name: flags.name.clone(),
            },
            &INTERRUPTED,
        ),
        "access" => cmd_access(flags),
        "status" => crate::console::status(),
        "logout" => crate::console::logout(),
        "unlink" => unlink(flags),
        "undo" => crate::console::undo(flags.list, flags.positional.first().map(String::as_str)),
        "screen" => cmd_screen(flags),
        "mcp" => cmd_mcp(flags),
        "workspace" => cmd_workspace(flags),
        "agent" => cmd_agent(flags),
        "audit" => {
            let verify = flags.positional.first().map(String::as_str) == Some("verify");
            let n = flags
                .positional
                .iter()
                .find_map(|s| s.parse::<usize>().ok())
                .unwrap_or(20)
                .clamp(1, 1000);
            crate::console::audit_cmd(verify, n)
        }
        "sandbox" => cmd_sandbox(flags),
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

/// `agentrouter sandbox setup|status|remove` (docs/LINKER-MCP.md §6, #42).
fn cmd_sandbox(flags: Flags) -> i32 {
    let rt = agentrouter_sandbox::Runtime::new(&data_dir());
    let report = |done: Result<Vec<String>, String>| match done {
        Ok(lines) if lines.is_empty() => {
            println!("没有要做的。");
            0
        }
        Ok(lines) => {
            for l in lines {
                println!("{l}");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    };
    match flags.positional.first().map(String::as_str) {
        Some("setup") => {
            let code = report(rt.setup());
            if code == 0 {
                print_sandbox_status(&rt.status());
            }
            code
        }
        Some("remove") => report(rt.remove()),
        None | Some("status") => {
            let st = rt.status();
            print_sandbox_status(&st);
            if st.ready { 0 } else { 1 }
        }
        Some(other) => {
            eprintln!("不认识的 sandbox 子命令：{other}（setup、status 或 remove）");
            2
        }
    }
}

fn print_sandbox_status(st: &agentrouter_sandbox::Status) {
    println!(
        "隔离环境 {}（{}）：{}",
        st.backend,
        st.mechanism,
        if st.ready { "可用" } else { "不可用" }
    );
    for p in &st.problems {
        println!("  {}", p.message);
        println!("  {}", p.next);
    }
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

/// `agentrouter workspace ...`: the workspace root and the project registry (workspace spec §1–3; M0).
/// `agentrouter workspace deploy ...`: the owner's side of S2a — register a target once with a local
/// confirmation; the AI can deploy/rollback through the workspace MCP service, never register.
fn cmd_deploy(flags: &Flags) -> i32 {
    let data = data_dir();
    let usage = "用法：agentrouter workspace deploy target add <名字> <目录> | target list | target remove <名字> | list | rollback <id>";
    let arg = |i: usize| flags.positional.get(i).map(String::as_str).unwrap_or("");
    match (arg(1), arg(2)) {
        ("target", "add") => {
            let name = arg(3);
            let path = arg(4);
            if name.is_empty() || path.is_empty() {
                println!("{usage}");
                return 1;
            }
            if !flags.yes
                && !confirm_in_terminal(&format!(
                    "把目录登记成部署目标？\n\n名字：{name}\n目录：{path}\n\n云端 AI 只能把文件部署到这里（先备份、本机批准、失败回滚）；目录本身不会被当工作区。"
                ))
            {
                println!("没有登记（没有终端时加 --yes）。");
                return 1;
            }
            match deploy::add_target(&data, name, path) {
                Ok(target) => {
                    println!("部署目标：{} → {}", target.name, target.path);
                    0
                }
                Err(e) => {
                    print_device_error(&e);
                    1
                }
            }
        }
        ("target", "list") => match deploy::list_targets(&data) {
            Ok(targets) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"targets": targets}))
                        .unwrap_or_default()
                );
                0
            }
            Err(e) => {
                print_device_error(&e);
                1
            }
        },
        ("target", "remove") => {
            let name = arg(3);
            if name.is_empty() {
                println!("{usage}");
                return 1;
            }
            match deploy::remove_target(&data, name) {
                Ok(removed) => {
                    println!(
                        "{}",
                        if removed {
                            "已删除"
                        } else {
                            "没有这个目标"
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
        ("list", _) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "deployments": deploy::list(&data, 50),
                }))
                .unwrap_or_default()
            );
            0
        }
        ("rollback", _) => {
            let id = arg(2);
            if id.is_empty() {
                println!("{usage}");
                return 1;
            }
            if !flags.yes {
                match deploy::rollback_text(&data, id) {
                    Ok(text) => {
                        if !confirm_in_terminal(&text) {
                            println!("没有回滚（没有终端时加 --yes）。");
                            return 1;
                        }
                    }
                    Err(e) => {
                        print_device_error(&e);
                        return 1;
                    }
                }
            }
            match deploy::rollback_apply(&data, id) {
                Ok(summary) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&summary).unwrap_or_default()
                    );
                    0
                }
                Err(e) => {
                    print_device_error(&e);
                    1
                }
            }
        }
        _ => {
            println!("{usage}");
            1
        }
    }
}

/// `agentrouter agent list`: locally registered Agents (`lag_`) and their sessions (`lses_`), recorded
/// the first time a local Agent used `agentrouter mcp-serve local`.
fn cmd_agent(flags: Flags) -> i32 {
    let data = data_dir();
    match flags.positional.get(1).map(String::as_str) {
        Some("list") | None => match local_agent::list(&data) {
            Ok(value) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
                0
            }
            Err(e) => {
                print_device_error(&e);
                1
            }
        },
        _ => {
            println!("用法：agentrouter agent list");
            1
        }
    }
}

fn cmd_workspace(flags: Flags) -> i32 {
    if flags.positional.first().map(String::as_str) == Some("deploy") {
        return cmd_deploy(&flags);
    }
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
