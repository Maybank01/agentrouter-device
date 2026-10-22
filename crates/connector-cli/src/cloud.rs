//! `hostd` in a cloud sandbox (docs/HOSTD-MCP.md §13.1, issue #82): `agentrouter link --placement cloud <folder>…`,
//! started by the sandbox's entrypoint. Same link, frames, signing, `host__*` tools and audit as on a PC; only the
//! policy differs:
//!
//! - **No pairing, no device key.** The platform issues a host token bound to (sandbox, session tree) at create and
//!   revokes it at destroy; it is the `hello` token. `AGENTROUTER_HOST_TOKEN_FILE` (re-read before every connection,
//!   so the platform can rotate it) or `AGENTROUTER_HOST_TOKEN` (removed from this process's environment at start, so
//!   no server or command the Agent runs inherits it).
//! - **The control plane's key** comes from `AGENTROUTER_CONTROL_KEY` (raw Ed25519 public key, base64url, or the
//!   `{"kid","publicKey"}` JSON), since there is no pairing answer to pin it from.
//! - **The sandbox is the boundary:** servers run without `srt` (like 完全访问), no checkpoints or journal (the project
//!   service's per-turn changesets are the undo), nobody is asked anything (custom servers and `link__folder_add` are
//!   refused), the folders are the session's mounted views named at start.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agentrouter_aggregator::servers::ServerDef;
use agentrouter_aggregator::{Aggregator, Config as AggConfig, Feed, Hooks, Outcome};
use agentrouter_device::config::Config;
use agentrouter_device::protocol::{ControlKey, sha256_hex};
use agentrouter_device::util::{b64url_decode, data_dir};
use agentrouter_hostd_link::{End, Event, LinkInfo};
use serde_json::{Value, json};

pub const ENV_TOKEN: &str = "AGENTROUTER_HOST_TOKEN";
pub const ENV_TOKEN_FILE: &str = "AGENTROUTER_HOST_TOKEN_FILE";
pub const ENV_CONTROL_KEY: &str = "AGENTROUTER_CONTROL_KEY";
pub const ENV_GATEWAY: &str = "AGENTROUTER_GATEWAY";
/// The sandbox id (part of the stable link id); the host name when unset.
pub const ENV_HOST_ID: &str = "AGENTROUTER_HOST_ID";

pub struct CloudOptions {
    pub dirs: Vec<String>,
    pub gateway: Option<String>,
    pub token_file: Option<String>,
}

fn line(text: &str) {
    let t = chrono::Local::now().format("%H:%M:%S");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{t}  {text}");
    let _ = out.flush();
}

/// The control plane's signing key from `AGENTROUTER_CONTROL_KEY`: a raw public key (the kid is derived) or the JSON
/// the pairing answer carries.
pub fn parse_control_key(text: &str) -> Result<ControlKey, String> {
    let text = text.trim();
    let key = if text.starts_with('{') {
        serde_json::from_str::<ControlKey>(text)
            .map_err(|e| format!("{ENV_CONTROL_KEY} 不是有效的 JSON：{e}"))?
    } else {
        let raw = b64url_decode(text)
            .filter(|r| r.len() == 32)
            .ok_or(format!(
                "{ENV_CONTROL_KEY} 要是 32 字节的 Ed25519 公钥（base64url）"
            ))?;
        ControlKey {
            kid: sha256_hex(&raw)[..16].to_string(),
            public_key: text.to_string(),
        }
    };
    if !key.is_consistent() {
        return Err(format!("{ENV_CONTROL_KEY} 的 kid 和公钥对不上"));
    }
    Ok(key)
}

/// The mounted folders: real, existing directories, never the file-system root; the first is home.
fn folders(dirs: &[String]) -> Result<Vec<PathBuf>, String> {
    if dirs.is_empty() {
        return Err(
            "云端模式要写出挂载的文件夹：agentrouter link --placement cloud /workspace/…".into(),
        );
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let real = std::fs::canonicalize(d).map_err(|e| format!("看不到 {d}：{e}"))?;
        if !real.is_dir() || real.parent().is_none() {
            return Err(format!(
                "{} 不能作为文件夹（不是目录或是根目录）",
                real.display()
            ));
        }
        if !out.contains(&real) {
            out.push(real);
        }
    }
    if out.len() > agentrouter_aggregator::MAX_FOLDERS {
        return Err(format!(
            "最多 {} 个文件夹",
            agentrouter_aggregator::MAX_FOLDERS
        ));
    }
    Ok(out)
}

fn host_id() -> String {
    if let Ok(id) = std::env::var(ENV_HOST_ID)
        && !id.trim().is_empty()
    {
        return id.trim().to_string();
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "sandbox".into())
}

/// The sandbox's log: one line per call and server event; no questions.
struct CloudHooks {
    /// The host's outbox, where feeds write (Cloud #2739).
    events: Option<Arc<dyn agentrouter_aggregator::feeds::EventSink>>,
}

impl Hooks for CloudHooks {
    fn events(&self) -> Option<Arc<dyn agentrouter_aggregator::feeds::EventSink>> {
        self.events.clone()
    }

    fn feed(&self, event: Feed) {
        match event {
            Feed::Session {
                title,
                opened: true,
            } => line(&format!("session opened {title}")),
            Feed::Session { .. } => {}
            Feed::Call {
                server_title,
                tool,
                outcome,
                ms,
                ..
            } => {
                let result = match outcome {
                    Outcome::Ok => "ok".to_string(),
                    Outcome::ToolError(text) => {
                        format!("error {}", agentrouter_device::util::clip(&text, 80))
                    }
                    Outcome::Denied(code) => format!("denied {code}"),
                };
                line(&format!("call {server_title} {tool} {result} {ms}ms"));
            }
            Feed::Server {
                name,
                event,
                detail,
                ..
            } => line(&format!(
                "server {name} {event} {}",
                detail
                    .map(|d| agentrouter_device::util::clip(&d, 120))
                    .unwrap_or_default()
            )),
            // The address is a short-lived login token: never in a cloud host's log.
            Feed::Login { name, .. } => line(&format!("server {name} login_started")),
        }
    }
}

/// `agentrouter link --placement cloud <folder>…`.
pub fn link(opts: CloudOptions, interrupted: &'static AtomicBool) -> i32 {
    let folders = match folders(&opts.dirs) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let control = match std::env::var(ENV_CONTROL_KEY) {
        Ok(text) => match parse_control_key(&text) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        },
        Err(_) => {
            eprintln!("云端模式要 {ENV_CONTROL_KEY}（平台的签名公钥）");
            return 2;
        }
    };
    let token_file = opts
        .token_file
        .clone()
        .or_else(|| std::env::var(ENV_TOKEN_FILE).ok())
        .filter(|f| !f.trim().is_empty())
        .map(PathBuf::from);
    let token_value = std::env::var(ENV_TOKEN)
        .ok()
        .filter(|t| !t.trim().is_empty());
    // SAFETY: called before the runtime and any server start; the only other thread (the Ctrl+C handler) never reads
    // the environment. Children (servers, the Agent's commands) must not inherit the host token.
    unsafe { std::env::remove_var(ENV_TOKEN) };
    if token_file.is_none() && token_value.is_none() {
        eprintln!("云端模式要主机令牌：{ENV_TOKEN_FILE} 或 {ENV_TOKEN}");
        return 2;
    }
    let gateway = opts
        .gateway
        .clone()
        .or_else(|| std::env::var(ENV_GATEWAY).ok())
        .filter(|g| !g.trim().is_empty())
        .unwrap_or_else(|| Config::load().gateway);
    if let Err(e) = agentrouter_device::net::gateway_allowed(&gateway) {
        eprintln!("{e}");
        return 2;
    }

    let first = folders[0].clone();
    let name = first
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.display().to_string());
    let link_id = agentrouter_hostd_link::link_id(&format!("cloud:{}", host_id()), &first);
    let data = data_dir();
    // Before the aggregator takes `data` and the link id: the host's outbox (Cloud #2739).
    let outbox = crate::events::open_outbox(&data, &link_id, line);
    let shown: Vec<String> = folders.iter().map(|f| f.display().to_string()).collect();
    line(&format!(
        "hostd (cloud) {} · {} · {gateway}",
        env!("CARGO_PKG_VERSION"),
        shown.join(", ")
    ));
    let info = LinkInfo {
        id: link_id.clone(),
        folder: name.clone(),
        os: agentrouter_device::util::os_name().into(),
        arch: agentrouter_device::util::arch_name().into(),
        app: "hostd".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        sandbox: String::new(),
        read_only: false,
        folders: shown,
        full: true,
        placement: "cloud".into(),
        capacity: agentrouter_device::capacity::capacity(&first, None),
        labels: agentrouter_device::capacity::labels(&[]),
        pool: None,
    };
    let meter = Arc::new(agentrouter_device::capacity::LoadMeter::new(None));
    let load_home = first.clone();
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
    let code = runtime.block_on(async move {
        let handle_cell: Arc<std::sync::OnceLock<agentrouter_hostd_link::Handle>> =
            Arc::new(std::sync::OnceLock::new());
        let cell = handle_cell.clone();
        let agg = Aggregator::new(
            AggConfig {
                link: link_id,
                folders,
                folder_name: name,
                read_only: false,
                full: true,
                data_dir: data,
                control,
                yes_servers: false,
                cloud: true,
                limits: Default::default(),
                device_program: std::env::current_exe().ok(),
            },
            Arc::new(CloudHooks {
                events: crate::events::sink(&outbox),
            }),
            Arc::new(move |s: &str, m: Value| {
                if let Some(h) = cell.get() {
                    h.send(s, m);
                }
            }),
        );
        let for_head = agg.clone();
        let for_load = agg.clone();
        let (handle, mut events) = agentrouter_hostd_link::start(agentrouter_hostd_link::Options {
            gateway,
            info,
            tokens: agentrouter_hostd_link::host_token(token_file, token_value),
            audit_head: Arc::new(move || for_head.audit_head()),
            load: Some(Arc::new(move || meter.sample(&load_home, for_load.environments()))),
            // A cloud sandbox serves its loopback directly (the sandbox is the boundary).
            bridge: None,
            outbox: outbox.clone(),
        });
        let _ = handle_cell.set(handle.clone());

        // The `host` server (exec, files, search, jobs, PTY, info, changes): always, unconfined.
        {
            let agg = agg.clone();
            tokio::spawn(async move {
                let Some(exe) = std::env::current_exe().ok() else {
                    line("host server not started: no executable path");
                    return;
                };
                let def = ServerDef::parse(&json!({
                    "name": "host", "title": "终端", "command": exe.display().to_string(),
                    "args": ["mcp-serve", "host", "{{folders}}"],
                }));
                let started = match def {
                    Ok(def) => agg.add_server(def, Duration::from_secs(30)).await.map(|_| ()),
                    Err(e) => Err(e),
                };
                if let Err(e) = started {
                    line(&format!("host server not started: {}", e.message));
                }
            });
        }

        let mut ticker = tokio::time::interval(Duration::from_millis(200));
        let mut downs = 0u32;
        let code = loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break 1 };
                    match event {
                        Event::Up { link } => {
                            agg.link_up();
                            crate::leases::after_welcome(&agg, &handle);
                            downs = 0;
                            line(&format!("link up {link}"));
                        }
                        Event::Down { reason, retry_in } => {
                            agg.link_down();
                            downs += 1;
                            if downs == 1 || downs.is_multiple_of(10) {
                                line(&format!("link down, retry in {}s ({})", retry_in.as_secs().max(1), agentrouter_device::util::clip(&reason, 120)));
                            }
                        }
                        Event::Data { s, m } => agg.on_message(&s, m),
                        Event::Close { s } => agg.on_close(&s),
                        Event::LeaseLost { jobs } => crate::leases::lost(&agg, jobs, line),
                        Event::Ended(end) => {
                            break match end {
                                End::Stopped => 0,
                                End::Replaced => {
                                    line("replaced by another hostd for this sandbox");
                                    0
                                }
                                End::Revoked(why) => {
                                    line(&format!("host token revoked: {why}"));
                                    1
                                }
                                End::Refused { code, message } => {
                                    line(&format!("gateway refused: {code} {message}"));
                                    1
                                }
                            };
                        }
                    }
                }
                _ = ticker.tick() => {
                    if interrupted.swap(false, Ordering::SeqCst) {
                        handle.stop();
                    }
                }
            }
        };
        agg.shutdown().await;
        line("stopped");
        code
    });
    runtime.shutdown_timeout(Duration::from_secs(3));
    code
}

#[cfg(test)]
mod tests {
    use super::parse_control_key;

    #[test]
    fn control_key_raw_or_json() {
        let key = agentrouter_device::protocol::DeviceKey::from_seed([7; 32]).public_key();
        let raw = parse_control_key(&key).expect("raw key");
        assert_eq!(raw.public_key, key);
        assert!(raw.is_consistent());
        let json = format!(r#"{{"kid":"{}","publicKey":"{}"}}"#, raw.kid, key);
        assert_eq!(parse_control_key(&json).unwrap().kid, raw.kid);
        assert!(parse_control_key(r#"{"kid":"0000000000000000","publicKey":"AAAA"}"#).is_err());
        assert!(parse_control_key("not-a-key").is_err());
    }

    #[test]
    fn folders_must_be_named() {
        assert!(super::folders(&[]).is_err());
        assert!(super::folders(&["/".into()]).is_err());
    }
}
