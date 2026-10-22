//! `agentrouter` (also installed as `agentrouter-device`): console mode. Run `agentrouter link` in any folder and
//! the cloud Agent works in that folder for as long as the command runs (docs/HOSTD-MCP.md §8; `console.rs`).
//! `agentrouter mcp-serve local` (main.rs) is the local-Agent entry, kept pending its own design (HOSTD-MCP §4).

use std::sync::atomic::{AtomicBool, Ordering};

use agentrouter_device::util::data_dir;

/// Set by Ctrl+C (one handler for the whole run: pairing, then the link).
pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "AgentRouter（agentrouter）：在任意文件夹里运行 agentrouter link，云端对话里的 Agent 就能在这个文件夹里干活
link 启动 hostd（宿主守护进程）；agentrouter hostd 是同一条命令

用法：
  agentrouter link [文件夹…] [--read-only | --full] [--yes-servers]
                                链接这些文件夹（不写就是当前文件夹；第一次会显示配对码，在网页上确认）。命令开着就一直连着，Ctrl+C 断开。
                                云端 Agent 只能在这些文件夹里干活：每个本机工具都在系统沙箱里，文件夹外的写、删直接被拒绝。
                                连着的时候输入 + 路径 可以再加一个文件夹。
      --read-only               这些文件夹只读
      --full                    完全访问：本机工具用你的账户权限，整台电脑都能读写（只能在这台电脑上打开）
      --yes-servers             不在目录里的本机工具不用逐个问（无人值守的电脑）
      --gateway <网址>  --name <这台电脑的名字>
  agentrouter start [--full]    双击 agentrouter.exe 做的事：选一个工作文件夹（只选一次，以后记住），第一次在浏览器里确认，
                                然后保持连接，云端 Agent 只能在这个文件夹里干活；--full 是完全访问；关掉窗口就断开
  agentrouter link --placement cloud <文件夹…> [--token-file <文件>] [--gateway <网址>]
                                云端沙箱里的 hostd（由沙箱启动，不配对、不问人）：平台发的主机令牌
                                （AGENTROUTER_HOST_TOKEN_FILE 或 AGENTROUTER_HOST_TOKEN）、平台签名公钥
                                （AGENTROUTER_CONTROL_KEY）；沙箱本身就是边界，没有系统沙箱和检查点
  agentrouter undo [--list] [--journal] [检查点]
                                撤销 Agent 在这个文件夹里最近一轮的改动（再运行一次就再往前一轮）；--list 列出检查点；
                                --journal 放回完全访问时 Agent 改过的文件的旧版
  agentrouter audit [verify] [条数]
                                这个文件夹的审计记录（每次调用连参数一起记，密钥不记）；verify 检查有没有被改过
  agentrouter mcp add <名字> [--display [--yes]] [--title <标题>] [--network <域名,…>] -- <命令> [参数…]
                                自己加一个本机工具（MCP 服务器）：之后每次 agentrouter link 都启动它，云端 Agent 就能用它的工具。
                                它在文件夹的沙箱里运行；--display 让它能看到并操作屏幕（比如 Windows-MCP）：
                                这要你在这里同意（--yes 是事先同意，给无人值守的电脑），它会以你自己的权限运行，不受文件夹限制
  agentrouter mcp list          自己加的本机工具
  agentrouter mcp remove <名字> 去掉一个自己加的本机工具（连同给它的权限）
  agentrouter software [list]   这台电脑上的软件和游戏（平台按它们找能干活的电脑；只读系统和启动器自己的清单，不看你的文件）
  agentrouter software add <名字> [--path <文件夹或程序>] [--version <版本>] [--exclusive]
                                补一个没找到的软件或游戏；--exclusive 表示同一时间只能一个 Agent 用（比如游戏）
  agentrouter software hide <id> | unhide <id>
                                不让平台看到某一项（id 见 software list）/ 取消隐藏
  agentrouter software source <来源> on|off
                                打开或关掉一个来源（steam、epic、windows-uninstall、msix、gog、macos-apps、dpkg、rpm、pacman、flatpak、snap）
  agentrouter data add <名字> <文件夹> [--read-only]
                                给一个文件夹起名字（素材、游戏存档）：Agent 和空间按名字用它，不写路径；--read-only 只让读
  agentrouter data list | rename <旧名字> <新名字> | remove <名字>
                                看、改名、拿掉命名文件夹（拿掉后 Agent 立刻用不到它）
  agentrouter sandbox setup|status|remove
                                本机工具的隔离环境（Windows 会弹一次管理员确认）
  sudo agentrouter sandbox userns install|remove
                                Ubuntu 这类限制用户命名空间的 Linux：给 AgentRouter 自己的隔离程序 bubblewrap
                                加一条只管它的 AppArmor 规则（装一份 root 所有的副本）；remove 撤销
  agentrouter service install [文件夹…] [--read-only | --full] [--yes-servers]
                                开机自动连接（只有你运行这条命令才会打开）：登录这台电脑后自动链接这些文件夹，断了自己重连。
                                由系统自己的机制启动（Linux systemd 用户服务、macOS 登录项 LaunchAgent、Windows 任务计划程序）；
                                要先用 agentrouter link 确认过这台电脑。启动后没有人回答 y/N，要问的一律当作「不」
  agentrouter service status    开机自动连接：开着没有、在不在运行、哪些文件夹、链接状态
  agentrouter service uninstall 关掉开机自动连接（删掉系统里的启动项；这台电脑仍然链接着账号）
  agentrouter status            账号、网关、沙箱、当前文件夹
  agentrouter logout            这台电脑退出链接（删掉本机的链接身份）
";

#[derive(Default)]
struct Flags {
    gateway: Option<String>,
    name: Option<String>,
    list: bool,
    journal: bool,
    read_only: bool,
    full: bool,
    yes_servers: bool,
    placement: Option<String>,
    token_file: Option<String>,
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
            "--list" => flags.list = true,
            "--read-only" => flags.read_only = true,
            "--full" => flags.full = true,
            "--journal" => flags.journal = true,
            "--yes-servers" => flags.yes_servers = true,
            "--placement" => flags.placement = Some(value("--placement")?),
            "--token-file" => flags.token_file = Some(value("--token-file")?),
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with("--") => return Err(format!("不认识的选项：{other}")),
            other => flags.positional.push(other.to_string()),
        }
    }
    Ok(flags)
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
    let Some(command) = args.first().cloned() else {
        // Opened from Explorer (a console of its own): the double-click start. From a terminal: the usage.
        if crate::start::own_console() {
            std::process::exit(crate::start::run(false, &INTERRUPTED));
        }
        println!("{USAGE}");
        std::process::exit(0);
    };
    // `mcp add … -- <command…>`: everything after `--` is the tool's own command line, not our flags.
    if command == "mcp" {
        std::process::exit(cmd_mcp(&args[1..]));
    }
    if command == "software" {
        std::process::exit(cmd_software(&args[1..]));
    }
    if command == "data" {
        std::process::exit(cmd_data(&args[1..]));
    }
    let flags = match parse(&args[1..]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    // `hostd` is the program's name (docs/HOSTD-MCP.md §0.3, #80); `link` stays the command people know.
    let command = if command == "hostd" {
        "link".to_string()
    } else {
        command
    };
    let code = match command.as_str() {
        "link" if flags.placement.as_deref() == Some("cloud") => crate::cloud::link(
            crate::cloud::CloudOptions {
                dirs: flags.positional.clone(),
                gateway: flags.gateway.clone(),
                token_file: flags.token_file.clone(),
            },
            &INTERRUPTED,
        ),
        "link" if flags.placement.as_deref().is_some_and(|p| p != "pc") => {
            eprintln!("--placement 只能是 pc 或 cloud");
            2
        }
        "link" => crate::console::link(
            crate::console::LinkOptions {
                dirs: flags.positional.clone(),
                read_only: flags.read_only,
                full: flags.full,
                yes_servers: flags.yes_servers,
                gateway: flags.gateway.clone(),
                name: flags.name.clone(),
                double_click: false,
                service: false,
            },
            &INTERRUPTED,
        ),
        "start" if flags.read_only => {
            eprintln!(
                "agentrouter start 不支持 --read-only；只读请用 agentrouter link --read-only。"
            );
            2
        }
        "start" => crate::start::run(flags.full, &INTERRUPTED),
        "undo" => crate::console::undo(
            flags.list,
            flags.journal,
            flags.positional.first().map(String::as_str),
        ),
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
        "service" => crate::service::cmd(
            flags.positional.first().map(String::as_str),
            flags.positional.get(1..).unwrap_or_default(),
            flags.read_only,
            flags.full,
            flags.yes_servers,
        ),
        "status" => crate::console::status(),
        "logout" => crate::console::logout(),
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

/// `agentrouter mcp add|list|remove` (device #97, Cloud #2396 gap 1): the person's own local tools. Adding one is the
/// person's yes for it; a display grant is asked here once more in plain words (`console::server_question`).
fn cmd_mcp(args: &[String]) -> i32 {
    use agentrouter_aggregator::grants;
    let data = data_dir();
    match args.first().map(String::as_str) {
        Some("add") => mcp_add(&data, &args[1..]),
        None | Some("list") => {
            let own = grants::own(&data);
            if own.is_empty() {
                println!(
                    "还没有自己加的本机工具。用 agentrouter mcp add <名字> -- <命令> 加一个。"
                );
                return 0;
            }
            println!("自己加的本机工具（每次 agentrouter link 都会启动）：");
            for t in own {
                let Ok(def) = agentrouter_aggregator::servers::ServerDef::parse(&t) else {
                    continue;
                };
                let how = if def.display {
                    "能看到并操作屏幕，以你的权限运行"
                } else {
                    "在文件夹的沙箱里运行"
                };
                let granted = if grants::granted(&data, &def) {
                    ""
                } else {
                    "（下次启动时会再问你）"
                };
                println!(
                    "  {}（{}）：{} · {how}{granted}",
                    def.name,
                    def.title,
                    def.shown()
                );
            }
            0
        }
        Some("remove") => match args.get(1) {
            Some(name) => match grants::own_remove(&data, name) {
                Ok(true) => {
                    println!("去掉了 {name}；正在运行的链接里它会在下次 agentrouter link 时消失。");
                    0
                }
                Ok(false) => {
                    eprintln!("没有叫 {name} 的本机工具（agentrouter mcp list 看看有哪些）。");
                    1
                }
                Err(e) => {
                    eprintln!("没去掉：{e}");
                    1
                }
            },
            None => {
                eprintln!("要写名字：agentrouter mcp remove <名字>");
                2
            }
        },
        Some(other) => {
            eprintln!("不认识的 mcp 命令：{other}\n\n{USAGE}");
            2
        }
    }
}

fn mcp_add(data: &std::path::Path, args: &[String]) -> i32 {
    use agentrouter_aggregator::grants;
    let Some(split) = args.iter().position(|a| a == "--") else {
        eprintln!(
            "要在 -- 后面写启动它的命令，比如：agentrouter mcp add screen --display -- uvx windows-mcp"
        );
        return 2;
    };
    let (ours, command) = (&args[..split], &args[split + 1..]);
    let (mut name, mut title, mut network, mut display, mut yes) =
        (None, None, Vec::new(), false, false);
    let mut it = ours.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--display" => display = true,
            // Typed by the person on this computer, like `link --yes-servers`: the answer given in advance.
            "--yes" => yes = true,
            "--title" => title = it.next().cloned(),
            "--network" => {
                network = it
                    .next()
                    .map(|v| {
                        v.split(',')
                            .map(|d| d.trim().to_string())
                            .filter(|d| !d.is_empty())
                            .collect()
                    })
                    .unwrap_or_default()
            }
            other if other.starts_with("--") => {
                eprintln!("不认识的选项：{other}\n\n{USAGE}");
                return 2;
            }
            other if name.is_none() => name = Some(other.to_string()),
            other => {
                eprintln!("多了一个参数：{other}（命令写在 -- 后面）");
                return 2;
            }
        }
    }
    let (Some(name), Some(program)) = (name, command.first()) else {
        eprintln!("要写名字和命令：agentrouter mcp add <名字> -- <命令> [参数…]");
        return 2;
    };
    let def = match agentrouter_aggregator::servers::ServerDef::parse(&serde_json::json!({
        "name": name, "title": title.unwrap_or_default(), "command": program, "args": &command[1..],
        "network": network, "display": display,
    })) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", e.message);
            return 2;
        }
    };
    // The screen: asked in plain words even though the person typed the command (owner draft 2026-10-22).
    if def.display && !yes && !crate::console::ask(&crate::console::server_question(&def)) {
        println!("没有加。");
        return 1;
    }
    if let Err(e) = grants::own_add(data, &def).and_then(|()| grants::record(data, &def)) {
        eprintln!("没加上：{e}");
        return 1;
    }
    let how = if def.display {
        "它能看到并操作屏幕，以你的权限运行"
    } else {
        "它在文件夹的沙箱里运行"
    };
    println!(
        "加好了：{}（{}）。{how}；下次 agentrouter link 时启动，云端 Agent 就能用它的工具。",
        def.name,
        def.shown()
    );
    0
}

/// `agentrouter sandbox setup|status|remove|userns` (docs/HOSTD-MCP.md §6, #42, #122).
fn cmd_sandbox(flags: Flags) -> i32 {
    // Run inside the sandbox by its entry, as the sandbox account (device #129): nothing here touches a data folder.
    if flags.positional.first().map(String::as_str) == Some("job-join") {
        return sandbox_job_join(&flags.positional[1..]);
    }
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
        // Run with sudo: nothing here reads the person's data folder (#122).
        Some("userns") => match flags.positional.get(1).map(String::as_str) {
            Some("install") => report(agentrouter_sandbox::userns::install()),
            Some("remove") => report(agentrouter_sandbox::userns::remove()),
            _ => {
                eprintln!(
                    "用法：{}（或 remove 撤销）。只在 AppArmor 限制用户命名空间的 Linux（Ubuntu 23.10 起）上需要",
                    agentrouter_sandbox::userns::admin_line("install")
                );
                2
            }
        },
        None | Some("status") => {
            let st = rt.status();
            print_sandbox_status(&st);
            if st.ready { 0 } else { 1 }
        }
        Some(other) => {
            eprintln!("不认识的 sandbox 子命令：{other}（setup、status、remove 或 userns）");
            2
        }
    }
}

/// `agentrouter sandbox job-join <name> <pid>` (Windows, device #129): the sandbox entry puts itself into the named
/// Job object hostd made with the environment's limits, before it starts the server. Not for people to run.
fn sandbox_job_join(args: &[String]) -> i32 {
    let (Some(name), Some(pid)) = (
        args.first(),
        args.get(1).and_then(|p| p.parse::<u32>().ok()),
    ) else {
        eprintln!(
            "用法：agentrouter sandbox job-join <作业对象名> <进程号>（由隔离环境的入口调用）"
        );
        return 2;
    };
    #[cfg(windows)]
    {
        match agentrouter_device::limits::join(name, pid) {
            Ok(()) => {
                // The entry starts the server only on this exact answer (another program answering 0 is no join).
                println!("joined");
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (name, pid);
        eprintln!(
            "sandbox job-join 只用在 Windows 上（Linux 和 macOS 的资源上限从隔离环境外面管）"
        );
        2
    }
}

fn print_sandbox_status(st: &agentrouter_sandbox::Status) {
    println!(
        "隔离环境 {}（{}）：{}",
        st.backend,
        st.mechanism,
        if st.ready { "可用" } else { "不可用" }
    );
    for p in st.problems.iter().chain(&st.notes) {
        println!("  {}", p.message);
        println!("  {}", p.next);
    }
}

/// `agentrouter data list|add|rename|remove` (Cloud `docs/development/DEVICES.md` §2.5): named local data — a folder
/// registered once under a name that spaces require by name (`{ local: 素材 }`), never by path. Saved in `config.json`;
/// a running link reports the change and, in folder mode, adds or takes the folder out of its grants within seconds.
fn cmd_data(args: &[String]) -> i32 {
    use agentrouter_device::config::Config;
    use agentrouter_device::local_data::{self, LocalData};
    const USE: &str = "用法：agentrouter data add <名字> <文件夹> [--read-only] | list | rename <旧名字> <新名字> | remove <名字>";
    let mut cfg = Config::load();
    let save = |cfg: &Config, done: &str| match cfg.save() {
        Ok(()) => {
            println!("{done}");
            0
        }
        Err(e) => {
            eprintln!("没保存：{e}（{}）", Config::path().display());
            1
        }
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            if cfg.local_data.is_empty() {
                println!("还没有命名文件夹。{USE}");
                return 0;
            }
            println!("命名文件夹（Agent 按名字用，平台按名字找有它的电脑）：");
            for d in &cfg.local_data {
                let state = if !std::path::Path::new(&d.path).is_dir() {
                    " · 现在找不到"
                } else if d.read_only {
                    " · 只读"
                } else {
                    ""
                };
                println!("  {} → {}{state}", d.name, d.path);
            }
            0
        }
        Some("add") => {
            let mut read_only = false;
            let mut positional = Vec::new();
            for a in &args[1..] {
                match a.as_str() {
                    "--read-only" => read_only = true,
                    other if other.starts_with("--") => {
                        eprintln!("不认识的选项：{other}\n{USE}");
                        return 2;
                    }
                    other => positional.push(other.to_string()),
                }
            }
            let [name, folder] = positional.as_slice() else {
                eprintln!("{USE}");
                return 2;
            };
            let real = match agentrouter_aggregator::check_folder(
                std::path::Path::new(folder),
                &agentrouter_device::util::data_dir(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let path = real.to_string_lossy().into_owned();
            if let Some(other) = cfg
                .local_data
                .iter()
                .find(|d| d.path == path && !local_data::same_name(&d.name, name))
            {
                eprintln!(
                    "{path} 已经叫 {} 了（改名用 agentrouter data rename）。",
                    other.name
                );
                return 1;
            }
            let entry = LocalData {
                name: name.clone(),
                path: path.clone(),
                read_only,
            };
            if let Err(e) = local_data::upsert(&mut cfg.local_data, entry) {
                eprintln!("{e}");
                return 2;
            }
            let mode = if read_only { "（只读）" } else { "" };
            save(&cfg, &format!("加上了 {} → {path}{mode}。", name.trim()))
        }
        Some("rename") => {
            let (Some(from), Some(to)) = (args.get(1), args.get(2)) else {
                eprintln!("{USE}");
                return 2;
            };
            if let Err(e) = local_data::rename(&mut cfg.local_data, from, to) {
                eprintln!("{e}");
                return 1;
            }
            save(
                &cfg,
                &format!(
                    "{} 改名为 {}（用旧名字的空间要跟着改）。",
                    from.trim(),
                    to.trim()
                ),
            )
        }
        Some("remove") => {
            let Some(name) = args.get(1) else {
                eprintln!("{USE}");
                return 2;
            };
            if !local_data::remove(&mut cfg.local_data, name) {
                eprintln!("没有叫 {} 的命名文件夹。", name.trim());
                return 1;
            }
            save(
                &cfg,
                &format!(
                    "拿掉了 {}：Agent 从现在起用不到它，用着它的 Agent 会被停下。",
                    name.trim()
                ),
            )
        }
        Some(other) => {
            eprintln!("没有 data {other}。{USE}");
            2
        }
    }
}

/// `agentrouter software list|add|hide|unhide|source` (Cloud `docs/development/DEVICES.md` §2.3): the person stays in
/// charge of what this computer reports. Changes are saved in `config.json`; a running link picks them up within a
/// minute (`console::report_software`).
fn cmd_software(args: &[String]) -> i32 {
    use agentrouter_device::config::Config;
    use agentrouter_device::software::{self, SOURCES};
    let mut cfg = Config::load();
    let save = |cfg: &Config, done: &str| match cfg.save() {
        Ok(()) => {
            println!("{done}");
            0
        }
        Err(e) => {
            eprintln!("没保存：{e}（{}）", Config::path().display());
            1
        }
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let found = software::discover(&cfg.software);
            println!(
                "这台电脑上的软件和游戏（{} 项，平台按它们找能干活的电脑）：",
                found.software.len()
            );
            for s in &found.software {
                let version = s
                    .version
                    .as_deref()
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                let only_one = if s.exclusive {
                    " · 同一时间只给一个 Agent"
                } else {
                    ""
                };
                println!("  {}{version} · {} · {}{only_one}", s.name, s.source, s.id);
            }
            for line in &found.status {
                println!("  （没读到：{line}）");
            }
            if !cfg.software.hidden.is_empty() {
                println!(
                    "隐藏了 {} 项（agentrouter software unhide <id> 取消）。",
                    cfg.software.hidden.len()
                );
            }
            0
        }
        Some("add") => {
            let mut name = None;
            let (mut path, mut version, mut exclusive) = (None, None, false);
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--path" => path = rest.next().map(std::path::PathBuf::from),
                    "--version" => version = rest.next().cloned(),
                    "--exclusive" => exclusive = true,
                    other if name.is_none() && !other.starts_with("--") => {
                        name = Some(other.to_string())
                    }
                    other => {
                        eprintln!(
                            "看不懂 {other}：agentrouter software add <名字> [--path <文件夹或程序>] [--version <版本>] [--exclusive]"
                        );
                        return 2;
                    }
                }
            }
            let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
                eprintln!("要写名字：agentrouter software add <名字> [--path <文件夹或程序>]");
                return 2;
            };
            if let Some(p) = &path
                && !p.exists()
            {
                eprintln!("{} 不存在，没有加。", p.display());
                return 1;
            }
            let entry =
                software::manual_entry(&name, path.as_deref(), version.as_deref(), exclusive);
            let id = entry.id.clone();
            cfg.software.added.retain(|s| s.id != id);
            cfg.software.added.push(entry);
            save(&cfg, &format!("加上了 {name}（{id}）。"))
        }
        Some(verb @ ("hide" | "unhide")) => {
            let Some(id) = args.get(1) else {
                eprintln!(
                    "要写 id：agentrouter software {verb} <id>（id 见 agentrouter software list）"
                );
                return 2;
            };
            cfg.software.hidden.retain(|h| h != id);
            if verb == "hide" {
                cfg.software.hidden.push(id.clone());
                // A hand-added entry that is hidden is simply removed.
                cfg.software.added.retain(|s| &s.id != id);
                cfg.software.hidden.retain(|h| !h.starts_with("manual:"));
                save(&cfg, &format!("{id} 不再报给平台。"))
            } else {
                save(&cfg, &format!("{id} 会再报给平台。"))
            }
        }
        Some("source") => {
            let (Some(source), Some(state)) = (args.get(1), args.get(2)) else {
                eprintln!(
                    "用法：agentrouter software source <来源> on|off（来源：{}）",
                    SOURCES.join("、")
                );
                return 2;
            };
            if !SOURCES.contains(&source.as_str()) {
                eprintln!("没有叫 {source} 的来源（有：{}）。", SOURCES.join("、"));
                return 2;
            }
            let on = match state.as_str() {
                "on" => true,
                "off" => false,
                _ => {
                    eprintln!("要写 on 或 off。");
                    return 2;
                }
            };
            if on {
                cfg.software.sources.remove(source);
            } else {
                cfg.software.sources.insert(source.clone(), false);
            }
            save(
                &cfg,
                &format!("来源 {source} 已{}。", if on { "打开" } else { "关掉" }),
            )
        }
        Some(other) => {
            eprintln!("没有 software {other}；可以用 list、add、hide、unhide、source。");
            2
        }
    }
}
