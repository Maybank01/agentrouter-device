//! `agentrouter` (also installed as `agentrouter-device`): console mode. Run `agentrouter link` in any folder and
//! the cloud Agent works in that folder for as long as the command runs (docs/LINKER-MCP.md §8; `console.rs`).
//! `agentrouter mcp-serve local` (main.rs) is the local-Agent entry, kept pending its own design (LINKER-MCP §4).

use std::sync::atomic::{AtomicBool, Ordering};

use agentrouter_device::util::data_dir;

/// Set by Ctrl+C (one handler for the whole run: pairing, then the link).
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

const USAGE: &str = "AgentRouter（agentrouter）：在任意文件夹里运行 agentrouter link，云端对话里的 Agent 就能在这个文件夹里干活

用法：
  agentrouter link [文件夹…] [--read-only | --full] [--yes-servers]
                                链接这些文件夹（不写就是当前文件夹；第一次会显示配对码，在网页上确认）。命令开着就一直连着，Ctrl+C 断开。
                                云端 Agent 只能在这些文件夹里干活：每个本机工具都在系统沙箱里，文件夹外的写、删直接被拒绝。
                                连着的时候输入 + 路径 可以再加一个文件夹。
      --read-only               这些文件夹只读
      --full                    完全访问：本机工具用你的账户权限，整台电脑都能读写（只能在这台电脑上打开）
      --yes-servers             不在目录里的本机工具不用逐个问（无人值守的电脑）
      --gateway <网址>  --name <这台电脑的名字>
  agentrouter link --placement cloud <文件夹…> [--token-file <文件>] [--gateway <网址>]
                                云端沙箱里的 hostd（由沙箱启动，不配对、不问人）：平台发的主机令牌
                                （AGENTROUTER_HOST_TOKEN_FILE 或 AGENTROUTER_HOST_TOKEN）、平台签名公钥
                                （AGENTROUTER_CONTROL_KEY）；沙箱本身就是边界，没有系统沙箱和检查点
  agentrouter undo [--list] [--journal] [检查点]
                                撤销 Agent 在这个文件夹里最近一轮的改动（再运行一次就再往前一轮）；--list 列出检查点；
                                --journal 放回完全访问时 Agent 改过的文件的旧版
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
        println!("{USAGE}");
        std::process::exit(0);
    };
    let flags = match parse(&args[1..]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
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
            },
            &INTERRUPTED,
        ),
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
