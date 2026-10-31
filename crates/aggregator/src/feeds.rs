//! The feed runner (Cloud PROJECT-SERVICE §9.8.5, SPACE-STRUCTURE §2.6, Cloud #2739 step 2): generic for any MCP server.
//!
//! A space whose runtime runs an MCP server (`runtime.program`) declares `feeds:` beside it — which tool to call, how
//! often, the dedupe key, the table each new item becomes a row of, and which error codes mean "back off". The platform
//! checks them at submit and sends the runtime instance's feeds to the host that runs it (`link__feeds`, signed like
//! every call). This runner then, per feed:
//!
//! 1. calls the tool of the named local server on its timer (`every`, ±20 % jitter; the first call soon after start);
//! 2. takes the items: `structuredContent` (at `items` when given, else the result itself must be the list), or JSON
//!    inside a text result found by the declared `extract.json` path; anything else is `FEED_OUTPUT_UNSTRUCTURED` —
//!    prose is never parsed;
//! 3. keeps of each item the columns of its table (`columns`; a number for a `text` column becomes its text) — a
//!    column the feed's `map` names is filled from that item field (a tool's `entityType` into `entity_type`), every
//!    other one from the field of its own name; nothing is renamed unless the feed declares it — and
//!    writes every item whose `key` this feed has not seen into the host's outbox — the key is marked seen in the same
//!    transaction, persisted, so a restart never sends a row twice (the Space Server's dedupe stays authoritative);
//! 4. backs off: an error carrying a code of `backoff.on` (an HTTP status, a JSON-RPC code or the server's own code;
//!    `-412` matches `412`) pauses the feed for `backoff.wait` and says so once; other errors wait longer each time
//!    (the feed's `every`, doubled per failure, at most an hour beyond it); a full outbox pauses polling until it drains.
//!
//! The status of every feed is in `link__servers` (`feeds`) and in the answer of `link__feeds`. Rows go into the
//! space's table (`into`): the platform upserts them by the table's key and the space raises `<table>.inserted|updated`
//! once per row when they land — a feed raises no event of its own (Cloud #2739, tables engine #2762).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::{Failure, Feed, Inner};

/// The platform's bounds on `every` (project-service `FEED_EVERY_MIN_MS`; a program's own floor is checked by Control).
pub const EVERY_MIN: Duration = Duration::from_secs(30);
pub const EVERY_MAX: Duration = Duration::from_secs(7 * 24 * 3600);
/// Feeds one runtime instance may run, and runtime instances one host may run feeds for.
pub const FEEDS_MAX: usize = 16;
pub const RUNTIMES_MAX: usize = 32;
/// How long one tool call of a feed may take.
const CALL_WAIT: Duration = Duration::from_secs(120);
/// How soon a paused feed looks again (its server not running yet, a full outbox).
const RECHECK: Duration = Duration::from_secs(30);

/// One feed as the Space Server describes it (`describe.feeds[]`) plus the columns of its table.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedSpec {
    pub name: String,
    pub tool: String,
    pub args: Option<Map<String, Value>>,
    pub every: Duration,
    pub key: String,
    pub items: Option<String>,
    pub extract_json: Option<String>,
    /// The space's table the items become rows of (the table raises `<table>.inserted|updated` when they land).
    pub into: String,
    /// The table's columns: name → type (`text`, `number`, …); an item keeps only these.
    pub columns: BTreeMap<String, String>,
    /// Column → the item field that fills it, for a column spelled otherwise than that field (the feed's `map`, Cloud
    /// #2739). A column not named here is filled by the item field of its own name.
    pub map: BTreeMap<String, String>,
    pub backoff_on: Vec<Value>,
    pub backoff_wait: Duration,
}

/// The feeds of one runtime instance on this host.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeFeeds {
    pub instance: String,
    pub space: String,
    /// The local MCP server (`link__server_add` name) that runs the space's program.
    pub server: String,
    pub feeds: Vec<FeedSpec>,
}

/// One new row a feed hands to the host's outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedRow {
    pub space: String,
    pub instance: String,
    /// The table it is a row of (the feed's `into`).
    pub table: String,
    /// `<feed>:<item key>`: the row's source key (this host's seen-set dedupes by it; the table upserts by its own key).
    pub key: String,
    pub payload: Value,
    /// What led to it: the feed's tool call (`feed:<instance>/<feed>@<ms>`).
    pub cause: String,
}

/// One declared event a space's runtime raised on this host (a `host` job started with `events`, Cloud #3607): the
/// platform takes it only for the space whose runtime instance runs on this link.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeEvent {
    pub space: String,
    pub instance: String,
    /// The declared event it raises, or — with `op` — the declared op of the space it calls (Cloud #3675).
    pub event: String,
    /// `event` names an op of the space, called with `payload` as its args (an `::agentrouter-op::` line).
    pub op: bool,
    pub payload: Value,
    /// The runtime's own key (a repeated line is one event); none: the outbox entry's id.
    pub key: Option<String>,
    /// `runtime:<instance> <job>`.
    pub cause: String,
}

/// The host's outbox as the runner sees it (implemented over `agentrouter_hostd_link::outbox::Outbox`).
pub trait EventSink: Send + Sync {
    /// Writes one event a space's runtime raised; its seq once it is on disk. A refusal names its catalog code
    /// (`OUTBOX_FULL`, `EVENT_TOO_LARGE`, `EVENT_INVALID`). The default: this sink takes no runtime events.
    fn append_event(&self, event: &RuntimeEvent) -> Result<u64, Failure> {
        Err(Failure::new(
            "OUTBOX_UNAVAILABLE",
            format!(
                "{} for {}: this host keeps no outbox for runtime events",
                event.event, event.space
            ),
        ))
    }
    /// Writes `row` unless `scope` saw its key before: `Ok(true)` written, `Ok(false)` seen before. A refusal names
    /// its catalog code (`OUTBOX_FULL` pauses the feed; `EVENT_TOO_LARGE` / `EVENT_INVALID` skip that row).
    fn append_new(&self, scope: &str, row: &FeedRow) -> Result<bool, Failure>;
    /// The producer should pause (the outbox is nearly full).
    fn nearly_full(&self) -> bool;
}

fn invalid(what: impl Into<String>) -> Failure {
    Failure::new("FEED_INVALID", what).next(
        "Control sends the space's checked feeds; fix the space's feeds in its space.yaml and submit it again.",
    )
}

fn text<'a>(v: &'a Value, field: &str, max: usize, w: &str) -> Result<&'a str, Failure> {
    match v.get(field).and_then(Value::as_str) {
        Some(s) if !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control) => Ok(s),
        _ => Err(invalid(format!(
            "{w}: {field} is a text of 1 to {max} characters"
        ))),
    }
}

fn ms(v: &Value, field: &str, w: &str) -> Result<Duration, Failure> {
    v.get(field)
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .ok_or_else(|| invalid(format!("{w}: {field} is a number of milliseconds")))
}

/// A path like `$`, `$.data.items`, `$.list[0].items` (the subset the Space Server checks).
pub fn valid_path(path: &str) -> bool {
    let Some(mut rest) = path.strip_prefix('$') else {
        return false;
    };
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            if end == 0 {
                return false;
            }
            rest = &r[end..];
        } else if let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else {
                return false;
            };
            if end == 0 || end > 4 || !r[..end].chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            rest = &r[end + 1..];
        } else {
            return false;
        }
    }
    true
}

/// The value at `path` in `v`.
pub fn select<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut at = v;
    let mut rest = path.strip_prefix('$')?;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            at = at.get(&r[..end])?;
            rest = &r[end..];
        } else {
            let r = rest.strip_prefix('[')?;
            let end = r.find(']')?;
            at = at.get(r[..end].parse::<usize>().ok()?)?;
            rest = &r[end + 1..];
        }
    }
    Some(at)
}

impl FeedSpec {
    /// The item field that fills `column`: the one the feed's `map` names, else the column's own name.
    pub fn field_of<'a>(&'a self, column: &'a str) -> &'a str {
        self.map.get(column).map(String::as_str).unwrap_or(column)
    }

    pub fn parse(v: &Value) -> Result<FeedSpec, Failure> {
        if !v.is_object() {
            return Err(invalid("a feed is an object"));
        }
        let name = text(v, "name", 64, "feed")?.to_string();
        let w = format!("feed {name}");
        let tool = text(v, "tool", 128, &w)?.to_string();
        if !tool
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Err(invalid(format!(
                "{w}: tool {tool:?} is not an MCP tool name"
            )));
        }
        let args = match v.get("args") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) if Value::Object(m.clone()).to_string().len() <= 4096 => {
                Some(m.clone())
            }
            Some(_) => return Err(invalid(format!("{w}: args is an object of at most 4 KiB"))),
        };
        let every = ms(v, "everyMs", &w)?;
        if every < EVERY_MIN || every > EVERY_MAX {
            return Err(invalid(format!("{w}: every is between 30 s and 7 days")));
        }
        let key = text(v, "key", 64, &w)?.to_string();
        let path = |field: &str, p: Option<&Value>| -> Result<Option<String>, Failure> {
            match p {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.len() <= 200 && valid_path(s) => Ok(Some(s.clone())),
                Some(_) => Err(invalid(format!("{w}: {field} is a path like $.data.items"))),
            }
        };
        let items = path("items", v.get("items"))?;
        let extract_json = path("extract.json", v.get("extract").and_then(|e| e.get("json")))?;
        let into = text(v, "into", 64, &w)?.to_string();
        let columns: BTreeMap<String, String> = v
            .get("columns")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, t)| (k.clone(), t.as_str().unwrap_or("text").to_string()))
                    .collect()
            })
            .ok_or_else(|| invalid(format!("{w}: columns lists the columns of table {into}")))?;
        if !columns.contains_key(&key) {
            return Err(invalid(format!(
                "{w}: key {key} is not a column of table {into} ({})",
                columns.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        let mut map = BTreeMap::new();
        match v.get("map") {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => {
                for (column, field) in m {
                    let Some(field) = field.as_str().filter(|f| !f.is_empty() && f.len() <= 64)
                    else {
                        return Err(invalid(format!(
                            "{w}: map {column} is the name of the item field that fills it"
                        )));
                    };
                    if !columns.contains_key(column) {
                        return Err(invalid(format!(
                            "{w}: map {column}: not a column of table {into} ({})",
                            columns.keys().cloned().collect::<Vec<_>>().join(", ")
                        )));
                    }
                    map.insert(column.clone(), field.to_string());
                }
            }
            Some(_) => {
                return Err(invalid(format!(
                    "{w}: map is an object {{ <column>: <item field> }}"
                )));
            }
        }
        let (backoff_on, backoff_wait) = match v.get("backoff") {
            None | Some(Value::Null) => (Vec::new(), Duration::ZERO),
            Some(b) => {
                let on = b
                    .get("on")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if on.is_empty()
                    || on.len() > 16
                    || on.iter().any(|c| !c.is_i64() && !c.is_string())
                {
                    return Err(invalid(format!(
                        "{w}: backoff.on lists 1 to 16 error codes (an HTTP status like 412, or the server's own code)"
                    )));
                }
                (on, ms(b, "waitMs", &format!("{w} backoff"))?)
            }
        };
        Ok(FeedSpec {
            name,
            tool,
            args,
            every,
            key,
            items,
            extract_json,
            into,
            columns,
            map,
            backoff_on,
            backoff_wait,
        })
    }
}

impl RuntimeFeeds {
    /// `link__feeds` arguments: `{instance, space, server, feeds: [...]}` (`feeds: []` stops them).
    pub fn parse(args: &Value) -> Result<RuntimeFeeds, Failure> {
        let instance = text(args, "instance", 120, "link__feeds")?.to_string();
        let space = text(args, "space", 200, "link__feeds")?.to_string();
        let server = text(args, "server", 24, "link__feeds")?.to_string();
        let list = args.get("feeds").and_then(Value::as_array).ok_or_else(|| {
            invalid("link__feeds: feeds is a list (empty: stop this instance's feeds)")
        })?;
        if list.len() > FEEDS_MAX {
            return Err(invalid(format!(
                "link__feeds: at most {FEEDS_MAX} feeds per runtime"
            )));
        }
        let feeds = list
            .iter()
            .map(FeedSpec::parse)
            .collect::<Result<Vec<_>, _>>()?;
        let mut names: Vec<&str> = feeds.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid("link__feeds: two feeds share a name"));
        }
        Ok(RuntimeFeeds {
            instance,
            space,
            server,
            feeds,
        })
    }
}

/// Why a call of a feed's tool did not give a result: its error codes (for `backoff.on`) and its words.
#[derive(Debug, Clone, PartialEq)]
pub struct CallError {
    pub not_ready: bool,
    pub codes: Vec<String>,
    pub message: String,
}

/// The codes an error carries: a JSON-RPC code, `code` / `status` / `error.code` fields of a structured error, and
/// numbers or UPPER_CASE words in its text.
pub fn codes_in(text: &str, structured: Option<&Value>) -> Vec<String> {
    let mut codes = Vec::new();
    let mut push = |c: String| {
        if !c.is_empty() && !codes.contains(&c) {
            codes.push(c);
        }
    };
    if let Some(s) = structured {
        for p in [
            "$.code",
            "$.status",
            "$.error.code",
            "$.error.status",
            "$.data.code",
        ] {
            match select(s, p) {
                Some(Value::Number(n)) => push(n.to_string()),
                Some(Value::String(t)) if t.len() <= 64 => push(t.clone()),
                _ => {}
            }
        }
    }
    for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        let w = word.trim_matches('-');
        let numeric = w.chars().all(|c| c.is_ascii_digit()) && (2..=6).contains(&w.len());
        let upper = w.len() >= 3
            && w.len() <= 64
            && w.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && w.chars().any(|c| c.is_ascii_uppercase());
        if numeric || upper {
            push(w.to_string());
        }
        if word.starts_with('-') && numeric {
            push(word.to_string());
        }
    }
    codes
}

/// Whether any of `codes` is one of `on` (numbers compare by absolute value: B站's `-412` is `412`).
pub fn backoff_matches(on: &[Value], codes: &[String]) -> bool {
    on.iter().any(|want| {
        codes.iter().any(|c| match want {
            Value::Number(n) => {
                let w = n.as_i64().unwrap_or(i64::MIN);
                c.parse::<i64>().is_ok_and(|v| v.abs() == w.abs())
            }
            Value::String(s) => c == s,
            _ => false,
        })
    })
}

/// The items of one tool result, or why there are none.
pub fn items_of(result: &CallToolResult, spec: &FeedSpec) -> Result<Vec<Value>, Failure> {
    let (base, path) = if let Some(structured) = &result.structured_content {
        (
            structured.clone(),
            spec.items.clone().unwrap_or_else(|| "$".into()),
        )
    } else if let Some(rule) = &spec.extract_json {
        let found = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.trim().to_string()))
            .filter_map(|t| serde_json::from_str::<Value>(&t).ok())
            .find(|v| select(v, rule).is_some());
        match found {
            Some(v) => (v, rule.clone()),
            None => {
                return Err(Failure::new(
                    "FEED_OUTPUT_UNSTRUCTURED",
                    format!(
                        "feed {}: {} returned no text that is JSON with {rule} in it",
                        spec.name, spec.tool
                    ),
                ));
            }
        }
    } else {
        return Err(Failure::new(
            "FEED_OUTPUT_UNSTRUCTURED",
            format!(
                "feed {}: {} returned no structuredContent and the feed declares no extract rule",
                spec.name, spec.tool
            ),
        ));
    };
    match select(&base, &path) {
        Some(Value::Array(list)) => Ok(list.clone()),
        other => {
            let lists: Vec<String> = base
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(_, v)| v.is_array())
                        .map(|(k, _)| format!("$.{k}"))
                        .collect()
                })
                .unwrap_or_default();
            Err(Failure::new(
                "FEED_ITEMS_NOT_A_LIST",
                format!(
                    "feed {}: {} at {path} is {}, not a list{}",
                    spec.name,
                    spec.tool,
                    match other {
                        None => "missing".to_string(),
                        Some(Value::Object(_)) => "an object".to_string(),
                        Some(v) => format!("{v}").chars().take(40).collect(),
                    },
                    if lists.is_empty() {
                        String::new()
                    } else {
                        format!(" (lists in the result: {})", lists.join(", "))
                    }
                ),
            ))
        }
    }
}

/// An item as a row: its table's columns only, each from the item field that fills it (`map`, else the field of
/// its name; a number or a boolean for a `text` column becomes its text); its key as text. None when the key is
/// missing.
pub fn row_of(item: &Value, spec: &FeedSpec) -> Option<(String, Value)> {
    let obj = item.as_object()?;
    let key = match obj.get(spec.field_of(&spec.key))? {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let mut payload = Map::new();
    for (column, ty) in &spec.columns {
        let Some(v) = obj.get(spec.field_of(column)) else {
            continue;
        };
        let v = match (ty.as_str(), v) {
            ("text", Value::Number(n)) => Value::String(n.to_string()),
            ("text", Value::Bool(b)) => Value::String(b.to_string()),
            _ => v.clone(),
        };
        payload.insert(column.clone(), v);
    }
    Some((key, Value::Object(payload)))
}

/// What one feed is doing now.
#[derive(Debug, Clone, Default)]
struct FeedStatus {
    state: &'static str,
    at: i64,
    next_at: i64,
    until: Option<i64>,
    last_ok: Option<i64>,
    runs: u64,
    written: u64,
    last_written: u64,
    skipped: u64,
    error: Option<(String, String)>,
}

impl FeedStatus {
    fn view(&self, name: &str, spec: &FeedSpec) -> Value {
        json!({
            "name": name, "tool": spec.tool, "into": spec.into, "everyMs": spec.every.as_millis() as u64,
            "state": self.state, "at": self.at, "nextAt": self.next_at, "until": self.until, "lastOkAt": self.last_ok,
            "runs": self.runs, "written": self.written, "lastWritten": self.last_written, "skipped": self.skipped,
            "error": self.error.as_ref().map(|(code, message)| json!({"code": code, "message": message})),
        })
    }
}

struct Running {
    spec: RuntimeFeeds,
    stop: CancellationToken,
    status: Arc<Mutex<BTreeMap<String, FeedStatus>>>,
}

/// The feeds this host runs, by runtime instance.
#[derive(Default)]
pub struct Feeds {
    running: Mutex<BTreeMap<String, Running>>,
}

fn now_ms() -> i64 {
    agentrouter_device::util::now_ms()
}

/// `d` ±20 %, from the clock's nanoseconds (no randomness source needed for spreading polls).
fn jitter(d: Duration) -> Duration {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    let factor = 0.8 + f64::from(n % 4001) / 10_000.0;
    d.mul_f64(factor)
}

impl Feeds {
    /// Every feed's status, by instance.
    pub fn view(&self) -> Value {
        let running = self.running.lock().unwrap();
        Value::Array(running.values().map(Running::view).collect())
    }

    /// Stops every feed (the link stopped).
    pub fn stop_all(&self) {
        for (_, r) in std::mem::take(&mut *self.running.lock().unwrap()) {
            r.stop.cancel();
        }
    }
}

impl Running {
    fn view(&self) -> Value {
        let status = self.status.lock().unwrap();
        json!({
            "instance": self.spec.instance, "space": self.spec.space, "server": self.spec.server,
            "feeds": self.spec.feeds.iter().map(|f| status.get(&f.name).cloned().unwrap_or_default().view(&f.name, f)).collect::<Vec<_>>(),
        })
    }
}

/// `link__feeds`: replaces the feeds of one runtime instance (unchanged ones keep running and keep their timers).
pub(crate) fn set(inner: &Arc<Inner>, args: &Value) -> Result<Value, Failure> {
    let spec = RuntimeFeeds::parse(args)?;
    let Some(sink) = inner.hooks.events() else {
        return Err(Failure::new(
            "OUTBOX_UNAVAILABLE",
            "this computer's event outbox is not open, so feeds cannot write anything",
        )
        .next("Check hostd's start lines on that computer (its data folder must be writable)."));
    };
    let mut running = inner.feeds.running.lock().unwrap();
    if let Some(old) = running.get(&spec.instance)
        && old.spec == spec
    {
        return Ok(old.view());
    }
    if let Some(old) = running.remove(&spec.instance) {
        old.stop.cancel();
    }
    if spec.feeds.is_empty() {
        inner.record(json!({"kind": "feeds", "event": "stopped", "link": inner.cfg.link, "instance": spec.instance, "space": spec.space}));
        return Ok(
            json!({"instance": spec.instance, "space": spec.space, "server": spec.server, "feeds": []}),
        );
    }
    if running.len() >= RUNTIMES_MAX {
        return Err(Failure::new(
            "BUSY",
            format!("this computer runs feeds for at most {RUNTIMES_MAX} runtimes"),
        )
        .next("Stop the feeds of a runtime that is no longer used (link__feeds with feeds: [])."));
    }
    let stop = CancellationToken::new();
    let status = Arc::new(Mutex::new(BTreeMap::new()));
    for feed in &spec.feeds {
        tokio::spawn(run(
            Arc::downgrade(inner),
            sink.clone(),
            spec.clone(),
            feed.clone(),
            status.clone(),
            stop.clone(),
        ));
    }
    inner.record(json!({"kind": "feeds", "event": "set", "link": inner.cfg.link, "instance": spec.instance, "space": spec.space, "server": spec.server,
                        "feeds": spec.feeds.iter().map(|f| json!({"name": f.name, "tool": f.tool, "everyMs": f.every.as_millis() as u64, "into": f.into})).collect::<Vec<_>>()}));
    let r = Running { spec, stop, status };
    let view = r.view();
    running.insert(r.spec.instance.clone(), r);
    Ok(view)
}

/// What one poll came to.
enum Polled {
    Written {
        new: u64,
        skipped: u64,
        problem: Option<Failure>,
    },
    Paused(Failure),
    NotReady(Failure),
    BackOff(Failure),
    Failed(Failure),
}

async fn poll(
    inner: &Arc<Inner>,
    sink: &Arc<dyn EventSink>,
    rt: &RuntimeFeeds,
    feed: &FeedSpec,
) -> Polled {
    if sink.nearly_full() {
        return Polled::Paused(Failure::new(
            "OUTBOX_FULL",
            "this computer's outbox is nearly full; the feed waits until the link has taken the waiting events",
        ));
    }
    let called = now_ms();
    let result = match inner
        .feed_call(&rt.server, &feed.tool, feed.args.clone(), CALL_WAIT)
        .await
    {
        Ok(r) => r,
        Err(e) if e.not_ready => {
            return Polled::NotReady(Failure::new("FEED_SERVER_NOT_READY", e.message));
        }
        Err(e) => {
            let f = Failure::new("FEED_TOOL_FAILED", e.message);
            return if backoff_matches(&feed.backoff_on, &e.codes) {
                Polled::BackOff(f)
            } else {
                Polled::Failed(f)
            };
        }
    };
    if result.is_error == Some(true) {
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join(" ");
        let codes = codes_in(&text, result.structured_content.as_ref());
        let f = Failure::new(
            "FEED_TOOL_FAILED",
            format!(
                "{} answered an error: {}",
                feed.tool,
                agentrouter_device::util::clip(&text, 600)
            ),
        );
        return if backoff_matches(&feed.backoff_on, &codes) {
            Polled::BackOff(f)
        } else {
            Polled::Failed(f)
        };
    }
    let items = match items_of(&result, feed) {
        Ok(items) => items,
        Err(f) => return Polled::Failed(f),
    };
    let scope = format!("{}/{}", rt.space, feed.name);
    let cause = format!("feed:{}/{}@{called}", rt.instance, feed.name);
    let (mut new, mut skipped, mut problem) = (0u64, 0u64, None);
    let mut no_key_fields: Option<Vec<String>> = None;
    for item in &items {
        let Some((key, payload)) = row_of(item, feed) else {
            skipped += 1;
            if no_key_fields.is_none() {
                no_key_fields = Some(
                    item.as_object()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default(),
                );
            }
            continue;
        };
        let row = FeedRow {
            space: rt.space.clone(),
            instance: rt.instance.clone(),
            table: feed.into.clone(),
            key: format!("{}:{key}", feed.name),
            payload,
            cause: cause.clone(),
        };
        match sink.append_new(&scope, &row) {
            Ok(true) => new += 1,
            Ok(false) => {}
            Err(f) if f.code == "OUTBOX_FULL" => return Polled::Paused(f),
            Err(f) => {
                skipped += 1;
                problem = Some(f);
            }
        }
    }
    if let Some(fields) = no_key_fields {
        problem = Some(Failure::new(
            "FEED_ITEM_NO_KEY",
            format!(
                "feed {}: {skipped} items had no {} (their fields: {})",
                feed.name,
                feed.field_of(&feed.key),
                fields.join(", ")
            ),
        ));
    }
    Polled::Written {
        new,
        skipped,
        problem,
    }
}

async fn run(
    inner: std::sync::Weak<Inner>,
    sink: Arc<dyn EventSink>,
    rt: RuntimeFeeds,
    feed: FeedSpec,
    status: Arc<Mutex<BTreeMap<String, FeedStatus>>>,
    stop: CancellationToken,
) {
    let mut failures: u32 = 0;
    let mut wait = jitter(Duration::from_secs(5));
    let mut said: Option<&'static str> = None;
    loop {
        {
            let mut s = status.lock().unwrap();
            let e = s.entry(feed.name.clone()).or_default();
            if e.state.is_empty() {
                e.state = "starting";
            }
            e.next_at = now_ms() + wait.as_millis() as i64;
        }
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stop.cancelled() => return,
        }
        let Some(inner) = inner.upgrade() else { return };
        let polled = poll(&inner, &sink, &rt, &feed).await;
        let now = now_ms();
        let mut s = status.lock().unwrap();
        let e = s.entry(feed.name.clone()).or_default();
        e.at = now;
        e.runs += 1;
        let (state, detail): (&'static str, Option<String>) = match polled {
            Polled::Written {
                new,
                skipped,
                problem,
            } => {
                failures = 0;
                e.last_ok = Some(now);
                e.written += new;
                e.last_written = new;
                e.skipped += skipped;
                e.until = None;
                e.error = problem.map(|p| (p.code.to_string(), p.message));
                wait = jitter(feed.every);
                ("running", None)
            }
            Polled::Paused(f) => {
                wait = RECHECK;
                let m = f.message.clone();
                e.error = Some((f.code.to_string(), f.message));
                ("paused", Some(m))
            }
            Polled::NotReady(f) => {
                wait = RECHECK;
                let m = f.message.clone();
                e.error = Some((f.code.to_string(), f.message));
                ("waiting", Some(m))
            }
            Polled::BackOff(f) => {
                failures = 0;
                wait = feed.backoff_wait;
                e.until = Some(now + wait.as_millis() as i64);
                let m = f.message.clone();
                e.error = Some(("FEED_BACKING_OFF".into(), f.message));
                ("backing_off", Some(m))
            }
            Polled::Failed(f) => {
                failures = failures.saturating_add(1);
                let grown = feed.every.saturating_mul(1u32 << failures.min(6));
                wait = jitter(grown.min(feed.every + Duration::from_secs(3600)));
                let m = format!("{}: {}", f.code, f.message);
                e.error = Some((f.code.to_string(), f.message));
                ("failing", Some(m))
            }
        };
        e.state = state;
        e.next_at = now + wait.as_millis() as i64;
        drop(s);
        // Say a change of state once (the person's terminal and the audit), never every poll.
        if said != Some(state) && (state != "running" || said.is_some()) {
            inner.record(json!({"kind": "feeds", "event": state, "link": inner.cfg.link, "instance": rt.instance, "space": rt.space, "feed": feed.name, "detail": detail}));
            inner.hooks.feed(Feed::Server {
                name: rt.server.clone(),
                title: format!("feed {}", feed.name),
                event: format!("feed_{state}"),
                detail,
            });
        }
        said = Some(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> FeedSpec {
        FeedSpec::parse(&json!({
            "name": "关注动态", "tool": "get_following_feed", "everyMs": 900_000, "key": "id", "items": "$.items",
            "into": "feed", "columns": {"id": "text", "title": "text"},
            "backoff": {"on": [412, 429, "RATE_LIMITED"], "waitMs": 1_800_000}
        }))
        .unwrap()
    }

    #[test]
    fn paths() {
        assert!(valid_path("$") && valid_path("$.data.items") && valid_path("$.a[0].b"));
        assert!(!valid_path("data") && !valid_path("$..a") && !valid_path("$[x]"));
        let v = json!({"data": {"list": [{"x": 1}]}});
        assert_eq!(select(&v, "$.data.list[0].x"), Some(&json!(1)));
        assert_eq!(select(&v, "$.data.nope"), None);
    }

    #[test]
    fn items_from_structured_content_or_a_declared_extract_rule() {
        let s = spec();
        let r: CallToolResult = serde_json::from_value(
            json!({"content": [], "structuredContent": {"items": [{"id": 1}], "more": []}}),
        )
        .unwrap();
        assert_eq!(items_of(&r, &s).unwrap(), vec![json!({"id": 1})]);
        let r: CallToolResult =
            serde_json::from_value(json!({"content": [], "structuredContent": {"list": []}}))
                .unwrap();
        let e = items_of(&r, &s).unwrap_err();
        assert_eq!(e.code, "FEED_ITEMS_NOT_A_LIST");
        assert!(e.message.contains("$.list"), "{}", e.message);
        let prose: CallToolResult =
            serde_json::from_value(json!({"content": [{"type": "text", "text": "3 new posts"}]}))
                .unwrap();
        assert_eq!(
            items_of(&prose, &s).unwrap_err().code,
            "FEED_OUTPUT_UNSTRUCTURED"
        );
        let mut x = s.clone();
        x.extract_json = Some("$.data.cards".into());
        let text: CallToolResult = serde_json::from_value(json!({"content": [{"type": "text", "text": "{\"data\": {\"cards\": [{\"id\": \"a\"}]}}"}]})).unwrap();
        assert_eq!(items_of(&text, &x).unwrap(), vec![json!({"id": "a"})]);
    }

    #[test]
    fn rows_keep_the_declared_fields() {
        let s = spec();
        let (key, payload) =
            row_of(&json!({"id": 42, "title": "t", "extra": {"x": 1}}), &s).unwrap();
        assert_eq!(key, "42");
        assert_eq!(payload, json!({"id": "42", "title": "t"}));
        assert!(row_of(&json!({"title": "no id"}), &s).is_none());
    }

    #[test]
    fn a_mapped_column_is_filled_from_the_item_field_the_feed_names() {
        // memory_graph's shape (Cloud #2739): camelCase item fields, lower-case columns.
        let s = FeedSpec::parse(&json!({
            "name": "图谱实体", "tool": "read_graph", "everyMs": 30_000, "key": "name", "items": "$.entities",
            "into": "entities", "columns": {"name": "text", "entity_type": "text"},
            "map": {"entity_type": "entityType"}
        }))
        .unwrap();
        let (key, payload) = row_of(
            &json!({"name": "alpha", "entityType": "test", "observations": ["x"]}),
            &s,
        )
        .unwrap();
        assert_eq!(key, "alpha");
        assert_eq!(payload, json!({"name": "alpha", "entity_type": "test"}));
        // Without the map nothing is renamed: the column stays empty.
        let mut plain = s.clone();
        plain.map.clear();
        let (_, payload) = row_of(&json!({"name": "alpha", "entityType": "test"}), &plain).unwrap();
        assert_eq!(payload, json!({"name": "alpha"}));
        // The key column may be mapped too.
        let k = FeedSpec::parse(&json!({
            "name": "a", "tool": "t", "everyMs": 60_000, "key": "entity_id", "into": "x",
            "columns": {"entity_id": "text"}, "map": {"entity_id": "entityId"}
        }))
        .unwrap();
        assert_eq!(row_of(&json!({"entityId": 7}), &k).unwrap().0, "7");
        // A map entry for a column the table does not have is refused.
        let e = FeedSpec::parse(&json!({
            "name": "a", "tool": "t", "everyMs": 60_000, "key": "id", "into": "x",
            "columns": {"id": "text"}, "map": {"kind": "entityType"}
        }))
        .unwrap_err();
        assert!(
            e.message.contains("map kind: not a column of table x"),
            "{}",
            e.message
        );
    }

    #[test]
    fn backoff_codes() {
        let s = spec();
        assert!(backoff_matches(
            &s.backoff_on,
            &codes_in("request failed: code -412 risk control", None)
        ));
        assert!(backoff_matches(
            &s.backoff_on,
            &codes_in("HTTP 429 Too Many Requests", None)
        ));
        assert!(backoff_matches(
            &s.backoff_on,
            &codes_in("", Some(&json!({"error": {"code": "RATE_LIMITED"}})))
        ));
        assert!(!backoff_matches(
            &s.backoff_on,
            &codes_in("HTTP 500 Internal Server Error", None)
        ));
    }

    #[test]
    fn invalid_feeds_are_refused_naming_the_field() {
        let e = FeedSpec::parse(&json!({"name": "a", "tool": "t", "everyMs": 1000, "key": "id", "into": "x", "columns": {"id": "text"}})).unwrap_err();
        assert_eq!(e.code, "FEED_INVALID");
        assert!(e.message.contains("every"));
        let e = FeedSpec::parse(&json!({"name": "a", "tool": "t", "everyMs": 60000, "key": "id", "into": "x", "columns": {"title": "text"}})).unwrap_err();
        assert!(e.message.contains("key id"), "{}", e.message);
    }
}
