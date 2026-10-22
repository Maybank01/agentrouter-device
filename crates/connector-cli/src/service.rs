//! `agentrouter service install|uninstall|status|run` (#94): the person turns on, with this explicit command only, that
//! this computer links itself at login and stays linked (docs/AV-HYGIENE.md「开机自启只由本人打开」). The operating
//! system's own mechanism starts it — a systemd user unit (Linux), a launchd LaunchAgent (macOS), a Task Scheduler
//! logon task (Windows); no Run key, no startup folder, no supervisor of our own. The definition holds only
//! `<agentrouter> service run`; the folders and the access level are in `service.json` in the data folder. The link
//! reconnects by itself (backoff); the OS restarts it only after a crash, so a link the gateway ends (removed on the
//! web, an outdated version) stays stopped and `service status` says why.

use std::path::{Path, PathBuf};
use std::process::Command;

use agentrouter_device::keystore;
use agentrouter_device::util::data_dir;
use serde_json::{Value, json};

/// The systemd user unit (Linux).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const UNIT: &str = "agentrouter-link.service";
/// The LaunchAgent's label (macOS).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LABEL: &str = "top.agentrouter.link";
/// The Task Scheduler task (Windows).
#[cfg_attr(not(windows), allow(dead_code))]
const TASK: &str = "AgentRouter Link";

fn config_path(data: &Path) -> PathBuf {
    data.join("service.json")
}

fn state_path(data: &Path) -> PathBuf {
    data.join("service-state.json")
}

/// `agentrouter service …`; `dirs`, `read_only`, `full`, `yes_servers` are `install`'s.
pub fn cmd(
    sub: Option<&str>,
    dirs: &[String],
    read_only: bool,
    full: bool,
    yes_servers: bool,
) -> i32 {
    match sub {
        Some("install") => install(dirs, read_only, full, yes_servers),
        Some("uninstall") => uninstall(),
        None | Some("status") => status(),
        Some("run") => run(),
        Some(other) => {
            eprintln!("不认识的 service 命令：{other}（install、uninstall、status）");
            2
        }
    }
}

fn install(dirs: &[String], read_only: bool, full: bool, yes_servers: bool) -> i32 {
    if full && read_only {
        eprintln!("--full 和 --read-only 不能一起用。");
        return 2;
    }
    if std::env::var_os("AGENTROUTER_DEVICE_HOME").is_some() {
        eprintln!(
            "开机自动连接只用默认的数据文件夹：先去掉环境变量 AGENTROUTER_DEVICE_HOME 再装。"
        );
        return 2;
    }
    if keystore::load().is_none() {
        eprintln!(
            "这台电脑还没有链接：先在文件夹里运行一次 agentrouter link，在网页上确认，再装开机自动连接。"
        );
        return 2;
    }
    let folders = match crate::console::chosen_folders(dirs, full) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let exe = match std::env::current_exe().and_then(std::fs::canonicalize) {
        Ok(p) => crate::console::shown(&p),
        Err(e) => {
            eprintln!("找不到 agentrouter 程序本身的位置：{e}");
            return 1;
        }
    };
    if exe.contains("_npx") {
        // npx runs the CLI from its cache, which npm may clear: the login item would then point at nothing.
        eprintln!(
            "这个 agentrouter 是 npx 临时下载的（{exe}），缓存清掉后开机就启动不了：先 npm install -g @agentrouter-top/cli，再用装好的 agentrouter service install。"
        );
        return 2;
    }
    let data = data_dir();
    let folders: Vec<String> = folders.iter().map(|f| crate::console::shown(f)).collect();
    let config = json!({
        "folders": folders, "readOnly": read_only, "full": full, "yesServers": yes_servers,
        "program": exe, "installedAt": chrono::Utc::now().to_rfc3339(),
    });
    if let Err(e) = std::fs::create_dir_all(&data).and_then(|_| {
        std::fs::write(
            config_path(&data),
            serde_json::to_vec_pretty(&config).unwrap_or_default(),
        )
    }) {
        eprintln!("写不进 {}：{e}", config_path(&data).display());
        return 1;
    }
    match os::install(&exe, &data) {
        Ok(where_) => {
            println!("已打开开机自动连接：登录这台电脑后自动链接");
            for f in &folders {
                println!("  {f}");
            }
            if full {
                println!("  完全访问：云端 Agent 用你的账户权限，整台电脑都能读写。");
            } else if read_only {
                println!("  只读。");
            }
            println!("  由{where_}启动；现在已经在运行。");
            println!("看状态：agentrouter service status；关掉：agentrouter service uninstall");
            0
        }
        Err(e) => {
            eprintln!("没装上：{e}");
            1
        }
    }
}

fn uninstall() -> i32 {
    let data = data_dir();
    let removed = os::uninstall();
    let had_config = config_path(&data).exists();
    let _ = std::fs::remove_file(config_path(&data));
    let _ = std::fs::remove_file(state_path(&data));
    match removed {
        Ok(true) => {
            println!("已关掉开机自动连接（这台电脑仍然链接着账号；agentrouter logout 才会退出）。");
            0
        }
        Ok(false) if had_config => {
            println!("系统里没有找到它的启动项；设置已删掉。");
            0
        }
        Ok(false) => {
            println!("本来就没有打开开机自动连接。");
            0
        }
        Err(e) => {
            eprintln!("没关掉：{e}");
            1
        }
    }
}

fn status() -> i32 {
    let data = data_dir();
    let Some(config) = read_json(&config_path(&data)) else {
        println!("开机自动连接：没有打开（agentrouter service install [文件夹…] 打开）");
        return 0;
    };
    let running = os::running();
    println!(
        "开机自动连接：已打开（{}）",
        match &running {
            Ok(true) => "正在运行".to_string(),
            Ok(false) => "现在没有运行".to_string(),
            Err(e) => format!("看不到运行状态：{e}"),
        }
    );
    for f in config["folders"].as_array().into_iter().flatten() {
        println!("  文件夹   {}", f.as_str().unwrap_or(""));
    }
    if config["full"] == true {
        println!("  访问     完全访问");
    } else if config["readOnly"] == true {
        println!("  访问     只读");
    }
    if let Some(state) = read_json(&state_path(&data)) {
        let at = state["at"].as_str().unwrap_or("");
        let text = match state["state"].as_str().unwrap_or("") {
            "online" => "在线".to_string(),
            "offline" => format!(
                "连接断了，正在重连（{}）",
                state["detail"].as_str().unwrap_or("")
            ),
            "ended" => format!("已停止：{}", state["detail"].as_str().unwrap_or("")),
            "starting" => "正在启动".to_string(),
            other => other.to_string(),
        };
        println!("  链接     {text}（{at}）");
    }
    if matches!(running, Ok(false)) {
        println!(
            "  要现在启动：agentrouter service install 再运行一次；为什么停了：{}",
            os::LOG_HINT
        );
    }
    0
}

/// What the OS starts at login: the link with the installed folders, no terminal questions (a custom tool or
/// another folder is then refused, as a `n` would be).
fn run() -> i32 {
    let data = data_dir();
    let Some(config) = read_json(&config_path(&data)) else {
        eprintln!(
            "开机自动连接的设置不见了（{}）：运行 agentrouter service install 重新打开，或 agentrouter service uninstall 关掉。",
            config_path(&data).display()
        );
        return 2;
    };
    if keystore::load().is_none() {
        record("ended", "这台电脑没有链接身份了（退出了或在网页上被移除）");
        eprintln!(
            "这台电脑没有链接身份了：在文件夹里运行一次 agentrouter link 重新确认，开机自动连接会接着用。"
        );
        return 2;
    }
    let dirs = config["folders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(str::to_string))
        .collect();
    record("starting", "");
    crate::console::link(
        crate::console::LinkOptions {
            dirs,
            read_only: config["readOnly"] == true,
            full: config["full"] == true,
            yes_servers: config["yesServers"] == true,
            gateway: None,
            name: None,
            double_click: false,
            service: true,
        },
        &crate::cli::INTERRUPTED,
    )
}

/// The service's link state for `service status` (written by `console::link` when it runs as the service).
pub fn record(state: &str, detail: &str) {
    let data = data_dir();
    let body = json!({
        "state": state, "detail": agentrouter_device::util::clip(detail, 200),
        "at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), "pid": std::process::id(),
    });
    let _ = std::fs::write(
        state_path(&data),
        serde_json::to_vec(&body).unwrap_or_default(),
    );
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Run one OS tool with fixed arguments (names and paths only); its own words on failure.
fn tool(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program} 运行不了：{e}"))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if out.status.success() {
        Ok(text(&out.stdout))
    } else {
        Err(format!(
            "{program} {}：{}",
            args.join(" "),
            agentrouter_device::util::clip(
                [text(&out.stderr), text(&out.stdout)].join(" ").trim(),
                300
            )
        ))
    }
}

/// The definitions themselves (pure, tested on every OS; each OS uses its own).
#[allow(dead_code)]
pub mod def {
    /// A systemd user unit: restart only after a crash (`on-abnormal`), not when the link ends on purpose.
    pub fn systemd_unit(exe: &str) -> String {
        format!(
            "[Unit]\nDescription=AgentRouter link (agentrouter service install)\nAfter=network-online.target\n\n\
             [Service]\nExecStart={} service run\nRestart=on-abnormal\nRestartSec=10\n\n\
             [Install]\nWantedBy=default.target\n",
            systemd_quote(exe)
        )
    }

    /// systemd's command-line quoting: double quotes, `\` and `"` escaped, `%` and `$` doubled (specifiers and
    /// variables are expanded otherwise).
    pub fn systemd_quote(arg: &str) -> String {
        let mut out = String::from("\"");
        for c in arg.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '%' => out.push_str("%%"),
                '$' => out.push_str("$$"),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    pub fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    /// A LaunchAgent: at login, restarted only after a crash; its output goes to `log`.
    pub fn launch_agent(label: &str, exe: &str, log: &str) -> String {
        let (label, exe, log) = (xml_escape(label), xml_escape(exe), xml_escape(log));
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>service</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>Crashed</key><true/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
        )
    }

    /// A Task Scheduler task for this user at their logon: their own rights (least privilege, interactive token, no
    /// stored password), its console window visible, no time limit, one instance.
    pub fn task_xml(user: &str, exe: &str) -> String {
        let (user, exe) = (xml_escape(user), xml_escape(exe));
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>AgentRouter link (agentrouter service install)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>{exe}</Command><Arguments>service run</Arguments></Exec>
  </Actions>
</Task>
"#
        )
    }

    /// Task Scheduler reads a task file as UTF-16 (little endian, with its byte-order mark).
    pub fn utf16le(text: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;

    pub const LOG_HINT: &str = "journalctl --user -u agentrouter-link -n 50";

    fn unit_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| agentrouter_device::util::home_dir().join(".config"));
        base.join("systemd").join("user").join(UNIT)
    }

    pub fn install(exe: &str, _data: &Path) -> Result<&'static str, String> {
        let path = unit_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}：{e}", dir.display()))?;
        }
        std::fs::write(&path, def::systemd_unit(exe))
            .map_err(|e| format!("{}：{e}", path.display()))?;
        tool("systemctl", &["--user", "daemon-reload"])?;
        tool("systemctl", &["--user", "enable", UNIT])?;
        // A running one picks up the new folders.
        tool("systemctl", &["--user", "restart", UNIT])?;
        Ok("systemd（用户服务 agentrouter-link）")
    }

    pub fn uninstall() -> Result<bool, String> {
        let path = unit_path();
        if !path.exists() {
            return Ok(false);
        }
        let _ = tool("systemctl", &["--user", "disable", "--now", UNIT]);
        std::fs::remove_file(&path).map_err(|e| format!("{}：{e}", path.display()))?;
        tool("systemctl", &["--user", "daemon-reload"])?;
        Ok(true)
    }

    pub fn running() -> Result<bool, String> {
        // `is-active` exits non-zero for anything but active: its output is the answer.
        let out = Command::new("systemctl")
            .args(["--user", "is-active", UNIT])
            .output()
            .map_err(|e| format!("systemctl 运行不了：{e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "active")
    }
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;

    pub const LOG_HINT: &str = "数据文件夹里的 service.log";

    fn plist_path() -> PathBuf {
        agentrouter_device::util::home_dir()
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn domain() -> String {
        // SAFETY: getuid has no preconditions and cannot fail.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    pub fn install(exe: &str, data: &Path) -> Result<&'static str, String> {
        let path = plist_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}：{e}", dir.display()))?;
        }
        let log = data.join("service.log");
        std::fs::write(&path, def::launch_agent(LABEL, exe, &log.to_string_lossy()))
            .map_err(|e| format!("{}：{e}", path.display()))?;
        let target = format!("{}/{LABEL}", domain());
        // A loaded one is replaced, so the new folders apply.
        let _ = tool("launchctl", &["bootout", &target]);
        tool(
            "launchctl",
            &["bootstrap", &domain(), &path.to_string_lossy()],
        )?;
        Ok("launchd（登录项 top.agentrouter.link）")
    }

    pub fn uninstall() -> Result<bool, String> {
        let path = plist_path();
        if !path.exists() {
            return Ok(false);
        }
        let _ = tool("launchctl", &["bootout", &format!("{}/{LABEL}", domain())]);
        std::fs::remove_file(&path).map_err(|e| format!("{}：{e}", path.display()))?;
        Ok(true)
    }

    pub fn running() -> Result<bool, String> {
        match tool("launchctl", &["print", &format!("{}/{LABEL}", domain())]) {
            Ok(text) => Ok(text.lines().any(|l| l.trim() == "state = running")),
            Err(_) => Ok(false),
        }
    }
}

#[cfg(windows)]
mod os {
    use super::*;

    pub const LOG_HINT: &str = "它自己的窗口里最后几行（关掉窗口就停了）";

    fn user() -> Result<String, String> {
        let name =
            std::env::var("USERNAME").map_err(|_| "看不到当前用户名（USERNAME）".to_string())?;
        Ok(match std::env::var("USERDOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("{domain}\\{name}"),
            _ => name,
        })
    }

    pub fn install(exe: &str, data: &Path) -> Result<&'static str, String> {
        // The definition goes through a file in the data folder, not the command line.
        let file = data.join("agentrouter-link-task.xml");
        std::fs::write(&file, def::utf16le(&def::task_xml(&user()?, exe)))
            .map_err(|e| format!("{}：{e}", file.display()))?;
        let created = tool(
            "schtasks",
            &[
                "/Create",
                "/TN",
                TASK,
                "/XML",
                &file.to_string_lossy(),
                "/F",
            ],
        );
        let _ = std::fs::remove_file(&file);
        created?;
        // A running one picks up the new folders.
        let _ = tool("schtasks", &["/End", "/TN", TASK]);
        tool("schtasks", &["/Run", "/TN", TASK])?;
        Ok("任务计划程序（任务 AgentRouter Link，登录时）")
    }

    pub fn uninstall() -> Result<bool, String> {
        if tool("schtasks", &["/Query", "/TN", TASK]).is_err() {
            return Ok(false);
        }
        let _ = tool("schtasks", &["/End", "/TN", TASK]);
        tool("schtasks", &["/Delete", "/TN", TASK, "/F"])?;
        Ok(true)
    }

    pub fn running() -> Result<bool, String> {
        // The CSV's last field is the status, in the system's language.
        let out = tool("schtasks", &["/Query", "/TN", TASK, "/FO", "CSV", "/NH"])?;
        Ok(out.contains("\"Running\"") || out.contains("\"正在运行\""))
    }
}

#[cfg(test)]
mod tests {
    use super::def::*;

    #[test]
    fn a_systemd_unit_quotes_the_program() {
        let unit = systemd_unit("/home/a b/50%/$x/\"q\"/agentrouter");
        assert!(
            unit.contains(r#"ExecStart="/home/a b/50%%/$$x/\"q\"/agentrouter" service run"#),
            "{unit}"
        );
        assert!(unit.contains("Restart=on-abnormal") && unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn plist_and_task_escape_paths_and_run_only_service_run() {
        let plist = launch_agent(
            "top.agentrouter.link",
            "/Users/a&b/agentrouter",
            "/tmp/l<og",
        );
        assert!(plist.contains("<string>/Users/a&amp;b/agentrouter</string><string>service</string><string>run</string>"));
        assert!(plist.contains("<key>Crashed</key><true/>") && plist.contains("/tmp/l&lt;og"));
        let task = task_xml(
            r"PC\Ann & Bo",
            r"C:\Program Files\AgentRouter\agentrouter.exe",
        );
        assert!(task.contains(r"<UserId>PC\Ann &amp; Bo</UserId>"));
        assert!(task.contains(r"<Command>C:\Program Files\AgentRouter\agentrouter.exe</Command><Arguments>service run</Arguments>"));
        assert!(
            task.contains("<RunLevel>LeastPrivilege</RunLevel>")
                && task.contains("<LogonType>InteractiveToken</LogonType>")
        );
        let bytes = utf16le("<a/>");
        assert_eq!(&bytes[..4], &[0xFF, 0xFE, b'<', 0]);
    }
}
