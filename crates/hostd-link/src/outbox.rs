//! The host's outbox (Cloud PROJECT-SERVICE §9.8.2, Cloud #2739 step 1): every event this host produces for a space —
//! a feed row, a runtime's event, later a device space's file change — is written here first, on disk, with the host's
//! own monotonic sequence number and a globally unique event id, and leaves only after the platform acknowledged it.
//!
//! - **Store:** one SQLite database per link in hostd's own data directory (`rusqlite`, bundled SQLite; WAL with
//!   `synchronous=FULL`, so an accepted event survives a crash, a sleep or a power cut). `AUTOINCREMENT` never reuses a
//!   sequence number, also after the entries below it left.
//! - **Event ids:** `<producer>:<seq>`, where the producer is the link id plus a random id the database was created
//!   with: a reinstalled hostd (a new database) can never repeat an earlier event id, so the platform's dedupe by event
//!   id never swallows a new event.
//! - **Flush:** the link layer sends `{"op":"events","host":<producer>,"items":[…]}` in seq order after every welcome,
//!   one batch at a time; the platform answers `{"op":"events_ack","host","seq":<highest contiguous seq it is done
//!   with>,"refused":[{seq,code,message}]}`. Entries up to `seq` leave the outbox; refused ones (the platform will never
//!   take them: an undeclared event, a payload that does not fit) are kept in a bounded `refused` table for the status.
//!   A batch nobody acknowledged is sent again (the platform dedupes by event id).
//! - **Bounded, never lossy:** past the disk cap [`Outbox::append`] refuses with `OUTBOX_FULL`; the producer pauses
//!   (a feed stops polling and says so in its status). Nothing accepted is dropped.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tokio::sync::Notify;

/// A payload is at most this much JSON (the Space Server's event payload cap, project-service `PAYLOAD_MAX`).
pub const PAYLOAD_MAX: usize = 32 * 1024;
/// Default disk cap of one outbox (a `config.json` setting may lower or raise it).
pub const DEFAULT_CAP_BYTES: u64 = 256 << 20;
/// One `events` frame carries at most this many items …
pub const BATCH_ITEMS: usize = 200;
/// … and at most this much JSON (well under the 8 MiB frame).
pub const BATCH_BYTES: usize = 1 << 20;
/// Keys one dedupe scope (a feed) remembers; older ones are forgotten (the Space Server's own dedupe stays authoritative).
pub const SEEN_KEEP: i64 = 20_000;
/// Refusals kept for the status (the oldest leave first).
const REFUSED_KEEP: i64 = 500;

/// One event a producer on this host hands to the outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// The space the event belongs to (the platform checks that this host may write it).
    pub space: String,
    /// The platform instance that produced it (a space's runtime on this host), when there is one.
    pub instance: Option<String>,
    /// A declared event of that space (`job.done`), or none when the entry is a row of `table`.
    pub event: Option<String>,
    /// A declared table of that space the entry is a row of (a feed's `into`): the platform upserts it by the table's key
    /// and the space raises `<table>.inserted|updated` itself when the row lands (Cloud #2739, tables engine #2762).
    pub table: Option<String>,
    /// The declared payload, or the row (an object).
    pub payload: Value,
    /// The item's own key at its source (a feed row's key), recorded in the event's provenance.
    pub key: Option<String>,
    /// What led to it (an event id or a tool call), the provenance `cause`.
    pub cause: Option<String>,
}

/// Why an event was not accepted; `code` is a catalog code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// One refusal the platform answered for an entry (`events_ack.refused[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub seq: u64,
    pub code: String,
    pub message: String,
}

/// What the outbox holds, for the status of the producers on this host.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub producer: String,
    pub pending: u64,
    pub bytes: u64,
    pub cap: u64,
    pub full: bool,
    /// The newest seq handed out (0: none yet) and the highest the platform acknowledged.
    pub last_seq: u64,
    pub acked: u64,
    pub refused: u64,
    pub last_refused: Option<Value>,
}

impl Status {
    pub fn to_json(&self) -> Value {
        json!({
            "producer": self.producer, "pending": self.pending, "bytes": self.bytes, "cap": self.cap, "full": self.full,
            "lastSeq": self.last_seq, "acked": self.acked, "refused": self.refused, "lastRefused": self.last_refused,
        })
    }
}

/// The on-disk outbox of one link. Cheap to share (`Arc<Outbox>`); every method is short and synchronous.
pub struct Outbox {
    db: Mutex<Db>,
    producer: String,
    cap: u64,
    path: PathBuf,
    wake: Notify,
}

struct Db {
    conn: Connection,
    bytes: u64,
    acked: u64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sql(e: rusqlite::Error, path: &Path) -> String {
    format!("outbox {}: {e}", path.display())
}

/// What an entry is, for messages: `event job.done` or `row of table feed`.
fn what(e: &NewEvent) -> String {
    match (&e.event, &e.table) {
        (Some(ev), _) => ev.clone(),
        (None, Some(t)) => format!("a row of table {t}"),
        (None, None) => "an entry".into(),
    }
}

fn valid_table(table: &str) -> bool {
    // The Space Server's table names: a lower-case word of [a-z0-9_], starting with a letter.
    (1..=64).contains(&table.len())
        && table.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && table
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn valid_name(event: &str) -> bool {
    // Lower-case words of [a-z0-9_] joined by dots, each starting with a letter (the Space Server's event names).
    !event.is_empty()
        && event.len() <= 120
        && event.split('.').all(|w| {
            w.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

fn text_ok(v: &str, max: usize) -> bool {
    !v.is_empty() && v.len() <= max && !v.chars().any(char::is_control)
}

impl Outbox {
    /// `<data dir>/outbox/<link>.sqlite`.
    pub fn path_for(data_dir: &Path, link: &str) -> PathBuf {
        data_dir.join("outbox").join(format!("{link}.sqlite"))
    }

    /// Opens (or creates) the outbox of `link` at `path` with a disk cap of `cap` bytes.
    pub fn open(path: &Path, link: &str, cap: u64) -> Result<Outbox, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("outbox {}: {e}", dir.display()))?;
        }
        let conn = Connection::open(path).map_err(|e| sql(e, path))?;
        conn.pragma_update(None, "journal_mode", "wal")
            .map_err(|e| sql(e, path))?;
        // FULL: a commit is on disk before append returns (power cut, sleep), not only on the next checkpoint.
        conn.pragma_update(None, "synchronous", "full")
            .map_err(|e| sql(e, path))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| sql(e, path))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS outbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, item TEXT NOT NULL,
                                                bytes INTEGER NOT NULL, at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS refused (seq INTEGER PRIMARY KEY, item TEXT, code TEXT NOT NULL,
                                                 message TEXT NOT NULL, at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS seen (scope TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL,
                                              PRIMARY KEY (scope, key));",
        )
        .map_err(|e| sql(e, path))?;
        let stored: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k='producer'", [], |r| r.get(0))
            .optional()
            .map_err(|e| sql(e, path))?;
        let producer = match stored {
            Some(p) if p.starts_with(&format!("{link}.")) => p,
            Some(p) => {
                return Err(format!(
                    "outbox {} belongs to {p}, not to link {link}",
                    path.display()
                ));
            }
            None => {
                let mut r = [0u8; 8];
                getrandom::fill(&mut r).map_err(|e| format!("outbox: no randomness: {e}"))?;
                let id: String = r.iter().map(|b| format!("{b:02x}")).collect();
                let p = format!("{link}.{id}");
                conn.execute("INSERT INTO meta (k, v) VALUES ('producer', ?1)", [&p])
                    .map_err(|e| sql(e, path))?;
                p
            }
        };
        let bytes: i64 = conn
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM outbox", [], |r| {
                r.get(0)
            })
            .map_err(|e| sql(e, path))?;
        let acked: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k='acked'", [], |r| r.get(0))
            .optional()
            .map_err(|e| sql(e, path))?;
        let outbox = Outbox {
            db: Mutex::new(Db {
                conn,
                bytes: bytes.max(0) as u64,
                acked: acked.and_then(|a| a.parse().ok()).unwrap_or(0),
            }),
            producer,
            cap,
            path: path.to_path_buf(),
            wake: Notify::new(),
        };
        if outbox.status().pending > 0 {
            outbox.wake.notify_one();
        }
        Ok(outbox)
    }

    /// `<link>.<random id of this database>`: the `host` of every `events` frame and the prefix of every event id.
    pub fn producer(&self) -> &str {
        &self.producer
    }

    /// The event id of an entry.
    pub fn event_id(&self, seq: u64) -> String {
        format!("{}:{seq}", self.producer)
    }

    /// Writes one event; returns its seq once it is on disk. Refuses an event the platform could never take
    /// (`EVENT_INVALID`, `EVENT_TOO_LARGE`) and, past the disk cap, `OUTBOX_FULL` (pause the producer; retry later).
    pub fn append(&self, e: &NewEvent) -> Result<u64, Refusal> {
        let text = Self::entry(e)?;
        let mut db = self.db.lock().unwrap();
        let seq = self.insert(&mut db, &text)?;
        drop(db);
        self.wake.notify_one();
        Ok(seq)
    }

    /// Writes one event unless `scope` already saw its `key` (a feed's own dedupe, persisted with the outbox so a
    /// restart or a reinstall of the feed never sends a row twice): `Ok(None)` for a key seen before. The key is marked
    /// seen in the same transaction as the write, so a refusal (`OUTBOX_FULL`, …) leaves it unseen for the next try.
    /// Each scope keeps its newest [`SEEN_KEEP`] keys.
    pub fn append_new(&self, scope: &str, e: &NewEvent) -> Result<Option<u64>, Refusal> {
        let Some(key) = e.key.as_deref() else {
            return Err(Refusal {
                code: "EVENT_INVALID",
                message: format!("{} for {} has no key to deduplicate by", what(e), e.space),
            });
        };
        if !text_ok(scope, 400) {
            return Err(Refusal {
                code: "EVENT_INVALID",
                message: "a dedupe scope is a text of 1 to 400 characters".into(),
            });
        }
        let text = Self::entry(e)?;
        let mut db = self.db.lock().unwrap();
        let fault = |err: rusqlite::Error| Refusal {
            code: "OUTBOX_FAULT",
            message: sql(err, &self.path),
        };
        let seen: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM seen WHERE scope = ?1 AND key = ?2)",
                params![scope, key],
                |r| r.get(0),
            )
            .map_err(fault)?;
        if seen {
            return Ok(None);
        }
        db.conn.execute_batch("BEGIN IMMEDIATE").map_err(fault)?;
        let written = self.insert(&mut db, &text).and_then(|seq| {
            db.conn
                .execute(
                    "INSERT OR IGNORE INTO seen (scope, key, at) VALUES (?1, ?2, ?3)",
                    params![scope, key, now_ms()],
                )
                .map_err(fault)?;
            if seq % 128 == 0 {
                db.conn
                    .execute(
                        "DELETE FROM seen WHERE scope = ?1 AND rowid NOT IN
                           (SELECT rowid FROM seen WHERE scope = ?1 ORDER BY at DESC, rowid DESC LIMIT ?2)",
                        params![scope, SEEN_KEEP],
                    )
                    .map_err(fault)?;
            }
            Ok(seq)
        });
        match written.and_then(|seq| db.conn.execute_batch("COMMIT").map_err(fault).map(|()| seq)) {
            Ok(seq) => {
                drop(db);
                self.wake.notify_one();
                Ok(Some(seq))
            }
            Err(refusal) => {
                let _ = db.conn.execute_batch("ROLLBACK");
                // The size was counted by insert; the rollback took the entry back out.
                db.bytes = db
                    .conn
                    .query_row("SELECT COALESCE(SUM(bytes), 0) FROM outbox", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .map(|b| b.max(0) as u64)
                    .unwrap_or(db.bytes);
                Err(refusal)
            }
        }
    }

    /// Forget what `scope` saw (a feed that was removed for good).
    pub fn forget(&self, scope: &str) {
        let db = self.db.lock().unwrap();
        let _ = db
            .conn
            .execute("DELETE FROM seen WHERE scope = ?1", [scope]);
    }

    /// How many keys `scope` remembers.
    pub fn seen_count(&self, scope: &str) -> u64 {
        let db = self.db.lock().unwrap();
        db.conn
            .query_row("SELECT COUNT(*) FROM seen WHERE scope = ?1", [scope], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n.max(0) as u64)
            .unwrap_or(0)
    }

    /// The stored form of one event, or why the platform could never take it.
    fn entry(e: &NewEvent) -> Result<String, Refusal> {
        if !text_ok(&e.space, 200) {
            return Err(Refusal {
                code: "EVENT_INVALID",
                message: "space is a space id (1 to 200 characters)".into(),
            });
        }
        match (&e.event, &e.table) {
            (Some(event), None) if valid_name(event) => {}
            (None, Some(table)) if valid_table(table) => {}
            (Some(event), None) => {
                return Err(Refusal {
                    code: "EVENT_INVALID",
                    message: format!(
                        "event {event:?} is not a declared event name (lower-case words joined by dots, e.g. job.done)"
                    ),
                });
            }
            (None, Some(table)) => {
                return Err(Refusal {
                    code: "EVENT_INVALID",
                    message: format!(
                        "table {table:?} is not a table name (a lower-case word of a-z, 0-9 and _)"
                    ),
                });
            }
            _ => {
                return Err(Refusal {
                    code: "EVENT_INVALID",
                    message: "an entry is either an event (event) or a row of a table (table), exactly one".into(),
                });
            }
        }
        if !e.payload.is_object() {
            return Err(Refusal {
                code: "EVENT_INVALID",
                message: format!(
                    "the payload of {} is an object of its declared fields",
                    what(e)
                ),
            });
        }
        for (field, v) in [
            ("instance", &e.instance),
            ("key", &e.key),
            ("cause", &e.cause),
        ] {
            if let Some(v) = v
                && !text_ok(v, 400)
            {
                return Err(Refusal {
                    code: "EVENT_INVALID",
                    message: format!("{field} is a text of 1 to 400 characters"),
                });
            }
        }
        let payload_len = e.payload.to_string().len();
        if payload_len > PAYLOAD_MAX {
            return Err(Refusal {
                code: "EVENT_TOO_LARGE",
                message: format!(
                    "the payload of {} in {} is {payload_len} bytes; an event carries at most {PAYLOAD_MAX} (put bulk data in a file and send its reference)",
                    what(e),
                    e.space
                ),
            });
        }
        let mut item = json!({"space": e.space, "payload": e.payload, "at": now_ms()});
        for (field, v) in [
            ("event", &e.event),
            ("table", &e.table),
            ("instance", &e.instance),
            ("key", &e.key),
            ("cause", &e.cause),
        ] {
            if let Some(v) = v {
                item[field] = json!(v);
            }
        }
        Ok(item.to_string())
    }

    /// Inserts one stored entry (inside the caller's lock): its seq, or `OUTBOX_FULL` / `OUTBOX_FAULT`.
    fn insert(&self, db: &mut Db, text: &str) -> Result<u64, Refusal> {
        let size = text.len() as u64;
        if db.bytes + size > self.cap {
            return Err(Refusal {
                code: "OUTBOX_FULL",
                message: format!(
                    "this computer's outbox holds {} of {} bytes waiting for the platform; producers pause until the link takes them",
                    db.bytes, self.cap
                ),
            });
        }
        db.conn
            .execute(
                "INSERT INTO outbox (item, bytes, at) VALUES (?1, ?2, ?3)",
                params![text, size as i64, now_ms()],
            )
            .map_err(|err| Refusal {
                code: "OUTBOX_FAULT",
                message: sql(err, &self.path),
            })?;
        let seq = db.conn.last_insert_rowid() as u64;
        db.bytes += size;
        Ok(seq)
    }

    /// The next `events` frame: entries after `after`, in seq order, within [`BATCH_ITEMS`] / [`BATCH_BYTES`]; None when
    /// nothing waits.
    pub fn batch(&self, after: u64) -> Option<Value> {
        let db = self.db.lock().unwrap();
        let mut stmt = db
            .conn
            .prepare_cached("SELECT seq, item FROM outbox WHERE seq > ?1 ORDER BY seq LIMIT ?2")
            .ok()?;
        let rows = stmt
            .query_map(params![after as i64, BATCH_ITEMS as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .ok()?;
        let mut items = Vec::new();
        let mut bytes = 0;
        for row in rows.flatten() {
            let (seq, text) = row;
            if !items.is_empty() && bytes + text.len() > BATCH_BYTES {
                break;
            }
            bytes += text.len();
            let mut item: Value = serde_json::from_str(&text).unwrap_or(json!({}));
            item["seq"] = json!(seq);
            item["id"] = json!(self.event_id(seq as u64));
            items.push(item);
        }
        (!items.is_empty()).then(|| json!({"op": "events", "host": self.producer, "items": items}))
    }

    /// The platform is done with every entry up to `upto`: they leave; the refused ones among them are kept in the
    /// bounded `refused` table. Returns how many entries left.
    pub fn ack(&self, upto: u64, refused: &[Refused]) -> usize {
        let mut db = self.db.lock().unwrap();
        let acked = db.acked.max(upto);
        let Ok(tx) = db.conn.transaction() else {
            return 0;
        };
        for r in refused.iter().filter(|r| r.seq <= upto) {
            let _ = tx.execute(
                "INSERT OR REPLACE INTO refused (seq, item, code, message, at)
                 SELECT ?1, (SELECT item FROM outbox WHERE seq = ?1), ?2, ?3, ?4",
                params![r.seq as i64, r.code, r.message, now_ms()],
            );
        }
        let _ = tx.execute(
            "DELETE FROM refused WHERE seq NOT IN (SELECT seq FROM refused ORDER BY seq DESC LIMIT ?1)",
            [REFUSED_KEEP],
        );
        let left_bytes: i64 = tx
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM outbox WHERE seq <= ?1",
                [upto as i64],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let left = tx
            .execute("DELETE FROM outbox WHERE seq <= ?1", [upto as i64])
            .unwrap_or(0);
        let _ = tx.execute(
            "INSERT OR REPLACE INTO meta (k, v) VALUES ('acked', ?1)",
            [acked.to_string()],
        );
        if tx.commit().is_ok() {
            db.bytes = db.bytes.saturating_sub(left_bytes.max(0) as u64);
            db.acked = acked;
            left
        } else {
            0
        }
    }

    /// What the outbox holds now.
    pub fn status(&self) -> Status {
        let db = self.db.lock().unwrap();
        let one = |q: &str| -> i64 { db.conn.query_row(q, [], |r| r.get(0)).unwrap_or(0) };
        let pending = one("SELECT COUNT(*) FROM outbox").max(0) as u64;
        let last_seq =
            one("SELECT COALESCE(MAX(seq), 0) FROM sqlite_sequence WHERE name = 'outbox'").max(0)
                as u64;
        let refused = one("SELECT COUNT(*) FROM refused").max(0) as u64;
        let last_refused = db
            .conn
            .query_row(
                "SELECT seq, code, message, item FROM refused ORDER BY seq DESC LIMIT 1",
                [],
                |r| {
                    let item: Option<String> = r.get(3)?;
                    let item: Value = item
                        .and_then(|t| serde_json::from_str(&t).ok())
                        .unwrap_or(Value::Null);
                    Ok(json!({"seq": r.get::<_, i64>(0)?, "code": r.get::<_, String>(1)?, "message": r.get::<_, String>(2)?,
                              "space": item.get("space"), "event": item.get("event"), "table": item.get("table")}))
                },
            )
            .optional()
            .ok()
            .flatten();
        Status {
            producer: self.producer.clone(),
            pending,
            bytes: db.bytes,
            cap: self.cap,
            full: db.bytes >= self.cap,
            last_seq,
            acked: db.acked,
            refused,
            last_refused,
        }
    }

    /// Whether any entry waits for the platform.
    pub fn has_pending(&self) -> bool {
        let db = self.db.lock().unwrap();
        db.conn
            .query_row("SELECT EXISTS(SELECT 1 FROM outbox)", [], |r| {
                r.get::<_, bool>(0)
            })
            .unwrap_or(false)
    }

    /// Whether a producer should pause: the outbox is at least 90 % full.
    pub fn nearly_full(&self) -> bool {
        let db = self.db.lock().unwrap();
        db.bytes.saturating_mul(10) >= self.cap.saturating_mul(9)
    }

    /// Resolves once an append happened since the last call (or right away when one is pending).
    pub async fn appended(&self) {
        self.wake.notified().await;
    }
}

/// Reads `events_ack.refused[]`.
pub fn parse_refused(v: Option<&Value>) -> Vec<Refused> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Refused {
                        seq: r.get("seq")?.as_u64()?,
                        code: r
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("HOST_EVENT_INVALID")
                            .chars()
                            .take(64)
                            .collect(),
                        message: r
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(500)
                            .collect(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
