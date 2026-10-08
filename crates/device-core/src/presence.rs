//! Who is using this computer right now, and what they are doing (LINKED-DEVICES.md §17): the sessions
//! the "being controlled" bar lists, the activity line the device writes itself (never text from the
//! cloud), the pause switch, and the indicator gate: **no request is carried out unless the indicator
//! says it is on screen** (`INDICATOR_UNAVAILABLE`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::{Value, json};

use crate::protocol::DeviceError;
use crate::util::{clip, now_ms};

/// A session without a request or a running job for this long has ended (the bar disappears).
pub const IDLE_MS: i64 = 60_000;
/// The screen edge keeps glowing this long after the last command or write finished.
pub const GLOW_LINGER_MS: i64 = 2_000;

/// Who is behind a session, as the bar, the tray and the consent dialog show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Who {
    /// `conversation` (the owner's own AgentRouter conversation), `local` (an AI on this computer,
    /// through the local MCP), `remote` (someone the owner shared this computer with; later).
    pub via: &'static str,
    /// "你的对话", "Claude Code", "林小满"…
    pub name: String,
    /// The AI client, when known ("Claude Code", "ChatGPT").
    pub client: Option<String>,
    /// A short reference (a session id prefix), shown small.
    pub detail: String,
}

impl Who {
    /// The owner's own conversation (signed request v1: no title, no model).
    pub fn conversation(session: &str) -> Who {
        Who {
            via: "conversation",
            name: "你的对话".to_string(),
            client: None,
            detail: session.chars().take(12).collect(),
        }
    }

    /// An AI on this computer, by the name its MCP client reported (said to be, not proven).
    pub fn local(client: &str) -> Who {
        Who {
            via: "local",
            name: client.to_string(),
            client: Some(client.to_string()),
            detail: "本机 AI".to_string(),
        }
    }

    /// "你的对话" / "本机的 Claude Code": the subject of a sentence.
    pub fn title(&self) -> String {
        match self.via {
            "local" => format!("本机的 {}", self.name),
            _ => self.name.clone(),
        }
    }
}

/// Shows the "being controlled" indicator and reports whether it is on screen.
pub trait Indicator: Send + Sync {
    /// Bring the indicator up (or keep it up) for the current sessions; `false` when it is not visible.
    fn ensure(&self) -> bool;
}

/// The command line: the terminal itself is the indicator (each request is printed to the log).
pub struct Terminal;

impl Indicator for Terminal {
    fn ensure(&self) -> bool {
        true
    }
}

/// One session as the bar shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub session: String,
    pub who: Who,
    pub started_at: i64,
    pub last_at: i64,
    /// What the device is doing for it right now ("运行：dotnet build", "等你确认", "" when idle).
    pub activity: String,
    pub running_jobs: usize,
    pub waiting: bool,
}

struct Entry {
    who: Who,
    started: i64,
    last: i64,
    activity: String,
    busy: usize,
    waiting: usize,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Entry>,
    /// Sessions the person disconnected on the bar (refused until the app restarts).
    ended: HashSet<String>,
    busy: usize,
    last_busy_end: i64,
}

pub struct Presence {
    state: Mutex<State>,
    paused: AtomicBool,
    indicator: Arc<dyn Indicator>,
}

/// While alive, the session counts as working (glowing edge, activity line).
pub struct Busy<'a> {
    presence: &'a Presence,
    session: String,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        let mut s = self.presence.state.lock().unwrap();
        s.busy = s.busy.saturating_sub(1);
        s.last_busy_end = now_ms();
        if let Some(e) = s.sessions.get_mut(&self.session) {
            e.busy = e.busy.saturating_sub(1);
            e.last = now_ms();
            if e.busy == 0 {
                e.activity.clear();
            }
        }
    }
}

/// While alive, the session waits for the person (consent dialog open).
pub struct Waiting<'a> {
    presence: &'a Presence,
    session: String,
    before: String,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut s = self.presence.state.lock().unwrap();
        if let Some(e) = s.sessions.get_mut(&self.session) {
            e.waiting = e.waiting.saturating_sub(1);
            e.activity = std::mem::take(&mut self.before);
            e.last = now_ms();
        }
    }
}

impl Presence {
    pub fn new(indicator: Arc<dyn Indicator>) -> Presence {
        Presence {
            state: Mutex::new(State::default()),
            paused: AtomicBool::new(false),
            indicator,
        }
    }

    /// A request of `session` arrived: refuse it when the person paused or ended it, else note it.
    /// Returns whether the session is new (for the audit log).
    pub fn begin(&self, session: &str, who: &Who) -> Result<bool, DeviceError> {
        let mut s = self.state.lock().unwrap();
        if s.ended.contains(session) {
            return Err(DeviceError::new(
                "SESSION_ENDED",
                "这台电脑的主人断开了这个连接。",
            ));
        }
        if self.paused.load(Ordering::SeqCst) {
            return Err(DeviceError::new(
                "PAUSED",
                "这台电脑的主人暂停了 AI 的使用，等他点“继续”。已经在跑的命令不受影响。",
            ));
        }
        let now = now_ms();
        let fresh = !s.sessions.contains_key(session);
        let entry = s.sessions.entry(session.to_string()).or_insert(Entry {
            who: who.clone(),
            started: now,
            last: now,
            activity: String::new(),
            busy: 0,
            waiting: 0,
        });
        entry.last = now;
        entry.who = who.clone();
        Ok(fresh)
    }

    /// The indicator gate: nothing happens on this computer unless the indicator is on screen.
    pub fn ensure_shown(&self) -> Result<(), DeviceError> {
        if self.indicator.ensure() {
            Ok(())
        } else {
            Err(DeviceError::new(
                "INDICATOR_UNAVAILABLE",
                "这台电脑上“正在被控制”的提示条没能显示出来，所以什么也没做。请让用户检查 AgentRouter 是否正常运行。",
            ))
        }
    }

    /// The session starts doing something (`text` is written by the device: "运行：…", "写入：…").
    pub fn busy(&self, session: &str, text: &str) -> Busy<'_> {
        let mut s = self.state.lock().unwrap();
        s.busy += 1;
        if let Some(e) = s.sessions.get_mut(session) {
            e.busy += 1;
            e.activity = clip(text, 200);
            e.last = now_ms();
        }
        Busy {
            presence: self,
            session: session.to_string(),
        }
    }

    /// Set the activity line without counting as working (reads, looking at a job).
    pub fn note(&self, session: &str, text: &str) {
        let mut s = self.state.lock().unwrap();
        if let Some(e) = s.sessions.get_mut(session) {
            e.activity = clip(text, 200);
            e.last = now_ms();
        }
    }

    /// A request finished: nothing is going on for the session any more unless something still is
    /// (a refused or failed request must not leave its activity line behind).
    pub fn settle(&self, session: &str) {
        let mut s = self.state.lock().unwrap();
        if let Some(e) = s.sessions.get_mut(session)
            && e.busy == 0
            && e.waiting == 0
        {
            e.activity.clear();
        }
    }

    /// The session waits for the person to answer on this computer.
    pub fn waiting(&self, session: &str) -> Waiting<'_> {
        let mut s = self.state.lock().unwrap();
        let before = match s.sessions.get_mut(session) {
            Some(e) => {
                e.waiting += 1;
                std::mem::replace(&mut e.activity, "等你确认".to_string())
            }
            None => String::new(),
        };
        Waiting {
            presence: self,
            session: session.to_string(),
            before,
        }
    }

    /// The sessions using this computer now (`running` = sessions with a running job). Idle ones are dropped.
    pub fn snapshot(&self, running: &[String]) -> Vec<SessionView> {
        let now = now_ms();
        let mut s = self.state.lock().unwrap();
        s.sessions.retain(|id, e| {
            e.busy > 0 || e.waiting > 0 || running.contains(id) || now - e.last < IDLE_MS
        });
        let mut list: Vec<SessionView> = s
            .sessions
            .iter()
            .map(|(id, e)| SessionView {
                session: id.clone(),
                who: e.who.clone(),
                started_at: e.started,
                last_at: e.last,
                activity: e.activity.clone(),
                running_jobs: running.iter().filter(|r| *r == id).count(),
                waiting: e.waiting > 0,
            })
            .collect();
        list.sort_by_key(|v| v.started_at);
        list
    }

    /// Whether the screen edge should glow: a command or write is being carried out (or just was), or
    /// a job is running.
    pub fn glowing(&self, jobs_running: usize) -> bool {
        let s = self.state.lock().unwrap();
        jobs_running > 0 || s.busy > 0 || now_ms() - s.last_busy_end < GLOW_LINGER_MS
    }

    /// Someone waits for the person on this computer.
    pub fn anyone_waiting(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .sessions
            .values()
            .any(|e| e.waiting > 0)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// The person ended this session on the bar: it is refused from now on (until the app restarts).
    pub fn end(&self, session: &str) {
        let mut s = self.state.lock().unwrap();
        s.sessions.remove(session);
        s.ended.insert(session.to_string());
    }

    /// Forget every session (disconnect, revocation, quit); they may come back after a reconnect.
    pub fn clear(&self) {
        self.state.lock().unwrap().sessions.clear();
    }

    pub fn audit_entry(&self, event: &str, session: &str, who: &Who) -> Value {
        json!({"event": event, "session": session, "via": who.via, "client": who.client, "who": who.name})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Off;
    impl Indicator for Off {
        fn ensure(&self) -> bool {
            false
        }
    }

    #[test]
    fn sessions_pause_end_and_gate() {
        let p = Presence::new(Arc::new(Terminal));
        let who = Who::local("Claude Code");
        assert!(p.begin("s1", &who).unwrap());
        assert!(!p.begin("s1", &who).unwrap());
        {
            let _b = p.busy("s1", "运行：dir");
            assert!(p.glowing(0));
            let v = p.snapshot(&[]);
            assert_eq!(v[0].activity, "运行：dir");
            {
                let _w = p.waiting("s1");
                assert_eq!(p.snapshot(&[])[0].activity, "等你确认");
                assert!(p.anyone_waiting());
            }
            assert_eq!(p.snapshot(&[])[0].activity, "运行：dir");
        }
        assert_eq!(p.snapshot(&[])[0].activity, "");
        p.set_paused(true);
        assert_eq!(p.begin("s1", &who).unwrap_err().code, "PAUSED");
        p.set_paused(false);
        p.end("s1");
        assert_eq!(p.begin("s1", &who).unwrap_err().code, "SESSION_ENDED");
        assert!(p.snapshot(&[]).is_empty());
        assert!(p.ensure_shown().is_ok());
        let off = Presence::new(Arc::new(Off));
        assert_eq!(
            off.ensure_shown().unwrap_err().code,
            "INDICATOR_UNAVAILABLE"
        );
    }
}
