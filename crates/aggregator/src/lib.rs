//! The MCP aggregator (docs/LINKER-MCP.md §2, §3.3–§3.5, §5, §7; issue #46): everything the cloud Agent can do in
//! the linked folder comes from local MCP servers, and this crate makes them **one MCP endpoint** towards the link.
//!
//! - Towards the link: one MCP server session per `s` (official Rust SDK `rmcp`, over the Linker's `{s, m}` frames).
//!   `initialize` binds the session to a conversation; it and every `tools/call` must carry the control plane's
//!   signature v3 in `params._meta` ([`sig`]); `tools/list`, `ping` and notifications need none.
//! - Towards the servers: an `rmcp` client per server, each server process started by the sandbox launcher (the OS
//!   confines it to the folder; no sandbox = it does not run). Tools are exposed as `<server>__<tool>` with their
//!   annotations; a server's `list_changed` becomes ours; cancellation and progress pass through.
//! - Reserved management tools `link__servers`, `link__server_add`, `link__server_remove`, `link__server_logs`,
//!   `link__package_put` decide *which* servers run; they are not capabilities. A server that is not a pinned
//!   catalog preset needs the person's `y` on this computer ([`Hooks::confirm_server`]).
//! - Every request and every answer is one line in the append-only, hash-chained audit with its arguments.
//! - Before the first call of a turn to a tool without `readOnlyHint: true`, [`Hooks::checkpoint`] snapshots the
//!   folder (shadow git outside it, #47).

pub mod servers;
pub mod sig;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientJsonRpcMessage,
    ClientRequest, ErrorCode, ErrorData, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ProgressToken, ServerCapabilities,
    ServerJsonRpcMessage, ServerResult, Tool,
};
use rmcp::service::{
    NotificationContext, Peer, PeerRequestOptions, RequestContext, RoleServer, RxJsonRpcMessage,
    TxJsonRpcMessage,
};
use rmcp::transport::Transport;
use rmcp::{ServerHandler, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use agentrouter_device::audit::{self, Audit};
use agentrouter_device::mcp_pkg::Packages;
use agentrouter_device::protocol::{ControlKey, DeviceError, ReplayCache, sha256_hex};

use servers::{Server, ServerDef, State};

/// JSON-RPC error code of every refusal of ours; `data = {code, next}`.
pub const ERROR_CODE: i32 = -32001;
/// A session survives a link drop this long (the same `s` resumes).
pub const SESSION_GRACE: Duration = Duration::from_secs(120);
/// Results stay under the 8 MiB frame with room for the envelope.
pub const RESULT_MAX: usize = 7 << 20;
const CALL_DEFAULT: Duration = Duration::from_secs(600);
const CALL_MAX: Duration = Duration::from_secs(1800);
/// Without a turn id from the cloud, calls this far apart are different turns (a new checkpoint).
const TURN_GAP: Duration = Duration::from_secs(60);
/// `_meta` keys the control plane may add (unsigned, display and pacing only).
pub const META_TITLE: &str = "agentrouter.top/title";
pub const META_TURN: &str = "agentrouter.top/turn";
pub const META_TIMEOUT: &str = "agentrouter.top/timeoutMs";

/// A refusal: `data.code`, a sentence, and what the model should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub next: Option<String>,
}

impl Failure {
    pub fn new(code: &'static str, message: impl Into<String>) -> Failure {
        Failure {
            code,
            message: message.into(),
            next: None,
        }
    }
    pub fn next(mut self, next: impl Into<String>) -> Failure {
        self.next = Some(next.into());
        self
    }
    fn to_error(&self) -> ErrorData {
        ErrorData::new(
            ErrorCode(ERROR_CODE),
            self.message.clone(),
            Some(json!({"code": self.code, "next": self.next})),
        )
    }
}

impl From<DeviceError> for Failure {
    fn from(e: DeviceError) -> Failure {
        Failure {
            code: e.code,
            message: e.message,
            next: e.next,
        }
    }
}

/// One line of the person's live feed (§8).
#[derive(Debug, Clone, PartialEq)]
pub enum Feed {
    Session {
        title: String,
        opened: bool,
    },
    Call {
        title: String,
        server: String,
        /// The server's display title (`文件`, `Git`, …).
        server_title: String,
        tool: String,
        /// A short hint of the arguments (a path, a command).
        hint: String,
        outcome: Outcome,
        ms: u64,
    },
    Server {
        name: String,
        title: String,
        event: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The tool answered `isError` (the first words of its text): an OS refusal outside the folder lands here.
    ToolError(String),
    Denied(String),
}

/// Whether a tool's error text is the OS refusing a path (the sandbox boundary).
pub fn is_boundary_refusal(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "eacces",
        "eperm",
        "erofs",
        "permission denied",
        "access is denied",
        "read-only file system",
        "operation not permitted",
        "outside allowed directories",
        "access denied",
    ]
    .iter()
    .any(|p| t.contains(p))
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the console (or a test) does for the aggregator.
pub trait Hooks: Send + Sync + 'static {
    /// One line for the person's terminal.
    fn feed(&self, _event: Feed) {}
    /// A server that is not a pinned catalog preset: ask the person on this computer (y/N, default no).
    fn confirm_server(&self, _def: &ServerDef) -> BoxFuture<bool> {
        Box::pin(async { false })
    }
    /// Snapshot the folder before the first non-read-only call of a turn; returns the checkpoint id.
    fn checkpoint(&self, _label: &str) -> Option<String> {
        None
    }
}

/// Sends one MCP message of session `s` to the link.
pub type Outbound = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct Config {
    /// `lnk_…`
    pub link: String,
    /// The canonical folder (the only writable place for the servers).
    pub folder: PathBuf,
    /// Its display name.
    pub folder_name: String,
    pub read_only: bool,
    /// The device data folder (packages, audit; hidden from every sandbox).
    pub data_dir: PathBuf,
    /// The control plane's pinned signing key.
    pub control: ControlKey,
    /// `--yes-servers`: custom servers are accepted without asking (a local flag only).
    pub yes_servers: bool,
}

/// The audit log of one link.
pub fn audit_path(data_dir: &std::path::Path, link: &str) -> PathBuf {
    data_dir.join("audit").join(format!("link-{link}.jsonl"))
}

struct Pending {
    at: tokio::time::Instant,
    method: String,
    server: String,
    tool: String,
    hint: String,
    checkpoint: Option<String>,
}

struct Session {
    conversation: String,
    title: String,
    tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleServer>>,
    ct: CancellationToken,
    peer: Mutex<Option<Peer<RoleServer>>>,
    pending: Mutex<HashMap<String, Pending>>,
    last_write: Mutex<Option<(String, tokio::time::Instant)>>,
}

/// A downstream progress token of one server.
type ProgressKey = (String, ProgressToken);

pub struct Inner {
    pub cfg: Config,
    pub sandbox: agentrouter_sandbox::Runtime,
    pub packages: Packages,
    audit: Mutex<Audit>,
    hooks: Arc<dyn Hooks>,
    out: Outbound,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    servers: Mutex<BTreeMap<String, Arc<Server>>>,
    replay: Mutex<ReplayCache>,
    progress: Mutex<HashMap<ProgressKey, (Peer<RoleServer>, ProgressToken)>>,
    down_since: Mutex<Option<tokio::time::Instant>>,
    closed: CancellationToken,
}

/// The aggregator of one running link.
#[derive(Clone)]
pub struct Aggregator {
    inner: Arc<Inner>,
}

fn now_ms() -> i64 {
    agentrouter_device::util::now_ms()
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

/// `server__tool` → (`server`, `tool`).
pub fn split_tool(name: &str) -> Option<(&str, &str)> {
    name.split_once("__")
        .filter(|(s, t)| !s.is_empty() && !t.is_empty())
}

/// A short hint of a call's arguments for the feed (a path, a command, a query).
pub fn hint_of(arguments: &Value) -> String {
    for key in [
        "path",
        "file",
        "file_path",
        "command",
        "cmd",
        "query",
        "pattern",
        "url",
        "name",
    ] {
        if let Some(v) = arguments.get(key).and_then(Value::as_str) {
            return agentrouter_device::util::clip(&v.replace(['\n', '\r'], " "), 60);
        }
    }
    if let Some(v) = arguments.get("paths").and_then(Value::as_array)
        && let Some(first) = v.first().and_then(Value::as_str)
    {
        return agentrouter_device::util::clip(first, 60);
    }
    String::new()
}

impl Aggregator {
    /// Start the aggregator of one link (inside a tokio runtime). `out` sends to the link.
    pub fn new(cfg: Config, hooks: Arc<dyn Hooks>, out: Outbound) -> Aggregator {
        let audit = Audit::open(&audit_path(&cfg.data_dir, &cfg.link));
        let inner = Arc::new(Inner {
            sandbox: agentrouter_sandbox::Runtime::new(&cfg.data_dir),
            packages: Packages::new(cfg.data_dir.join("mcp-packages")),
            cfg,
            audit: Mutex::new(audit),
            hooks,
            out,
            sessions: Mutex::new(HashMap::new()),
            servers: Mutex::new(BTreeMap::new()),
            replay: Mutex::new(ReplayCache::default()),
            progress: Mutex::new(HashMap::new()),
            down_since: Mutex::new(None),
            closed: CancellationToken::new(),
        });
        let janitor = Arc::downgrade(&inner);
        let closed = inner.closed.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = closed.cancelled() => break,
                }
                let Some(inner) = janitor.upgrade() else {
                    break;
                };
                let expired = inner
                    .down_since
                    .lock()
                    .unwrap()
                    .is_some_and(|t| t.elapsed() > SESSION_GRACE);
                if expired {
                    let all: Vec<String> = inner.sessions.lock().unwrap().keys().cloned().collect();
                    for s in all {
                        inner.end_session(&s, "link lost");
                    }
                }
            }
        });
        inner.record(json!({"kind": "link", "event": "start", "link": inner.cfg.link, "folder": inner.cfg.folder_name, "readOnly": inner.cfg.read_only, "sandbox": agentrouter_sandbox::Runtime::backend()}));
        Aggregator { inner }
    }

    /// The audit chain head for the heartbeat.
    pub fn audit_head(&self) -> Value {
        self.inner.audit.lock().unwrap().head()
    }

    /// One MCP message of session `s` from the link.
    pub fn on_message(&self, s: &str, m: Value) {
        self.inner.on_message(s, m);
    }

    /// The cloud ended session `s`.
    pub fn on_close(&self, s: &str) {
        self.inner.end_session(s, "closed by the cloud");
    }

    pub fn link_down(&self) {
        let mut d = self.inner.down_since.lock().unwrap();
        if d.is_none() {
            *d = Some(tokio::time::Instant::now());
        }
    }

    pub fn link_up(&self) {
        *self.inner.down_since.lock().unwrap() = None;
    }

    /// The servers and the sandbox, as `link__servers` shows them.
    pub fn servers_view(&self) -> Value {
        self.inner.servers_view()
    }

    /// Add a server from this computer (not from the cloud): the console's own default set.
    pub async fn add_server(&self, def: ServerDef, wait: Duration) -> Result<Value, Failure> {
        Inner::add(&self.inner, def, wait, true).await
    }

    /// Stop every server and end every session (Ctrl+C).
    pub async fn shutdown(&self) {
        let sessions: Vec<String> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for s in sessions {
            self.inner.end_session(&s, "link stopped");
        }
        let servers: Vec<Arc<Server>> = self
            .inner
            .servers
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for s in &servers {
            s.stop.cancel();
        }
        for s in &servers {
            s.wait_settled(Duration::from_secs(1)).await;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while s.state() != State::Stopped
                && !matches!(s.state(), State::Crashed(_) | State::Unavailable(_))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        self.inner
            .record(json!({"kind": "link", "event": "stop", "link": self.inner.cfg.link}));
        self.inner.closed.cancel();
    }
}

impl Inner {
    fn record(&self, entry: Value) {
        self.audit.lock().unwrap().record(entry);
    }

    pub(crate) fn folder_shown(&self) -> String {
        self.cfg.folder.to_string_lossy().into_owned()
    }

    fn send(&self, s: &str, m: Value) {
        (self.out)(s, m);
    }

    fn reply_error(&self, s: &str, id: &Value, failure: &Failure) {
        self.send(
            s,
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": ERROR_CODE, "message": failure.message, "data": {"code": failure.code, "next": failure.next}}}),
        );
    }

    fn session(&self, s: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(s).cloned()
    }

    fn end_session(&self, s: &str, why: &str) {
        let Some(session) = self.sessions.lock().unwrap().remove(s) else {
            return;
        };
        session.ct.cancel();
        self.record(json!({"kind": "session", "event": "end", "link": self.cfg.link, "mcp": s, "session": session.conversation, "why": why}));
        self.hooks.feed(Feed::Session {
            title: session.title.clone(),
            opened: false,
        });
    }

    fn on_message(self: &Arc<Self>, s: &str, m: Value) {
        if s.is_empty() || s.len() > 200 {
            return;
        }
        let method = m.get("method").and_then(Value::as_str).map(str::to_string);
        let id = m.get("id").cloned().filter(|v| !v.is_null());
        let empty = json!({});
        let params = m.get("params").unwrap_or(&empty);
        if let (Some(method), Some(id)) = (&method, &id) {
            // A request: signature first where one is required.
            let mut signed = None;
            if sig::must_be_signed(method) {
                let checked = {
                    let mut replay = self.replay.lock().unwrap();
                    sig::check(
                        method,
                        params,
                        &self.cfg.link,
                        &self.cfg.control,
                        now_ms(),
                        &mut replay,
                    )
                };
                match checked {
                    Ok(ok) => signed = Some(ok),
                    Err(refused) => {
                        let failure = Failure::new(refused.code, refused.message).next(
                            "这个请求没有通过这台电脑的签名检查；不要原样重试，重新打开这个链接的会话。",
                        );
                        self.deny(s, id, method, params, None, &failure);
                        return;
                    }
                }
            }
            if method == "initialize" {
                let signed = signed.clone().expect("initialize is signed");
                self.open_session(s, &signed.session, params);
            }
            let Some(session) = self.session(s) else {
                let failure = Failure::new(
                    "SESSION_UNKNOWN",
                    "这个会话不在这台电脑上（可能链接断开太久）",
                )
                .next("重新建立这个链接的 MCP 会话（initialize）。");
                self.deny(s, id, method, params, None, &failure);
                return;
            };
            if let Some(signed) = &signed
                && signed.session != session.conversation
            {
                let failure = Failure::new("SIGNATURE_INVALID", "这个请求是签给另一个对话的");
                self.deny(s, id, method, params, Some(&session), &failure);
                return;
            }
            let (server, tool, hint, arguments) = if method == "tools/call" {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let (server, tool) = split_tool(name).unwrap_or(("", name));
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                (
                    server.to_string(),
                    tool.to_string(),
                    hint_of(&arguments),
                    Some(arguments),
                )
            } else {
                (String::new(), String::new(), String::new(), None)
            };
            let mut entry = json!({
                "kind": "request", "link": self.cfg.link, "mcp": s, "session": session.conversation,
                "id": id, "method": method, "decision": "allowed",
            });
            if let Some(signed) = &signed {
                entry["nonce"] = json!(signed.nonce);
            }
            if let Some(arguments) = &arguments {
                entry["server"] = json!(server);
                entry["tool"] = json!(tool);
                entry["arguments"] = audit::redact_args(&self.redacted_for(&server, arguments));
            }
            self.record(entry);
            session.pending.lock().unwrap().insert(
                id_key(id),
                Pending {
                    at: tokio::time::Instant::now(),
                    method: method.clone(),
                    server,
                    tool,
                    hint,
                    checkpoint: None,
                },
            );
            self.forward(s, &session, m);
            return;
        }
        // Notifications and answers to our own requests: only within a known session.
        if let Some(session) = self.session(s) {
            self.forward(s, &session, m);
        }
    }

    /// Arguments with the server's secret values blanked (before the generic redaction).
    fn redacted_for(&self, server: &str, arguments: &Value) -> Value {
        let mut a = arguments.clone();
        if let Some(srv) = self.servers.lock().unwrap().get(server) {
            servers::redact_value(&mut a, &srv.def.secrets());
        }
        a
    }

    fn deny(
        &self,
        s: &str,
        id: &Value,
        method: &str,
        params: &Value,
        session: Option<&Session>,
        failure: &Failure,
    ) {
        let mut entry = json!({
            "kind": "request", "link": self.cfg.link, "mcp": s, "id": id, "method": method,
            "decision": "denied", "code": failure.code,
        });
        if let Some(session) = session {
            entry["session"] = json!(session.conversation);
        }
        if method == "tools/call" {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let (server, tool) = split_tool(name).unwrap_or(("", name));
            entry["server"] = json!(server);
            entry["tool"] = json!(tool);
            entry["arguments"] =
                audit::redact_args(params.get("arguments").unwrap_or(&Value::Null));
            self.hooks.feed(Feed::Call {
                title: session.map(|s| s.title.clone()).unwrap_or_default(),
                server: server.to_string(),
                server_title: self.title_of(server),
                tool: tool.to_string(),
                hint: hint_of(params.get("arguments").unwrap_or(&Value::Null)),
                outcome: Outcome::Denied(failure.code.to_string()),
                ms: 0,
            });
        }
        self.record(entry);
        self.reply_error(s, id, failure);
    }

    fn forward(&self, s: &str, session: &Session, m: Value) {
        match serde_json::from_value::<ClientJsonRpcMessage>(m.clone()) {
            Ok(message) => {
                let _ = session.tx.send(message);
            }
            Err(e) => {
                if let Some(id) = m.get("id").filter(|v| !v.is_null()) {
                    session.pending.lock().unwrap().remove(&id_key(id));
                    self.send(
                        s,
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": format!("invalid request: {e}")}}),
                    );
                }
            }
        }
    }

    fn open_session(self: &Arc<Self>, s: &str, conversation: &str, params: &Value) {
        if let Some(old) = self.session(s) {
            if old.conversation == conversation {
                // A re-initialize of the same conversation (after a reconnect): a fresh MCP session.
                self.end_session(s, "re-initialized");
            } else {
                self.end_session(s, "taken over by another conversation");
            }
        }
        let title = params
            .get("_meta")
            .and_then(|m| m.get(META_TITLE))
            .and_then(Value::as_str)
            .map(|t| agentrouter_device::util::clip(t, 24))
            .unwrap_or_default();
        let (tx, rx) = mpsc::unbounded_channel();
        let ct = self.closed.child_token();
        let session = Arc::new(Session {
            conversation: conversation.to_string(),
            title: title.clone(),
            tx,
            ct: ct.clone(),
            peer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            last_write: Mutex::new(None),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(s.to_string(), session.clone());
        self.record(json!({"kind": "session", "event": "open", "link": self.cfg.link, "mcp": s, "session": conversation, "title": title}));
        self.hooks.feed(Feed::Session {
            title,
            opened: true,
        });
        let transport = LinkTransport {
            rx,
            s: s.to_string(),
            inner: Arc::downgrade(self),
        };
        let handler = SessionHandler {
            inner: Arc::downgrade(self),
            s: s.to_string(),
        };
        tokio::spawn(async move {
            if let Ok(running) = handler.serve_with_ct(transport, ct).await {
                let _ = running.waiting().await;
            }
        });
    }

    /// An answer leaves for the link: audit it (with the checkpoint taken for it), then send.
    fn on_outgoing(&self, s: &str, message: Value) {
        if let Some(id) = message.get("id").filter(|v| !v.is_null())
            && message.get("method").is_none()
            && let Some(session) = self.session(s)
            && let Some(p) = session.pending.lock().unwrap().remove(&id_key(id))
        {
            let ms = p.at.elapsed().as_millis() as u64;
            let mut entry = json!({
                "kind": "answer", "link": self.cfg.link, "mcp": s, "session": session.conversation,
                "id": id, "method": p.method, "ms": ms,
            });
            let outcome = if let Some(error) = message.get("error") {
                entry["error"] = json!({"code": error.get("code"), "data": error.get("data").and_then(|d| d.get("code"))});
                Outcome::Denied(
                    error
                        .get("data")
                        .and_then(|d| d.get("code"))
                        .and_then(Value::as_str)
                        .unwrap_or("ERROR")
                        .to_string(),
                )
            } else {
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                entry["result"] = audit::result_summary(&result);
                if result.get("isError").and_then(Value::as_bool) == Some(true) {
                    let text = result
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|c| c.iter().find_map(|i| i.get("text").and_then(Value::as_str)))
                        .unwrap_or("");
                    Outcome::ToolError(agentrouter_device::util::clip(text, 200))
                } else {
                    Outcome::Ok
                }
            };
            if let Some(cp) = &p.checkpoint {
                entry["checkpoint"] = json!(cp);
            }
            if !p.server.is_empty() {
                entry["server"] = json!(p.server);
                entry["tool"] = json!(p.tool);
            }
            self.record(entry);
            if p.method == "tools/call" && p.server != "link" {
                self.hooks.feed(Feed::Call {
                    title: session.title.clone(),
                    server_title: self.title_of(&p.server),
                    server: p.server,
                    tool: p.tool,
                    hint: p.hint,
                    outcome,
                    ms,
                });
            }
        }
        self.send(s, message);
    }

    fn title_of(&self, server: &str) -> String {
        self.servers
            .lock()
            .unwrap()
            .get(server)
            .map(|s| s.def.title.clone())
            .unwrap_or_else(|| server.to_string())
    }

    fn servers_view(&self) -> Value {
        let status = self.sandbox.status();
        let list: Vec<Value> = self
            .servers
            .lock()
            .unwrap()
            .values()
            .map(|s| s.view())
            .collect();
        json!({
            "folder": self.cfg.folder_name,
            "readOnly": self.cfg.read_only,
            "sandbox": {"backend": status.backend, "mechanism": status.mechanism, "ready": status.ready,
                        "problems": status.problems.iter().map(|p| json!({"message": p.message, "next": p.next})).collect::<Vec<_>>()},
            "servers": list,
        })
    }

    pub(crate) fn server_event(&self, server: &Server, event: &str, detail: Option<String>) {
        self.record(json!({"kind": "server", "event": event, "link": self.cfg.link, "name": server.def.name, "detail": detail}));
        self.hooks.feed(Feed::Server {
            name: server.def.name.clone(),
            title: server.def.title.clone(),
            event: event.to_string(),
            detail,
        });
    }

    pub(crate) fn audit_server_start(
        &self,
        def: &ServerDef,
        program: &std::path::Path,
        args: &[String],
        policy: &Value,
    ) {
        let mut line: Vec<String> = vec![program.to_string_lossy().into_owned()];
        line.extend(args.iter().cloned());
        let mut command = Value::from(line);
        servers::redact_value(&mut command, &def.secrets());
        self.record(json!({
            "kind": "server", "event": "start", "link": self.cfg.link, "name": def.name,
            "preset": def.preset, "command": command,
            "env": def.env.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            "policy": sha256_hex(agentrouter_device::protocol::canonical_json(policy).as_bytes()),
        }));
    }

    /// Every tool the link offers now: the management tools, then each ready server's tools as `<server>__<tool>`.
    fn tool_list(&self) -> Vec<Tool> {
        let mut tools = link_tools();
        for (name, server) in self.servers.lock().unwrap().iter() {
            if server.state() != State::Ready {
                continue;
            }
            for t in server.tools.lock().unwrap().iter() {
                let mut t = t.clone();
                t.name = format!("{name}__{}", t.name).into();
                tools.push(t);
            }
        }
        tools
    }

    pub(crate) async fn refresh_tools(&self, name: &str) {
        let server = self.servers.lock().unwrap().get(name).cloned();
        if let Some(server) = server {
            let peer = server.peer.lock().unwrap().clone();
            if let Some(peer) = peer
                && let Ok(tools) = peer.list_all_tools().await
            {
                *server.tools.lock().unwrap() = tools;
            }
        }
        self.notify_tools_changed().await;
    }

    pub(crate) async fn notify_tools_changed(&self) {
        let peers: Vec<Peer<RoleServer>> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| s.peer.lock().unwrap().clone())
            .collect();
        for p in peers {
            let _ = p.notify_tool_list_changed().await;
        }
    }

    pub(crate) async fn forward_progress(
        &self,
        server: &str,
        mut params: ProgressNotificationParam,
    ) {
        let target = self
            .progress
            .lock()
            .unwrap()
            .get(&(server.to_string(), params.progress_token.clone()))
            .cloned();
        if let Some((peer, token)) = target {
            params.progress_token = token;
            let _ = peer.notify_progress(params).await;
        }
    }

    async fn add(
        self: &Arc<Self>,
        def: ServerDef,
        wait: Duration,
        local: bool,
    ) -> Result<Value, Failure> {
        let existing = self.servers.lock().unwrap().get(&def.name).cloned();
        if let Some(old) = &existing {
            if old.def == def
                && !matches!(
                    old.state(),
                    State::Crashed(_) | State::Stopped | State::Unavailable(_)
                )
            {
                old.wait_settled(wait).await;
                return Ok(old.view());
            }
        } else if self.servers.lock().unwrap().len() >= servers::MAX_SERVERS {
            return Err(Failure::new(
                "BUSY",
                format!("一个链接最多同时运行 {} 个本机工具", servers::MAX_SERVERS),
            )
            .next("先用 link__server_remove 停掉不用的。"));
        }
        if !local && def.is_custom() && !self.cfg.yes_servers {
            self.hooks.feed(Feed::Server {
                name: def.name.clone(),
                title: def.title.clone(),
                event: "asking".into(),
                detail: Some(def.shown()),
            });
            let yes = self.hooks.confirm_server(&def).await;
            self.record(json!({"kind": "server", "event": if yes { "approved" } else { "declined" }, "link": self.cfg.link, "name": def.name, "command": def.shown()}));
            if !yes {
                return Err(Failure::new(
                    "DENIED_BY_USER",
                    "用户没有同意在这台电脑上运行这个本机工具",
                )
                .next(
                    "告诉用户这个工具没装上；需要的话请他在电脑上同意，或换一个目录里有的工具。",
                ));
            }
        }
        // What would make every start fail: say so now instead of a crash loop.
        let status = self.sandbox.status();
        if let Some(problem) = status.problems.first() {
            return Err(
                Failure::new(agentrouter_sandbox::UNAVAILABLE, problem.message.clone())
                    .next(problem.next.clone()),
            );
        }
        if let Some(pin) = &def.pin {
            self.packages.file(pin).map_err(|e| {
                Failure::new("PACKAGE_NEEDED", e.message)
                    .next("先用 link__package_put 发来这个包，再 link__server_add。")
            })?;
        }
        if servers::resolve(&def.command).is_none() {
            return Err(Failure::new(
                "RUNTIME_MISSING",
                format!("这台电脑上找不到 {}", def.command),
            )
            .next("npx 要装 Node.js，uvx 要装 uv；请用户先装上，或换一个工具。"));
        }
        if let Some(old) = existing {
            old.stop.cancel();
            old.wait_settled(Duration::from_secs(5)).await;
        }
        let server = Server::new(def);
        self.servers
            .lock()
            .unwrap()
            .insert(server.def.name.clone(), server.clone());
        self.server_event(&server, "starting", Some(server.def.shown()));
        tokio::spawn(servers::supervise(self.clone(), server.clone()));
        let state = server.wait_settled(wait).await;
        if let State::Unavailable(why) = state {
            return Err(Failure::new(agentrouter_sandbox::UNAVAILABLE, why)
                .next("请用户在这台电脑上运行 agentrouter sandbox setup，然后再试。"));
        }
        Ok(server.view())
    }

    async fn remove(&self, name: &str) -> Value {
        let server = self.servers.lock().unwrap().remove(name);
        match server {
            Some(server) => {
                server.stop.cancel();
                server.wait_settled(Duration::from_secs(5)).await;
                self.notify_tools_changed().await;
                json!({"name": name, "stopped": true})
            }
            None => json!({"name": name, "stopped": false}),
        }
    }

    /// The checkpoint before a non-read-only call (once per turn and session).
    async fn checkpoint_for(
        &self,
        session: &Session,
        turn: Option<String>,
        label: String,
    ) -> Option<String> {
        let now = tokio::time::Instant::now();
        let new_turn = {
            let mut last = session.last_write.lock().unwrap();
            let fresh = match (&*last, &turn) {
                (None, _) => true,
                (Some((t, _)), Some(turn)) => t != turn,
                (Some((_, at)), None) => now.duration_since(*at) > TURN_GAP,
            };
            let key = turn.clone().unwrap_or_else(|| {
                last.as_ref()
                    .filter(|_| !fresh)
                    .map(|(t, _)| t.clone())
                    .unwrap_or_else(|| format!("t{}", now_ms()))
            });
            *last = Some((key, now));
            fresh
        };
        if !new_turn {
            return None;
        }
        let hooks = self.hooks.clone();
        tokio::task::spawn_blocking(move || hooks.checkpoint(&label))
            .await
            .ok()
            .flatten()
    }

    async fn call(
        self: &Arc<Self>,
        s: &str,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.to_string();
        let arguments = request
            .arguments
            .clone()
            .map(Value::Object)
            .unwrap_or(json!({}));
        let Some((server_name, tool)) = split_tool(&name) else {
            return Err(
                Failure::new("UNKNOWN_TOOL", format!("没有这个工具：{name}"))
                    .next("用 tools/list 看这台电脑上有哪些工具。")
                    .to_error(),
            );
        };
        if server_name == "link" {
            return self
                .link_tool(tool, &arguments)
                .await
                .map(Into::into)
                .map_err(|f| f.to_error());
        }
        let Some(session) = self.session(s) else {
            return Err(Failure::new("SESSION_UNKNOWN", "会话已结束").to_error());
        };
        let server = self.servers.lock().unwrap().get(server_name).cloned();
        let Some(server) = server else {
            return Err(Failure::new(
                "SERVER_NOT_RUNNING",
                format!("本机工具 {server_name} 没有在运行"),
            )
            .next("用 link__servers 看在运行的本机工具，或用 link__server_add 装上它。")
            .to_error());
        };
        match server.wait_settled(Duration::from_secs(30)).await {
            State::Ready => {}
            State::Starting | State::Restarting => {
                return Err(Failure::new(
                    "SERVER_STARTING",
                    format!("本机工具 {server_name} 还在启动"),
                )
                .next("第一次启动要下载，可能要一两分钟：过一会儿再调。")
                .to_error());
            }
            State::Unavailable(why) => {
                return Err(Failure::new(agentrouter_sandbox::UNAVAILABLE, why)
                    .next("请用户在这台电脑上运行 agentrouter sandbox setup，然后再试。")
                    .to_error());
            }
            State::Crashed(why) => {
                return Err(Failure::new(
                    "SERVER_CRASHED",
                    format!("本机工具 {server_name} 反复退出：{why}"),
                )
                .next("用 link__server_logs 看它的日志；不要原样重试。")
                .to_error());
            }
            State::Stopped => {
                return Err(Failure::new(
                    "SERVER_NOT_RUNNING",
                    format!("本机工具 {server_name} 已停止"),
                )
                .to_error());
            }
        }
        let read_only = server
            .tools
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.name == tool)
            .and_then(|t| t.annotations.as_ref())
            .and_then(|a| a.read_only_hint)
            == Some(true);
        let meta = serde_json::to_value(&context.meta).unwrap_or(Value::Null);
        if !read_only {
            let turn = meta
                .get(META_TURN)
                .and_then(Value::as_str)
                .map(str::to_string);
            let label = format!("{} {}", name, hint_of(&arguments));
            if let Some(cp) = self.checkpoint_for(&session, turn, label).await
                && let Ok(key) = serde_json::to_value(&context.id).map(|v| id_key(&v))
                && let Some(p) = session.pending.lock().unwrap().get_mut(&key)
            {
                p.checkpoint = Some(cp);
            }
        }
        let timeout = meta
            .get(META_TIMEOUT)
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(CALL_DEFAULT)
            .clamp(Duration::from_secs(1), CALL_MAX);
        let Some(peer) = server.peer.lock().unwrap().clone() else {
            return Err(Failure::new(
                "SERVER_NOT_RUNNING",
                format!("本机工具 {server_name} 没有在运行"),
            )
            .to_error());
        };
        let mut params = CallToolRequestParams::new(tool.to_string());
        params.arguments = request.arguments.clone();
        let mut handle = match peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await
        {
            Ok(h) => h,
            Err(e) => {
                return Err(Failure::new(
                    "SERVER_CRASHED",
                    format!("本机工具 {server_name} 没有接住这个调用：{e}"),
                )
                .to_error());
            }
        };
        let upstream_token = context.meta.get_progress_token();
        let key = (server_name.to_string(), handle.progress_token.clone());
        if let Some(token) = upstream_token {
            self.progress
                .lock()
                .unwrap()
                .insert(key.clone(), (context.peer.clone(), token));
        }
        let answer = tokio::select! {
            r = &mut handle.rx => Some(r),
            _ = context.ct.cancelled() => None,
            _ = tokio::time::sleep(timeout) => {
                self.progress.lock().unwrap().remove(&key);
                let _ = handle.cancel(Some("timeout".into())).await;
                return Err(Failure::new("TIMEOUT", format!("{name} 在 {} 秒内没有答复，已取消", timeout.as_secs()))
                    .next("换一个更快的做法，或把事情拆小；不要原样重试。")
                    .to_error());
            }
        };
        self.progress.lock().unwrap().remove(&key);
        let Some(answer) = answer else {
            let _ = handle.cancel(Some("cancelled".into())).await;
            return Err(Failure::new("CANCELLED", "已取消").to_error());
        };
        match answer {
            Ok(Ok(ServerResult::CallToolResult(result))) => {
                let mut value = serde_json::to_value(&result).unwrap_or(Value::Null);
                servers::redact_value(&mut value, &server.def.secrets());
                let size = value.to_string().len();
                if size > RESULT_MAX {
                    return Err(
                        Failure::new("TOO_LARGE", format!("结果太大（{size} 字节）"))
                            .next("换一个返回更少内容的调用（只取一部分、缩小图片）。")
                            .to_error(),
                    );
                }
                let result: CallToolResult = serde_json::from_value(value).unwrap_or(result);
                Ok(CallToolResponse::Complete(result))
            }
            Ok(Ok(_)) => Err(Failure::new(
                "UNSUPPORTED",
                format!("{name} 要求的交互方式这台电脑还不支持"),
            )
            .to_error()),
            Ok(Err(rmcp::ServiceError::McpError(e))) => Err(e),
            Ok(Err(e)) => Err(Failure::new(
                "SERVER_CRASHED",
                format!("本机工具 {server_name} 在调用中断了：{e}"),
            )
            .next("用 link__server_logs 看它的日志。")
            .to_error()),
            Err(_) => Err(Failure::new(
                "SERVER_CRASHED",
                format!("本机工具 {server_name} 在调用中断了"),
            )
            .next("用 link__server_logs 看它的日志。")
            .to_error()),
        }
    }

    async fn link_tool(
        self: &Arc<Self>,
        tool: &str,
        args: &Value,
    ) -> Result<CallToolResult, Failure> {
        let value = match tool {
            "servers" => self.servers_view(),
            "server_add" => {
                let def = ServerDef::parse(args)?;
                let wait = args
                    .get("waitMs")
                    .and_then(Value::as_u64)
                    .map(Duration::from_millis)
                    .unwrap_or(Duration::from_secs(40))
                    .min(Duration::from_secs(120));
                Inner::add(self, def, wait, false).await?
            }
            "server_remove" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                self.remove(name).await
            }
            "server_logs" => {
                let name = args.get("name").and_then(Value::as_str).unwrap_or("");
                let lines = args
                    .get("lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(50)
                    .min(200) as usize;
                let server = self.servers.lock().unwrap().get(name).cloned();
                let Some(server) = server else {
                    return Err(Failure::new(
                        "SERVER_NOT_RUNNING",
                        format!("本机工具 {name} 没有在运行"),
                    ));
                };
                let logs = server.logs.lock().unwrap();
                let tail: Vec<&String> =
                    logs.iter().skip(logs.len().saturating_sub(lines)).collect();
                json!({"name": name, "state": server.state().label(), "lines": tail})
            }
            "package_put" => self.packages.store(args).map_err(Failure::from)?,
            other => {
                return Err(Failure::new(
                    "UNKNOWN_TOOL",
                    format!("没有这个管理工具：link__{other}"),
                ));
            }
        };
        Ok(text_result(&value))
    }
}

fn text_result(value: &Value) -> CallToolResult {
    serde_json::from_value(json!({
        "content": [{"type": "text", "text": value.to_string()}],
        "structuredContent": value,
        "isError": false,
    }))
    .expect("a plain text result")
}

/// The reserved management tools (§3.4).
pub fn link_tools() -> Vec<Tool> {
    let tool = |name: &str, description: &str, schema: Value, read_only: bool| -> Tool {
        serde_json::from_value(json!({
            "name": name,
            "description": description,
            "inputSchema": schema,
            "annotations": {"readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false},
        }))
        .expect("a valid tool")
    };
    let name_only =
        json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]});
    vec![
        tool(
            "link__servers",
            "The local MCP servers of this linked folder (state, tools count) and the sandbox.",
            json!({"type": "object", "properties": {}}),
            true,
        ),
        tool(
            "link__server_add",
            "Run a local MCP server in this folder's sandbox: a catalog preset (pinned package) or a custom command (the person answers y/N on the computer).",
            json!({"type": "object", "properties": {
                "name": {"type": "string"}, "title": {"type": "string"}, "command": {"type": "string"},
                "args": {"type": "array", "items": {"type": "string"}},
                "env": {"type": "object", "additionalProperties": {"type": "string"}},
                "secretEnv": {"type": "array", "items": {"type": "string"}},
                "keepOut": {"type": "array", "items": {"type": "string"}},
                "pin": {"type": "object"}, "network": {"type": "array", "items": {"type": "string"}},
                "preset": {"type": "string"}, "hash": {"type": "string"}, "waitMs": {"type": "integer"}
            }, "required": ["name", "command"]}),
            false,
        ),
        tool(
            "link__server_remove",
            "Stop a local MCP server.",
            name_only.clone(),
            false,
        ),
        tool(
            "link__server_logs",
            "The last log lines of a local MCP server (secrets blanked).",
            json!({"type": "object", "properties": {"name": {"type": "string"}, "lines": {"type": "integer"}}, "required": ["name"]}),
            true,
        ),
        tool(
            "link__package_put",
            "Store a pinned catalog package file (base64) after checking its registry digest.",
            json!({"type": "object", "properties": {"pin": {"type": "object"}, "data": {"type": "string"}}, "required": ["pin", "data"]}),
            false,
        ),
    ]
}

/// The MCP server side of one session.
struct SessionHandler {
    inner: std::sync::Weak<Inner>,
    s: String,
}

impl ServerHandler for SessionHandler {
    fn get_info(&self) -> InitializeResult {
        let folder = self
            .inner
            .upgrade()
            .map(|i| i.cfg.folder_name.clone())
            .unwrap_or_default();
        InitializeResult::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("agentrouter-link", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "Tools of the folder «{folder}» on the person's own computer. They work only inside that folder (the computer's sandbox refuses anything outside it). Tool names are <server>__<tool>; link__* manage which local servers run."
            ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .inner
            .upgrade()
            .map(|i| i.tool_list())
            .unwrap_or_default();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(inner) = self.inner.upgrade() else {
            return Err(Failure::new("LINK_LOST", "链接已关闭").to_error());
        };
        inner.call(&self.s, request, context).await
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(inner) = self.inner.upgrade()
            && let Some(session) = inner.session(&self.s)
        {
            *session.peer.lock().unwrap() = Some(context.peer);
        }
    }
}

/// The rmcp transport of one session: messages come from the Linker's frames, answers go back through it.
struct LinkTransport {
    rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
    s: String,
    inner: std::sync::Weak<Inner>,
}

impl Transport<RoleServer> for LinkTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let sent = match (
            self.inner.upgrade(),
            serde_json::to_value::<&ServerJsonRpcMessage>(&item),
        ) {
            (Some(inner), Ok(value)) => {
                inner.on_outgoing(&self.s, value);
                Ok(())
            }
            (None, _) => Err(std::io::Error::other("link closed")),
            (_, Err(e)) => Err(std::io::Error::other(e)),
        };
        std::future::ready(sent)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        self.rx.recv()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
