//! `changes`: what changed in the linked folders, so a bound session can drop its cached reads (PROJECT-SERVICE
//! §10.6 "Latency"). The OS watcher (inotify, FSEvents, ReadDirectoryChangesW through `notify`) starts with the first
//! call and records changed paths under a growing cursor; a call answers what happened after the cursor it passes
//! and can wait for the next change (a long poll, so a client keeps one call open instead of polling). Pull, not
//! push: it needs nothing new on the link, and a cursor survives a reconnect (a gap shows as `overflow`).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::{Value, json};

use super::{Call, Host, shown, tool};

/// Changes remembered; a cursor older than these answers `overflow` (drop everything cached).
const KEEP: usize = 20_000;
const ANSWER_MAX: u64 = 5_000;
const WAIT_MAX_MS: u64 = 30_000;
/// After the first change of a long poll, more changes join the same answer until it is quiet this long (at most
/// five times this in all).
const BURST: Duration = Duration::from_millis(50);

pub(super) fn tools() -> Vec<Value> {
    vec![tool(
        "changes",
        "Paths changed in the linked folders after `cursor` (create/modify/remove/rename), to invalidate cached reads. The first call (no cursor) starts watching and returns the cursor to pass next time. wait_ms (≤ 30000) waits for a change first. overflow = changes were missed: drop every cached read.",
        json!({"type": "object", "properties": {
            "cursor": {"type": "integer", "minimum": 0},
            "wait_ms": {"type": "integer", "minimum": 0, "maximum": WAIT_MAX_MS},
            "max": {"type": "integer", "minimum": 1, "maximum": ANSWER_MAX},
        }}),
        true,
    )]
}

#[derive(Default)]
struct Log {
    /// `(seq, path, kind)`, oldest first.
    events: VecDeque<(u64, PathBuf, &'static str)>,
    /// The seq of the newest change (0: none yet).
    last: u64,
    /// The highest cursor any answer has handed out: a change at or below it may already be known to a client, so a
    /// later change of the same path is a new change, never folded into it (Cloud #2307: folded, it was never reported
    /// and a cache kept the old content).
    answered: u64,
    /// The watcher's own failure (e.g. too many watches), once.
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct Changes {
    log: Arc<(Mutex<Log>, Condvar)>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// The folders watched (and those that could not be).
    watching: Mutex<Vec<(PathBuf, Option<String>)>>,
}

fn kind(kind: &EventKind) -> Option<&'static str> {
    use notify::event::ModifyKind;
    Some(match kind {
        EventKind::Access(_) => return None,
        EventKind::Create(_) => "create",
        EventKind::Remove(_) => "remove",
        EventKind::Modify(ModifyKind::Name(_)) => "rename",
        EventKind::Modify(ModifyKind::Metadata(_)) => "metadata",
        EventKind::Modify(_) => "modify",
        EventKind::Any | EventKind::Other => "any",
    })
}

impl Changes {
    /// Start watching `folders` (once).
    fn start(&self, folders: &[PathBuf]) {
        let mut slot = self.watcher.lock().unwrap();
        if slot.is_some() {
            return;
        }
        let log = self.log.clone();
        let made =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let (lock, changed) = &*log;
                let mut log = lock.lock().unwrap();
                match res {
                    Ok(event) => {
                        let Some(kind) = kind(&event.kind) else {
                            return;
                        };
                        for path in event.paths {
                            // The same path changing again right away is one change — unless an answer already covered the
                            // first one.
                            let answered = log.answered;
                            if log.events.back().is_some_and(|(seq, p, k)| {
                                *seq > answered && *p == path && *k == kind
                            }) {
                                continue;
                            }
                            log.last += 1;
                            let seq = log.last;
                            log.events.push_back((seq, path, kind));
                            if log.events.len() > KEEP {
                                log.events.pop_front();
                            }
                        }
                    }
                    Err(e) => {
                        // A lost event stream (overflowed kernel queue): every cursor is behind now.
                        log.error = Some(e.to_string());
                        log.last += 1;
                        let seq = log.last;
                        log.events.clear();
                        log.events.push_back((seq, PathBuf::new(), "overflow"));
                    }
                }
                changed.notify_all();
            });
        let mut watcher = match made {
            Ok(w) => w,
            Err(e) => {
                self.log.0.lock().unwrap().error = Some(e.to_string());
                return;
            }
        };
        let mut watching = self.watching.lock().unwrap();
        for folder in folders {
            let result = watcher
                .watch(folder, RecursiveMode::Recursive)
                .err()
                .map(|e| e.to_string());
            watching.push((folder.clone(), result));
        }
        *slot = Some(watcher);
    }

    pub(super) fn stop(&self) {
        self.watcher.lock().unwrap().take();
    }
}

impl Host {
    pub(super) fn changes(&self, args: &Value, call: &Call) -> Result<Value, String> {
        self.changes.start(&self.folders);
        let max = args
            .get("max")
            .and_then(Value::as_u64)
            .unwrap_or(ANSWER_MAX)
            .clamp(1, ANSWER_MAX) as usize;
        let until = Instant::now()
            + Duration::from_millis(
                args.get("wait_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(WAIT_MAX_MS),
            );
        let (lock, changed) = &*self.changes.log;
        let mut log = lock.lock().unwrap();
        let Some(cursor) = args.get("cursor").and_then(Value::as_u64) else {
            // The first call: the baseline.
            log.answered = log.answered.max(log.last);
            return Ok(self.changes_answer(log.last, Vec::new(), false, log.error.clone()));
        };
        let mut waited = false;
        while log.last <= cursor && !call.cancelled() {
            let now = Instant::now();
            if now >= until {
                break;
            }
            waited = true;
            log = changed
                .wait_timeout(log, (until - now).min(Duration::from_millis(100)))
                .unwrap()
                .0;
        }
        // Woken by a change: a save is several events (create, write, rename); answer them together.
        if waited && log.last > cursor {
            let burst_end = Instant::now() + BURST * 5;
            while Instant::now() < burst_end {
                let (next, quiet) = changed.wait_timeout(log, BURST).unwrap();
                log = next;
                if quiet.timed_out() {
                    break;
                }
            }
        }
        // A cursor from before what is kept (or from another server run) missed changes.
        let oldest = log.events.front().map(|e| e.0).unwrap_or(log.last + 1);
        let mut overflow = cursor > log.last || (cursor + 1 < oldest && cursor < log.last);
        let mut out = Vec::new();
        let mut next = cursor.min(log.last);
        for (seq, path, kind) in log.events.iter().filter(|e| e.0 > cursor) {
            if *kind == "overflow" {
                overflow = true;
                next = *seq;
                continue;
            }
            if out.len() >= max {
                break;
            }
            out.push(json!({"path": shown(path), "kind": kind}));
            next = *seq;
        }
        if overflow {
            next = log.last;
        }
        log.answered = log.answered.max(next);
        Ok(self.changes_answer(next, out, overflow, log.error.clone()))
    }

    fn changes_answer(
        &self,
        cursor: u64,
        changes: Vec<Value>,
        overflow: bool,
        error: Option<String>,
    ) -> Value {
        let watching: Vec<Value> = self
            .changes
            .watching
            .lock()
            .unwrap()
            .iter()
            .map(|(f, e)| json!({"folder": shown(f), "ok": e.is_none(), "error": e}))
            .collect();
        json!({"cursor": cursor, "changes": changes, "overflow": overflow, "watching": watching, "error": error})
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};
    use super::*;

    #[test]
    fn a_write_in_a_linked_folder_shows_up_after_the_cursor() {
        let dir = temp("watch");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let host = host_in(&dir);
        let call = Call::detached();
        let first = host.changes(&json!({}), &call).unwrap();
        assert_eq!(first["watching"][0]["ok"], true, "{first}");
        let cursor = first["cursor"].as_u64().unwrap();
        std::fs::write(dir.join("src").join("a.txt"), "x").unwrap();
        let mut seen = Vec::new();
        let mut at = cursor;
        let until = Instant::now() + Duration::from_secs(20);
        while !seen.iter().any(|p: &String| p.ends_with("a.txt")) && Instant::now() < until {
            let r = host
                .changes(&json!({"cursor": at, "wait_ms": 2000}), &call)
                .unwrap();
            assert_eq!(r["overflow"], false);
            at = r["cursor"].as_u64().unwrap();
            seen.extend(
                r["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["path"].as_str().unwrap().to_string()),
            );
        }
        assert!(seen.iter().any(|p| p.ends_with("a.txt")), "{seen:?}");
        assert!(at > cursor);
        // The same file changed again after the client saw its first change: reported again, not folded into it.
        std::fs::write(dir.join("src").join("a.txt"), "y").unwrap();
        let mut again = false;
        let until = Instant::now() + Duration::from_secs(20);
        while !again && Instant::now() < until {
            let r = host
                .changes(&json!({"cursor": at, "wait_ms": 2000}), &call)
                .unwrap();
            at = r["cursor"].as_u64().unwrap();
            again = r["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"].as_str().unwrap().ends_with("a.txt"));
        }
        assert!(again, "a second write to a.txt was not reported");
        // A cursor from the future (another server run) is an overflow.
        let future = host.changes(&json!({"cursor": at + 1000}), &call).unwrap();
        assert_eq!(future["overflow"], true);
        host.changes.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
