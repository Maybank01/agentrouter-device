//! Console mode (docs/HOSTD-MCP.md §8, issue #48): `agentrouter link` in any folder links **that folder** to the
//! person's own AgentRouter account for as long as the command runs. hostd's link layer keeps the link; the aggregator
//! serves the cloud Agent one MCP endpoint whose servers the OS confines to the folder; the terminal shows every
//! call in one line. Ctrl+C stops the servers and the link.
//!
//! Also here: `undo [--list] [id]` (shadow-git checkpoints of this folder), `audit [verify] [n]` (this folder's
//! argument-level audit), `status`, `logout`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agentrouter_aggregator::servers::ServerDef;
use agentrouter_aggregator::{
    Aggregator, BoxFuture, Config as AggConfig, Feed, Hooks, Outcome, audit_path,
    is_boundary_refusal,
};
use agentrouter_checkpoint::{Journal, Shadow, folder_key};
use agentrouter_device::config::Config;
use agentrouter_device::local_data::{self, LocalData};
use agentrouter_device::util::{data_dir, home_dir};
use agentrouter_device::{audit, keystore, link};
use agentrouter_hostd_link::{End, Event, LinkInfo};
use serde_json::{Value, json};

/// How long the person has to answer a y/N on this computer (no answer = no).
const ASK_FOR: Duration = Duration::from_secs(120);

pub struct LinkOptions {
    /// The folders to link (none = the current folder).
    pub dirs: Vec<String>,
    pub read_only: bool,
    /// 完全访问 (§6.4): chosen here only.
    pub full: bool,
    pub yes_servers: bool,
    pub gateway: Option<String>,
    pub name: Option<String>,
    /// Started by a double-click (`start.rs`): it said the access line itself; pairing opens the browser.
    pub double_click: bool,
    /// Started at login by `agentrouter service` (#94): the link state goes to `service-state.json` for `service status`.
    pub service: bool,
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn line(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// A path as the person reads it (no `\\?\`).
pub(crate) fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// The folder this command runs in, canonical; never the home folder, a drive root or AgentRouter's own data.
fn this_folder() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("看不到当前文件夹：{e}"))?;
    let real = std::fs::canonicalize(&cwd).map_err(|e| format!("看不到当前文件夹：{e}"))?;
    let real = PathBuf::from(shown(&real));
    let home = std::fs::canonicalize(home_dir())
        .map(|h| PathBuf::from(shown(&h)))
        .unwrap_or_else(|_| home_dir());
    let data = data_dir();
    if real.parent().is_none()
        || real == home
        || home.starts_with(&real)
        || real.starts_with(&data)
        || data.starts_with(&real)
    {
        return Err(format!(
            "{} 太大了：不能把用户主目录、磁盘根目录或 AgentRouter 自己的数据文件夹交给云端。请到一个具体的项目文件夹里运行。",
            shown(&real)
        ));
    }
    Ok(real)
}

fn folder_name(folder: &Path) -> String {
    folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| shown(folder))
}

/// The link id of `folder` for the paired identity.
fn link_of(folder: &Path) -> Result<(keystore::Identity, String), String> {
    let identity =
        keystore::load().ok_or("这台电脑还没有链接：先在这个文件夹里运行 agentrouter link")?;
    let link = agentrouter_hostd_link::link_id(&identity.device, folder);
    Ok((identity, link))
}

/// Lines typed in the terminal, read on one thread for the whole run.
pub(crate) fn stdin_lines() -> &'static Mutex<Receiver<String>> {
    static LINES: OnceLock<Mutex<Receiver<String>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in std::io::stdin().lock().lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Mutex::new(rx)
    })
}

/// A y/N on this computer; no terminal or no answer in time = no.
pub(crate) fn ask(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        line(&format!("{question}\n（没有终端可以回答：按不同意处理）"));
        return false;
    }
    let lines = stdin_lines().lock().unwrap();
    while lines.try_recv().is_ok() {}
    line(&format!(
        "{question} (y/N，{} 秒内不答就是不同意)",
        ASK_FOR.as_secs()
    ));
    match lines.recv_timeout(ASK_FOR) {
        Ok(answer) => matches!(answer.trim(), "y" | "Y" | "yes" | "是"),
        Err(_) => {
            line("没有回答：按不同意处理。");
            false
        }
    }
}

/// What the console does for the aggregator: the feed, the y/N questions, checkpoints, the journal, the link info.
struct Console {
    data: PathBuf,
    full: bool,
    /// One shadow repository per snapshotted folder (§7.2; keyed by the folder, so `undo` in it finds them).
    shadows: Mutex<Vec<(PathBuf, Shadow)>>,
    /// 完全访问's file journal and its current turn.
    journal: Option<Journal>,
    turn: Mutex<Option<String>>,
    /// The first folder (relative paths in arguments resolve there).
    home: PathBuf,
    asking: Arc<tokio::sync::Mutex<()>>,
    no_git_said: AtomicBool,
    /// The link's info and handle: `info` goes out when the folders change.
    info: Mutex<LinkInfo>,
    /// The named local data registered on this computer (`config.json` `local_data`), for `link.localData`.
    registered: Mutex<Vec<LocalData>>,
    handle: OnceLock<agentrouter_hostd_link::Handle>,
    /// The host's outbox, where feeds write (Cloud #2739): set once it is open.
    events: OnceLock<Arc<dyn agentrouter_aggregator::feeds::EventSink>>,
}

/// A folder gets shadow-git checkpoints unless it is the home folder, a drive root or AgentRouter's own data
/// (only possible in 完全访问, where the start folder may be any folder).
fn snapshottable(folder: &Path, data: &Path) -> bool {
    agentrouter_aggregator::check_folder(folder, data).is_ok()
}

impl Console {
    fn shadow_of(&self, folder: &Path) -> Shadow {
        Shadow::new(&self.data, &folder_key(folder), folder)
    }

    fn set_folders(&self, folders: &[PathBuf]) {
        let mut shadows = self.shadows.lock().unwrap();
        for f in folders {
            if !shadows.iter().any(|(g, _)| g == f) && snapshottable(f, &self.data) {
                shadows.push((f.clone(), self.shadow_of(f)));
            }
        }
        // A folder that left (a removed local data entry) gets no more checkpoints; its history stays for `undo`.
        shadows.retain(|(g, _)| folders.contains(g));
        drop(shadows);
        let mut info = self.info.lock().unwrap();
        info.folders = folders.iter().map(|f| shown(f)).collect();
        info.local_data = local_data_view(&self.registered.lock().unwrap(), folders);
        if let Some(h) = self.handle.get() {
            h.set_info(info.clone());
        }
    }

    /// The person changed their named local data (`agentrouter data …`): the cloud gets the new `link.localData`.
    fn set_registered(&self, list: Vec<LocalData>) {
        *self.registered.lock().unwrap() = list;
        let mut info = self.info.lock().unwrap();
        let folders: Vec<PathBuf> = info.folders.iter().map(PathBuf::from).collect();
        info.local_data = local_data_view(&self.registered.lock().unwrap(), &folders);
        if let Some(h) = self.handle.get() {
            h.set_info(info.clone());
        }
    }

    /// Paths a call names (its target files), real and absolute.
    fn named_paths(&self, arguments: &Value) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for key in ["path", "file_path", "destination", "target"] {
            if let Some(p) = arguments.get(key).and_then(Value::as_str)
                && !p.trim().is_empty()
            {
                let p = PathBuf::from(p.trim());
                out.push(if p.is_absolute() {
                    p
                } else {
                    self.home.join(p)
                });
            }
        }
        out
    }
}

fn outcome_text(outcome: &Outcome, ms: u64) -> String {
    match outcome {
        Outcome::Ok if ms >= 1000 => format!("✓ {:.1}s", ms as f64 / 1000.0),
        Outcome::Ok => "✓".into(),
        Outcome::ToolError(text) if is_boundary_refusal(text) => "拦下：在文件夹外".into(),
        Outcome::ToolError(text) => format!("失败：{}", agentrouter_device::util::clip(text, 60)),
        Outcome::Denied(code) => format!("拒绝：{code}"),
    }
}

impl Hooks for Console {
    fn feed(&self, event: Feed) {
        let t = now();
        match event {
            Feed::Session {
                title,
                opened: true,
            } => {
                let who = if title.is_empty() {
                    "一个对话".to_string()
                } else {
                    format!("「{title}」")
                };
                line(&format!("{t}  {who}开始使用这个文件夹"));
            }
            Feed::Session { .. } => {}
            Feed::Call {
                title,
                server_title,
                tool,
                hint,
                outcome,
                ms,
                ..
            } => {
                let title = if title.is_empty() {
                    "对话".to_string()
                } else {
                    agentrouter_device::util::clip(&title, 12)
                };
                line(&format!(
                    "{t}  {title}  {server_title} {tool} {hint}  {}",
                    outcome_text(&outcome, ms)
                ));
            }
            Feed::Server {
                title,
                event,
                detail,
                ..
            } => {
                let text = match event.as_str() {
                    "starting" => format!("正在启动本机工具「{title}」"),
                    "ready" => format!("本机工具「{title}」已就绪"),
                    "exited" | "start_failed" => format!(
                        "本机工具「{title}」退出了，正在重启{}",
                        detail
                            .map(|d| format!("（{}）", agentrouter_device::util::clip(&d, 80)))
                            .unwrap_or_default()
                    ),
                    "sandbox_unavailable" => format!(
                        "本机工具「{title}」没能在沙箱里启动：{}",
                        detail.unwrap_or_default()
                    ),
                    "stopped" => format!("本机工具「{title}」已停止"),
                    "login_started" => format!("本机工具「{title}」在等你扫码登录"),
                    "login_ok" => format!("本机工具「{title}」已登录，凭据只保存在这台电脑上"),
                    "login_failed" => format!(
                        "本机工具「{title}」没有登录成功：{}",
                        detail
                            .map(|d| agentrouter_device::util::clip(&d, 80))
                            .unwrap_or_default()
                    ),
                    "credential_saved" => {
                        format!("本机工具「{title}」的登录凭据已更新，只保存在这台电脑上")
                    }
                    "credential_forgotten" => {
                        format!("本机工具「{title}」的登录凭据已从这台电脑上删掉")
                    }
                    "limit" => format!(
                        "本机工具「{title}」碰到了资源上限：{}",
                        detail.unwrap_or_default()
                    ),
                    _ => return,
                };
                line(&format!("{t}  {text}"));
            }
            Feed::Login { title, url, .. } => {
                line(&format!(
                    "{t}  「{title}」请用手机扫码登录（用给 AI 专用的账号）：\n{}\n{url}",
                    terminal_qr(&url)
                ));
            }
        }
    }

    fn confirm_server(&self, def: &ServerDef) -> BoxFuture<bool> {
        let question = server_question(def);
        let asking = self.asking.clone();
        Box::pin(async move {
            let _one = asking.lock().await;
            tokio::task::spawn_blocking(move || ask(&question))
                .await
                .unwrap_or(false)
        })
    }

    fn confirm_folder(&self, path: &str, reason: &str) -> BoxFuture<bool> {
        let why = if reason.is_empty() {
            String::new()
        } else {
            format!("（{reason}）")
        };
        let question = format!("\n云端想再用一个文件夹：{path}{why}。同意吗？");
        let asking = self.asking.clone();
        Box::pin(async move {
            let _one = asking.lock().await;
            tokio::task::spawn_blocking(move || ask(&question))
                .await
                .unwrap_or(false)
        })
    }

    fn events(&self) -> Option<Arc<dyn agentrouter_aggregator::feeds::EventSink>> {
        self.events.get().cloned()
    }

    fn folders_changed(&self, folders: &[PathBuf]) {
        let before: Vec<PathBuf> = self
            .info
            .lock()
            .unwrap()
            .folders
            .iter()
            .map(PathBuf::from)
            .collect();
        self.set_folders(folders);
        for f in folders.iter().filter(|f| !before.contains(f)) {
            line(&format!("{}  加上了文件夹 {}", now(), shown(f)));
        }
        for f in before.iter().filter(|f| !folders.contains(f)) {
            line(&format!("{}  拿掉了文件夹 {}", now(), shown(f)));
        }
    }

    fn checkpoint(&self, label: &str) -> Option<String> {
        let shadows = self.shadows.lock().unwrap().clone();
        let mut ids = Vec::new();
        for (folder, shadow) in &shadows {
            match shadow.snapshot(label) {
                Ok(cp) => ids.push(if shadows.len() > 1 {
                    format!("{}:{}", folder_name(folder), cp.id)
                } else {
                    cp.id
                }),
                Err(e) => {
                    if !e.contains("git") || !self.no_git_said.swap(true, Ordering::SeqCst) {
                        line(&format!(
                            "{}  {} 这一轮没有检查点：{e}",
                            now(),
                            folder_name(folder)
                        ));
                    }
                }
            }
        }
        if let Some(journal) = &self.journal {
            *self.turn.lock().unwrap() = journal.begin(label).ok();
        }
        (!ids.is_empty()).then(|| ids.join(","))
    }

    fn journal(&self, _tool: &str, arguments: &Value) -> Option<String> {
        let journal = self.journal.as_ref()?;
        let shadowed: Vec<PathBuf> = self
            .shadows
            .lock()
            .unwrap()
            .iter()
            .map(|(f, _)| f.clone())
            .collect();
        let mut turn = self.turn.lock().unwrap();
        let mut kept = false;
        for path in self.named_paths(arguments) {
            if shadowed.iter().any(|f| path.starts_with(f)) {
                continue;
            }
            if turn.is_none() {
                *turn = journal.begin("").ok();
            }
            let Some(id) = turn.as_deref() else { break };
            match journal.keep(id, &path) {
                Ok(k) => kept |= k,
                Err(e) => line(&format!("{}  {e}", now())),
            }
        }
        kept.then(|| format!("journal:{}", turn.as_deref().unwrap_or("")))
    }
}

/// The y/N for a local tool (HOSTD-MCP §5, §6.7). A display grant says what it means in the owner's words (draft
/// 2026-10-22): it sees and drives the screen, so it runs with the person's own rights, outside the folders.
pub(crate) fn server_question(def: &ServerDef) -> String {
    if def.display {
        format!(
            "\n本机工具 {}：{}\n{} 要能看到并操作你的屏幕，所以它以你自己的权限运行，不受文件夹限制。允许？",
            def.title,
            def.shown(),
            def.title
        )
    } else {
        format!(
            "\n云端想在这台电脑上运行一个不在目录里的本机工具：\n  {}：{}\n它和别的本机工具一样受这次链接的限制。同意吗？",
            def.title,
            def.shown()
        )
    }
}

/// Servers the project itself names in `.agentrouter/mcp.json` (the common `mcpServers` format). They are custom:
/// each needs the person's y here.
fn project_servers(folder: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(folder.join(".agentrouter").join("mcp.json")) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        line("  .agentrouter/mcp.json 不是有效的 JSON，跳过。");
        return Vec::new();
    };
    v.get("mcpServers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(name, d)| {
                    json!({"name": name, "title": name, "command": d.get("command"), "args": d.get("args"), "env": d.get("env"), "display": d.get("display")})
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `link.localData` (Cloud DEVICES §2.5): the registered entries, then each linked folder no entry names yet, under its
/// folder name (`linked: true`; the link's unnamed folders become named entries, the registry keeps `folders` too).
fn local_data_view(registered: &[LocalData], folders: &[PathBuf]) -> Value {
    let mut view = local_data::view(registered);
    if let Value::Array(items) = &mut view {
        for f in folders {
            let (path, name) = (shown(f), folder_name(f));
            if registered.iter().any(|d| d.path == path)
                || local_data::find(registered, &name).is_some()
            {
                continue;
            }
            if items.iter().any(|v| {
                v["name"]
                    .as_str()
                    .is_some_and(|n| local_data::same_name(n, &name))
            }) {
                continue;
            }
            items.push(json!({"name": name, "path": path, "present": true, "linked": true}));
        }
    }
    view
}

/// The folders the writable named local data adds to a folder-mode link (Cloud DEVICES §3.4 items 4 and 6): checked
/// like a linked folder (no home, drive root or device data); one that is gone or not allowed is left out. Read-only
/// entries add nothing: the folder sandbox already lets the link's servers read outside its folders (except hidden
/// places) and never write there.
fn data_folders(list: &[LocalData], data: &Path) -> Vec<PathBuf> {
    list.iter()
        .filter(|d| !d.read_only)
        .filter_map(|d| agentrouter_aggregator::check_folder(Path::new(&d.path), data).ok())
        .collect()
}

/// Named local data changes while the link runs (`agentrouter data add|remove`, checked every few seconds): the
/// cloud gets the new list, and in folder mode the writable folders join or leave the link's grants at once (a
/// removed one is outside the sandbox from that moment; the cloud stops the instances that required it,
/// `LOCAL_DATA_REVOKED`). Folders chosen on the command line stay.
async fn watch_local_data(
    console: Arc<Console>,
    agg: Aggregator,
    chosen: Vec<PathBuf>,
    mut last: Vec<LocalData>,
    folder_mode: bool,
) {
    const EVERY: Duration = Duration::from_secs(5);
    let data = data_dir();
    loop {
        tokio::time::sleep(EVERY).await;
        let now_list = Config::load().local_data;
        if now_list == last {
            continue;
        }
        if folder_mode {
            let (before, after) = (data_folders(&last, &data), data_folders(&now_list, &data));
            for gone in before
                .iter()
                .filter(|f| !after.contains(f) && !chosen.contains(f))
            {
                agg.remove_folder(gone).await;
            }
            for new in after.iter().filter(|f| !before.contains(f)) {
                if let Err(e) = agg.add_folder(new).await {
                    line(&format!(
                        "{}  命名文件夹 {} 没加上：{}",
                        now(),
                        shown(new),
                        e.message
                    ));
                }
            }
        }
        console.set_registered(now_list.clone());
        last = now_list;
    }
}

/// The folders named on the command line (or the current folder), canonical and allowed.
pub(crate) fn chosen_folders(dirs: &[String], full: bool) -> Result<Vec<PathBuf>, String> {
    let data = data_dir();
    if dirs.is_empty() {
        if full {
            let cwd = std::env::current_dir().map_err(|e| format!("看不到当前文件夹：{e}"))?;
            let real = std::fs::canonicalize(&cwd).map_err(|e| format!("看不到当前文件夹：{e}"))?;
            return Ok(vec![PathBuf::from(shown(&real))]);
        }
        return this_folder().map(|f| vec![f]);
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let real = agentrouter_aggregator::check_folder(Path::new(d), &data)?;
        if !out.contains(&real) {
            out.push(real);
        }
    }
    if out.len() > agentrouter_aggregator::MAX_FOLDERS {
        return Err(format!(
            "一次最多链接 {} 个文件夹",
            agentrouter_aggregator::MAX_FOLDERS
        ));
    }
    Ok(out)
}

/// `agentrouter link [<dir>…] [--read-only | --full] [--yes-servers]`.
pub fn link(opts: LinkOptions, interrupted: &'static AtomicBool) -> i32 {
    if opts.full && opts.read_only {
        eprintln!("--full 和 --read-only 不能一起用。");
        return 2;
    }
    let folders = match chosen_folders(&opts.dirs, opts.full) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let first = folders[0].clone();
    let name = folder_name(&first);
    let mut cfg = Config::load();
    if let Some(g) = &opts.gateway {
        cfg.gateway = g.clone();
    }
    if let Some(n) = &opts.name {
        cfg.name = n.clone();
    }
    if opts.gateway.is_some() || opts.name.is_some() {
        let _ = cfg.save();
    }
    let data = data_dir();
    // Folder mode: the writable named local data are inside the person's grants (Cloud DEVICES §3.4).
    let chosen = folders.clone();
    let folders = if opts.full {
        folders
    } else {
        let mut all = folders;
        for f in data_folders(&cfg.local_data, &data) {
            if !all.contains(&f) && all.len() < agentrouter_aggregator::MAX_FOLDERS {
                all.push(f);
            }
        }
        all
    };
    if opts.double_click {
        // start.rs printed the folder and the one access line.
    } else if opts.full {
        line("AgentRouter · 完全访问");
        line(
            "  完全访问 云端 Agent 用你的账户权限，能读写这台电脑上的所有文件（包括密钥文件）。只在你信任的对话里用；审计照记，Ctrl+C 随时断开。",
        );
        for f in &folders {
            if snapshottable(f, &data) {
                line(&format!("  检查点   {}", shown(f)));
            } else {
                line(&format!(
                    "  检查点   {} 是主目录或磁盘根目录，不做文件夹检查点；Agent 写过的文件会先留一份旧版（agentrouter undo --journal）",
                    shown(f)
                ));
            }
        }
    } else {
        line(if folders.len() > 1 {
            "AgentRouter · 链接文件夹"
        } else {
            "AgentRouter · 链接这个文件夹"
        });
        for (i, f) in folders.iter().enumerate() {
            let label = if i == 0 {
                "  文件夹   "
            } else {
                "           "
            };
            let tail = if i + 1 == folders.len() {
                format!(
                    "   云端 Agent 只能在{}里干活{}",
                    if folders.len() > 1 {
                        "这些文件夹"
                    } else {
                        "这个文件夹"
                    },
                    if opts.read_only { "（只读）" } else { "" }
                )
            } else {
                String::new()
            };
            line(&format!("{label}{}{tail}", shown(f)));
        }
    }
    for d in &cfg.local_data {
        let state = if !Path::new(&d.path).is_dir() {
            "（现在找不到这个文件夹）"
        } else if d.read_only {
            "（只读）"
        } else {
            ""
        };
        line(&format!("  命名文件夹 {} → {}{state}", d.name, d.path));
    }
    let identity = match keystore::load() {
        Some(identity) => {
            line(&if opts.double_click {
                "  确认        已经确认过，直接连上".to_string()
            } else {
                format!("  账号     这台电脑已链接（{}）", cfg.name)
            });
            identity
        }
        None => {
            let paired = link::link(&cfg, interrupted, |pairing| {
                if opts.double_click {
                    return crate::start::show_pairing(pairing);
                }
                line(&format!(
                    "  账号     第一次使用：打开 {} 输入 {}（{} 分钟内有效）",
                    pairing.verify_url,
                    pairing.code,
                    pairing.expires_in / 60
                ));
            });
            match paired.ok().and_then(|_| keystore::load()) {
                Some(identity) => {
                    line(if opts.double_click {
                        "  确认        已确认"
                    } else {
                        "  账号     已确认"
                    });
                    identity
                }
                None if opts.double_click => {
                    line(
                        "没有链接上（配对码过期或被取消）。关掉这个窗口，再双击 agentrouter.exe 试一次。",
                    );
                    return 1;
                }
                None => {
                    eprintln!("没有链接上（配对码过期或被取消）。再运行一次 agentrouter link。");
                    return 1;
                }
            }
        }
    };
    let sandbox = agentrouter_sandbox::Runtime::new(&data);
    let mut status = sandbox.status();
    if !opts.full {
        if !status.ready && status.problems.iter().all(|p| p.setup_helps()) {
            line(
                "  沙箱     第一次要准备隔离环境（下载一次，约 70 MB；Windows 会弹一次管理员确认，可能要几分钟）…",
            );
            if let Err(e) = sandbox.setup() {
                line(&format!("  沙箱     没准备好：{e}"));
            }
            status = sandbox.status();
        }
        if status.ready {
            line(&format!(
                "  沙箱     {}（{}）",
                status.mechanism, status.backend
            ));
            for n in &status.notes {
                line(&format!("  沙箱     {}。{}", n.message, n.next));
            }
        } else {
            for p in &status.problems {
                line(&format!("  沙箱     不可用：{}。{}", p.message, p.next));
            }
            line("           没有沙箱时本机工具一个也不会运行（不会在沙箱外运行）。");
        }
    } else if sandbox.runtime_bins().len() < agentrouter_sandbox::runtimes::TOOLS.len() {
        // 完全访问 runs `npx`/`uvx` tools too: the device's own Node and uv, no install for the person (#116).
        line("  运行环境 第一次要下载 AgentRouter 自己用的 Node.js 和 uv（约 70 MB）…");
        match sandbox.provision(false) {
            Ok(done) => done.iter().for_each(|d| line(&format!("  运行环境 {d}"))),
            Err(e) => line(&format!(
                "  运行环境 没准备好：{e}（要用 npx 的本机工具这次起不来）"
            )),
        }
    }

    let link_id = if opts.full {
        agentrouter_hostd_link::link_id(
            &identity.device,
            Path::new(&format!("full:{}", shown(&first))),
        )
    } else {
        agentrouter_hostd_link::link_id(&identity.device, &first)
    };
    let gateway = cfg.gateway.clone();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("起不来：{e}");
            return 1;
        }
    };
    let info = LinkInfo {
        id: link_id.clone(),
        folder: name.clone(),
        os: agentrouter_device::util::os_name().into(),
        arch: agentrouter_device::util::arch_name().into(),
        app: "cli".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        sandbox: if status.ready && !opts.full {
            status.backend.clone()
        } else {
            String::new()
        },
        read_only: opts.read_only,
        folders: folders.iter().map(|f| shown(f)).collect(),
        full: opts.full,
        placement: "pc".into(),
        capacity: {
            // The owner's ceiling and how this computer enforces limits (device #127).
            let mut c = agentrouter_device::capacity::capacity(&first, cfg.max_environments);
            let mut l = agentrouter_device::limits::view();
            l["ceiling"] = cfg.limits.sane().view();
            c["limits"] = l;
            c
        },
        labels: agentrouter_device::capacity::labels(&cfg.labels),
        pool: cfg.pool_name(),
        resources: agentrouter_device::capacity::resources(),
        software: Value::Null,
        local_data: local_data_view(&cfg.local_data, &folders),
    };
    let meter = Arc::new(agentrouter_device::capacity::LoadMeter::new(
        cfg.max_environments,
    ));
    let load_home = first.clone();
    let computer = cfg.name.clone();
    let code = runtime.block_on(async move {
        let console = Arc::new(Console {
            data: data.clone(),
            full: opts.full,
            shadows: Mutex::new(Vec::new()),
            journal: opts.full.then(|| Journal::new(&data, "full")),
            turn: Mutex::new(None),
            home: first.clone(),
            asking: Arc::new(tokio::sync::Mutex::new(())),
            no_git_said: AtomicBool::new(false),
            info: Mutex::new(info.clone()),
            registered: Mutex::new(cfg.local_data.clone()),
            handle: OnceLock::new(),
            events: OnceLock::new(),
        });
        console.set_folders(&folders);
        let handle_cell: Arc<OnceLock<agentrouter_hostd_link::Handle>> = Arc::new(OnceLock::new());
        let cell = handle_cell.clone();
        let agg = Aggregator::new(
            AggConfig {
                link: link_id.clone(),
                folders: folders.clone(),
                folder_name: name.clone(),
                read_only: opts.read_only,
                full: opts.full,
                data_dir: data.clone(),
                control: identity.control.clone(),
                yes_servers: opts.yes_servers,
                cloud: false,
                limits: cfg.limits.sane(),
                device_program: std::env::current_exe().ok(),
            },
            console.clone(),
            Arc::new(move |s: &str, m: Value| {
                if let Some(h) = cell.get() {
                    h.send(s, m);
                }
            }),
        );
        let for_head = agg.clone();
        let for_load = agg.clone();
        // Folder mode on Linux: preview streams reach the sandbox's own loopback through the host server (#136).
        #[cfg(target_os = "linux")]
        let bridge: Option<agentrouter_hostd_link::port::Bridge> = (!opts.full).then(|| {
            let agg = agg.clone();
            let bridge: agentrouter_hostd_link::port::Bridge = Arc::new(move |port: u16| {
                let agg = agg.clone();
                Box::pin(async move {
                    agg.port_bridge(port)
                        .await
                        .map(|s| Box::new(s) as Box<dyn agentrouter_hostd_link::port::Duplex>)
                })
            });
            bridge
        });
        // Windows and macOS share the computer's loopback with the sandbox: dial the port here, only when its listener
        // is one of the link's own servers' processes (#139).
        #[cfg(any(windows, target_os = "macos"))]
        let bridge: Option<agentrouter_hostd_link::port::Bridge> = (!opts.full).then(|| {
            let agg = agg.clone();
            let bridge: agentrouter_hostd_link::port::Bridge = Arc::new(move |port: u16| {
                let agg = agg.clone();
                Box::pin(async move {
                    agg.port_owner(port).await?;
                    agentrouter_hostd_link::port::connect_loopback(port)
                        .await
                        .map(|s| Box::new(s) as Box<dyn agentrouter_hostd_link::port::Duplex>)
                        .ok_or("PORT_CLOSED")
                })
            });
            bridge
        });
        #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
        let bridge = None;
        let outbox = crate::events::open_outbox(&data_dir(), &link_id, line);
        if let Some(sink) = crate::events::sink(&outbox) {
            let _ = console.events.set(sink);
        }
        let (handle, mut events) = agentrouter_hostd_link::start(agentrouter_hostd_link::Options {
            gateway: gateway.clone(),
            info,
            tokens: agentrouter_hostd_link::keystore_tokens(gateway),
            audit_head: Arc::new(move || for_head.audit_head()),
            load: Some(Arc::new(move || meter.sample(&load_home, for_load.environments()))),
            bridge,
            outbox,
        });
        let _ = handle_cell.set(handle.clone());
        let _ = console.handle.set(handle.clone());
        {
            let handle = handle.clone();
            std::thread::spawn(move || report_software(handle));
        }
        tokio::spawn(watch_local_data(
            console.clone(),
            agg.clone(),
            chosen.clone(),
            cfg.local_data.clone(),
            !opts.full,
        ));

        // The link's own SSH-like server (exec/read/write, host.rs): started here, never by the cloud.
        if console.full || status.ready {
            let agg = agg.clone();
            tokio::spawn(async move {
                let Some(exe) = std::env::current_exe().ok().map(|p| shown(&p)) else { return };
                let def = ServerDef::parse(&json!({
                    "name": "host", "title": "终端", "command": exe,
                    "args": ["mcp-serve", "host", "{{folders}}"],
                }));
                match def {
                    Ok(def) => {
                        if let Err(e) = agg.add_server(def, Duration::from_secs(30)).await {
                            line(&format!("{}  终端没起来：{}", now(), e.message));
                        }
                    }
                    Err(e) => line(&format!("{}  终端没起来：{}", now(), e.message)),
                }
            });
        }

        // The person's own tools (`agentrouter mcp add`) and the project's (`.agentrouter/mcp.json`): one by one, each
        // asked here unless granted before (device #97; `--yes-servers` grants).
        let mut defined: Vec<(&'static str, Value)> =
            agentrouter_aggregator::grants::own(&console.data).into_iter().map(|a| ("agentrouter mcp add", a)).collect();
        defined.extend(project_servers(&first).into_iter().map(|a| (".agentrouter/mcp.json", a)));
        if !defined.is_empty() {
            let agg = agg.clone();
            tokio::spawn(async move {
                for (from, args) in defined {
                    let def = match ServerDef::parse(&args) {
                        Ok(d) => d,
                        Err(e) => {
                            line(&format!("  {from} 里的 {}：{}", args["name"], e.message));
                            continue;
                        }
                    };
                    if let Err(e) = agg.add_person_server(def, Duration::from_secs(120)).await {
                        line(&format!("{}  本机工具没装上：{}", now(), e.message));
                    }
                }
            });
        }

        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        let mut was_up = false;
        let mut downs = 0u32;
        let typing = std::io::stdin().is_terminal();
        let code = loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break 1 };
                    match event {
                        Event::Up { owner, .. } => {
                            agg.link_up(owner.as_ref());
                            crate::leases::after_welcome(&agg, &handle);
                            if opts.service {
                                crate::service::record("online", "");
                            }
                            if was_up {
                                line(&format!("{}  重新连上了", now()));
                            } else if opts.double_click {
                                line(&format!("\n已连上 · 在线。网页对话里选「{computer}」就能用，这里会逐条显示 Agent 做的事。"));
                                line("要停止：关掉这个窗口（或按 Ctrl+C）。下次双击会直接连上，不用再确认。\n");
                            } else {
                                let more = if typing && !opts.full { "要再加文件夹，在这里输入 + 路径。" } else { "" };
                                line(&format!("已连上。网页对话里选「{computer}」就能用。{more}Ctrl+C 断开。"));
                            }
                            was_up = true;
                            downs = 0;
                        }
                        Event::Down { reason, retry_in } => {
                            agg.link_down();
                            if opts.service {
                                crate::service::record("offline", &reason);
                            }
                            downs += 1;
                            if downs == 1 || downs.is_multiple_of(10) {
                                line(&format!(
                                    "{}  连接断了，{} 秒后重连（{}）",
                                    now(),
                                    retry_in.as_secs().max(1),
                                    agentrouter_device::util::clip(&reason, 80)
                                ));
                            }
                        }
                        Event::Data { s, m } => agg.on_message(&s, m),
                        Event::Close { s } => agg.on_close(&s),
                        Event::LeaseLost { jobs } => crate::leases::lost(&agg, jobs, line),
                        Event::Ended(end) => {
                            if opts.service {
                                crate::service::record("ended", &match &end {
                                    End::Stopped => "已停止".to_string(),
                                    End::Revoked(_) => "这台电脑在网页上被移除了".to_string(),
                                    End::Replaced => "同样的文件夹在别的终端里又链接了".to_string(),
                                    End::Refused { code, message } => format!("网关拒绝了这个链接：{code} {message}"),
                                });
                            }
                            break match end {
                                End::Stopped => 0,
                                End::Revoked(_) => {
                                    keystore::forget();
                                    line(if opts.double_click {
                                        "这台电脑在网页上被移除了。要再用，重新双击 agentrouter.exe。"
                                    } else {
                                        "这台电脑在网页上被移除了。要再用，在文件夹里重新运行 agentrouter link。"
                                    });
                                    1
                                }
                                End::Replaced => {
                                    line("这些文件夹在别的终端里又运行了 agentrouter link，这里断开。");
                                    0
                                }
                                End::Refused { code, message } if code == "DEVICE_OUTDATED" => {
                                    line(&format!("这个版本太旧了：请运行 npx @agentrouter-top/cli@next link（{message}）"));
                                    1
                                }
                                End::Refused { code, message } => {
                                    line(&format!("网关拒绝了这个链接：{code} {message}"));
                                    1
                                }
                            };
                        }
                    }
                }
                _ = ticker.tick() => {
                    if interrupted.load(Ordering::SeqCst) {
                        handle.stop();
                        interrupted.store(false, Ordering::SeqCst);
                    }
                    // `+ <path>` typed in this terminal adds a folder (typing it is the consent). Not while a y/N
                    // question holds the terminal.
                    if typing
                        && let Ok(rx) = stdin_lines().try_lock()
                    {
                        while let Ok(typed) = rx.try_recv() {
                            let Some(path) = typed.trim().strip_prefix('+').map(str::trim) else {
                                if !typed.trim().is_empty() {
                                    line("要加文件夹就输入 + 路径（例如 + D:\\Games\\Mods）。");
                                }
                                continue;
                            };
                            if opts.full {
                                line("完全访问下不用加文件夹：整台电脑都能用。");
                                continue;
                            }
                            let path = path.trim_matches('"').to_string();
                            let agg = agg.clone();
                            tokio::spawn(async move {
                                if let Err(e) = agg.add_folder(Path::new(&path)).await {
                                    line(&format!("{}  没加上：{}", now(), e.message));
                                }
                            });
                        }
                    }
                }
            }
        };
        line("正在停止本机工具…");
        agg.shutdown().await;
        line("已断开。");
        code
    });
    runtime.shutdown_timeout(Duration::from_secs(3));
    code
}

/// `agentrouter undo [--list] [--journal] [id]`: this folder's checkpoints, or (`--journal`) 完全访问's file journal.
pub fn undo(list: bool, journal: bool, id: Option<&str>) -> i32 {
    if journal {
        let journal = Journal::new(&data_dir(), "full");
        if list {
            let turns = journal.list(30);
            if turns.is_empty() {
                line("完全访问还没有留过文件的旧版。");
            }
            for t in turns {
                line(&format!("{}  {}  {} 个文件", t.id, t.label, t.files.len()));
                for f in t.files.iter().take(5) {
                    line(&format!("    {}", shown(f)));
                }
            }
            return 0;
        }
        return match journal.undo(id) {
            Ok(t) => {
                line(&format!(
                    "已放回 {} 个文件的旧版（{}）。",
                    t.files.len(),
                    t.label
                ));
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
    }
    let folder = match this_folder() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let shadow = Shadow::new(&data_dir(), &folder_key(&folder), &folder);
    if list {
        match shadow.list(30) {
            Ok(items) if items.is_empty() => line("这个文件夹还没有检查点。"),
            Ok(items) => {
                for c in items {
                    let when = chrono::DateTime::from_timestamp(c.at, 0)
                        .map(|t| {
                            t.with_timezone(&chrono::Local)
                                .format("%m-%d %H:%M:%S")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let kind = if c.kind == "turn" {
                        "改动前"
                    } else {
                        "撤销前"
                    };
                    line(&format!("{}  {when}  {kind}  {}", c.id, c.label));
                }
            }
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
        return 0;
    }
    match shadow.undo(id) {
        Ok(done) => {
            line(&format!(
                "已撤销：文件夹回到 {}（{}）之前的样子。",
                done.restored.id, done.restored.label
            ));
            line(&format!("反悔了就运行 agentrouter undo {}", done.saved.id));
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// `agentrouter audit [verify] [n]` for this folder's link.
pub fn audit_cmd(verify_only: bool, n: usize) -> i32 {
    let folder = match this_folder() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (_, link) = match link_of(&folder) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let path = audit_path(&data_dir(), &link);
    if !verify_only {
        for e in audit::last_entries(&path, n) {
            let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let mut text = format!("{}  {}  {}", s("at"), s("kind"), s("method"));
            if !s("server").is_empty() {
                text.push_str(&format!("  {}__{}", s("server"), s("tool")));
            }
            if let Some(args) = e.get("arguments").filter(|a| !a.is_null()) {
                text.push_str(&format!(
                    "  {}",
                    agentrouter_device::util::clip(&args.to_string(), 300)
                ));
            }
            if !s("decision").is_empty() {
                text.push_str(&format!("  {}", s("decision")));
            }
            if !s("code").is_empty() {
                text.push_str(&format!(" {}", s("code")));
            }
            if let Some(ms) = e.get("ms").and_then(Value::as_u64) {
                text.push_str(&format!("  {ms}ms"));
            }
            if !s("checkpoint").is_empty() {
                text.push_str(&format!("  检查点 {}", s("checkpoint")));
            }
            line(&text);
        }
    }
    let head = audit::head(&path);
    match audit::verify(&path) {
        Ok(count) => {
            line(&format!(
                "审计日志完好，共 {count} 条（链头 {}；链头每次心跳都报给云端）。",
                head["hash"].as_str().unwrap_or("").get(..12).unwrap_or("")
            ));
            0
        }
        Err((at, why)) => {
            line(&format!("审计日志第 {at} 行有问题：{why}"));
            1
        }
    }
}

/// `agentrouter status`.
pub fn status() -> i32 {
    let cfg = Config::load();
    match keystore::load() {
        Some(identity) => line(&format!(
            "账号：这台电脑已链接（{}，{}）",
            cfg.name, identity.device
        )),
        None => line("账号：还没有链接（在项目文件夹里运行 agentrouter link）"),
    }
    line(&format!("网关：{}", cfg.gateway));
    let st = agentrouter_sandbox::Runtime::new(&data_dir()).status();
    line(&format!(
        "沙箱：{}（{}）{}",
        st.mechanism,
        st.backend,
        if st.ready { "可用" } else { "不可用" }
    ));
    for p in st.problems.iter().chain(&st.notes) {
        line(&format!("  {}。{}", p.message, p.next));
    }
    if let Ok(folder) = this_folder()
        && let Ok((_, link)) = link_of(&folder)
    {
        let path = audit_path(&data_dir(), &link);
        let head = audit::head(&path);
        line(&format!(
            "这个文件夹：{}（链接 {link}，审计 {} 条）",
            shown(&folder),
            head["seq"]
        ));
    }
    0
}

/// `agentrouter logout`: this computer forgets its link identity.
pub fn logout() -> i32 {
    if keystore::load().is_none() {
        line("这台电脑本来就没有链接。");
        return 0;
    }
    keystore::forget();
    line("已退出：这台电脑的链接身份删掉了。网页上「我的设备」里也可以把它删掉。");
    0
}

/// A QR for the terminal: two rows of modules per character line (`▀▄█`), with a quiet zone, readable by phone
/// cameras on light and dark themes. Empty when the text does not fit a QR.
pub fn terminal_qr(text: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    match qrcode::QrCode::new(text.as_bytes()) {
        Ok(code) => code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}

/// Installed software (Cloud `docs/development/DEVICES.md` §2.3): discovered at link start, again every hour and
/// whenever the person's `config.json` `software` settings change (`agentrouter software add|hide`, a source turned
/// off); only a change goes out (`{"op":"software", added, removed, changed, status}`), every later `hello` carries the list.
fn report_software(handle: agentrouter_hostd_link::Handle) {
    use agentrouter_device::software;
    const EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
    const CONFIG_CHECK: std::time::Duration = std::time::Duration::from_secs(60);
    let mut sent: Option<Vec<software::Software>> = None;
    let mut settings: Option<software::SoftwareConfig> = None;
    let mut last_run: Option<std::time::Instant> = None;
    loop {
        let current = Config::load().software;
        let due =
            last_run.is_none_or(|t| t.elapsed() >= EVERY) || settings.as_ref() != Some(&current);
        if due {
            let found = software::discover(&current);
            let before = sent.as_deref().unwrap_or(&[]);
            let delta = software::delta(before, &found.software);
            if sent.is_none() || !delta.is_empty() {
                let list = serde_json::to_value(&found.software).unwrap_or(Value::Null);
                let mut frame = serde_json::to_value(&delta).unwrap_or_else(|_| json!({}));
                // Sources that exist here but could not be read, for the device page (never an error).
                frame["status"] = json!(found.status);
                handle.set_software(list, frame);
                sent = Some(found.software);
            }
            settings = Some(current);
            last_run = Some(std::time::Instant::now());
        }
        std::thread::sleep(CONFIG_CHECK);
    }
}
