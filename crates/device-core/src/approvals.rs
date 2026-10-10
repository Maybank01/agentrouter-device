//! Pending local approvals (DEVICE-PROTOCOL.md §5.5). A request that needs a yes on this computer is
//! asked once; when nobody answers within the request's `approvalWait`, the cloud gets an
//! `awaiting_approval` value while the question stays open here for up to ten minutes. When the person
//! allows it, the device runs it then; the result is fetched with `job` on the `apv_…` id. The same
//! request arriving again while it waits joins the open question instead of asking twice.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::consent::{APPROVAL_TTL, Ask, Confirm, Decision};
use crate::protocol::DeviceError;
use crate::util::{now_ms, short_id};

/// What runs once the person allows the request.
pub type Run = Box<dyn FnOnce() -> Result<Value, DeviceError> + Send>;
/// Called once when the question is settled (audit, console, 同类允许, disconnect).
pub type Settle = Box<dyn FnOnce(&Pending, Decision) + Send>;

#[derive(Debug, Clone)]
pub enum Outcome {
    Waiting,
    Running,
    Done(Result<Value, DeviceError>),
    Denied { disconnect: bool },
    Unavailable,
    Expired,
    Withdrawn,
}

impl Outcome {
    fn open(&self) -> bool {
        matches!(self, Outcome::Waiting | Outcome::Running)
    }
}

pub struct Pending {
    pub id: String,
    pub session: String,
    pub action: &'static str,
    pub text: String,
    pub expires_at: i64,
    key: String,
    state: Mutex<(Outcome, Option<Instant>)>,
    changed: Condvar,
    /// Closes the open question (withdrawn, expired, everything stopped).
    cancel: AtomicBool,
    withdrawn: AtomicBool,
}

impl Pending {
    pub fn outcome(&self) -> Outcome {
        self.state.lock().unwrap().0.clone()
    }

    fn set(&self, outcome: Outcome) {
        let mut state = self.state.lock().unwrap();
        let settled = !outcome.open();
        state.0 = outcome;
        if settled {
            state.1 = Some(Instant::now());
        }
        self.changed.notify_all();
    }

    /// Wait until the question is settled and anything it allowed has run, `max` passes, or `cancel` is set.
    pub fn wait(&self, max: Duration, cancel: &AtomicBool) {
        let deadline = Instant::now() + max;
        let mut state = self.state.lock().unwrap();
        while state.0.open() && !cancel.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let slice = (deadline - now).min(Duration::from_millis(100));
            state = self.changed.wait_timeout(state, slice).unwrap().0;
        }
    }

    /// The protocol action this question is about.
    fn protocol_action(&self) -> &'static str {
        match self.action {
            "exec_full" | "exec_beyond" => "exec",
            "input" => "job",
            other => other,
        }
    }

    /// The `awaiting_approval` value the cloud gets while the question is open.
    pub fn awaiting(&self) -> Value {
        json!({
            "status": "awaiting_approval",
            "job": self.id,
            "approval": self.id,
            "action": self.protocol_action(),
            "text": crate::util::clip(&self.text, 300),
            "expiresAt": self.expires_at,
            "message": "已在用户电脑上请求确认。不要重发同一个请求；可以先做别的，或用 job wait 等它。",
        })
    }

    pub fn summary(&self) -> Value {
        json!({"approval": self.id, "action": self.protocol_action(), "text": crate::util::clip(&self.text, 300), "expiresAt": self.expires_at})
    }

    /// The answer for the cloud once the wait is over (the action's own result when it ran).
    pub fn answer(&self) -> Result<Value, DeviceError> {
        match self.outcome() {
            Outcome::Waiting | Outcome::Running => Ok(self.awaiting()),
            Outcome::Done(Ok(mut value)) => {
                if let Some(map) = value.as_object_mut() {
                    map.insert("approval".into(), json!(self.id));
                }
                Ok(value)
            }
            Outcome::Done(Err(e)) => Err(e),
            Outcome::Denied { disconnect } => Err(DeviceError::new(
                "DENIED_BY_USER",
                if disconnect {
                    "用户在设备上拒绝了，并断开了这台设备"
                } else {
                    "用户在设备上拒绝了"
                },
            )
            .next("不要换个写法重试；问用户为什么拒绝。")),
            Outcome::Unavailable => Err(DeviceError::denied(
                "需要在电脑上确认：小助手在后台运行，这台电脑上没有人能点允许",
            )
            .next("请用户在这台电脑的终端里运行 agentrouter link（前台运行时会在终端里问），或者在电脑上把访问级别调高。")),
            Outcome::Expired => Err(DeviceError::new(
                "APPROVAL_EXPIRED",
                "10 分钟内没有人在设备上确认",
            )
            .next("问用户还要不要做；要的话再发一次，并提醒用户去电脑上点允许。")),
            Outcome::Withdrawn => Ok(json!({"job": self.id, "approval": self.id, "status": "withdrawn"})),
        }
    }
}

#[derive(Default)]
pub struct Approvals {
    pending: Mutex<HashMap<String, Arc<Pending>>>,
}

impl Approvals {
    fn prune(&self) {
        let now = Instant::now();
        self.pending.lock().unwrap().retain(|_, p| {
            let state = p.state.lock().unwrap();
            state.0.open()
                || state
                    .1
                    .is_none_or(|settled| now.duration_since(settled) < APPROVAL_TTL)
        });
    }

    /// The approval `id`, if it belongs to this conversation.
    pub fn get(&self, id: &str, session: &str) -> Option<Arc<Pending>> {
        self.prune();
        self.pending
            .lock()
            .unwrap()
            .get(id)
            .filter(|p| p.session == session)
            .cloned()
    }

    /// The questions still open for a conversation.
    pub fn open_for(&self, session: &str) -> Vec<Value> {
        self.prune();
        let pending = self.pending.lock().unwrap();
        let mut list: Vec<&Arc<Pending>> = pending
            .values()
            .filter(|p| p.session == session && matches!(p.outcome(), Outcome::Waiting))
            .collect();
        list.sort_by_key(|p| p.expires_at);
        list.iter().map(|p| p.summary()).collect()
    }

    /// Ask (or join the same open question). `key` identifies identical requests.
    pub fn request(
        &self,
        confirm: &Confirm,
        ask: Ask,
        key: String,
        run: Run,
        settle: Settle,
    ) -> Arc<Pending> {
        self.prune();
        let mut pending = self.pending.lock().unwrap();
        if let Some(open) = pending
            .values()
            .find(|p| p.key == key && p.outcome().open())
        {
            return open.clone();
        }
        let p = Arc::new(Pending {
            id: short_id("apv_"),
            session: ask.session.clone(),
            action: ask.action,
            text: ask.text.clone(),
            expires_at: now_ms() + APPROVAL_TTL.as_millis() as i64,
            key,
            state: Mutex::new((Outcome::Waiting, None)),
            changed: Condvar::new(),
            cancel: AtomicBool::new(false),
            withdrawn: AtomicBool::new(false),
        });
        pending.insert(p.id.clone(), p.clone());
        drop(pending);
        // Expiry: close the question after ten minutes.
        let timer = p.clone();
        std::thread::spawn(move || {
            timer.wait(APPROVAL_TTL, &AtomicBool::new(false));
            if matches!(timer.outcome(), Outcome::Waiting) {
                timer.cancel.store(true, Ordering::SeqCst);
            }
        });
        let asker = p.clone();
        let confirm = confirm.clone();
        std::thread::spawn(move || {
            let decision = confirm(&ask, &asker.cancel);
            let closed = asker.cancel.load(Ordering::SeqCst);
            let outcome = if asker.withdrawn.load(Ordering::SeqCst) {
                Outcome::Withdrawn
            } else if closed && !decision.allows() {
                Outcome::Expired
            } else {
                match decision {
                    Decision::Once | Decision::Session => {
                        asker.set(Outcome::Running);
                        Outcome::Done(run())
                    }
                    Decision::Deny => Outcome::Denied { disconnect: false },
                    Decision::Disconnect => Outcome::Denied { disconnect: true },
                    Decision::Unavailable => Outcome::Unavailable,
                }
            };
            asker.set(outcome);
            settle(&asker, decision);
        });
        p
    }

    /// Withdraw an open question (`job kill` on its id): the local prompt closes.
    pub fn withdraw(&self, p: &Pending) {
        if matches!(p.outcome(), Outcome::Waiting) {
            p.withdrawn.store(true, Ordering::SeqCst);
            p.cancel.store(true, Ordering::SeqCst);
            let give_up = Instant::now() + Duration::from_secs(5);
            let mut state = p.state.lock().unwrap();
            while matches!(state.0, Outcome::Waiting) && Instant::now() < give_up {
                state = p
                    .changed
                    .wait_timeout(state, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }
    }

    /// Close every open question (disconnect, quit, access change).
    pub fn withdraw_all(&self) {
        let open: Vec<Arc<Pending>> = self.pending.lock().unwrap().values().cloned().collect();
        for p in open {
            if matches!(p.outcome(), Outcome::Waiting) {
                p.withdrawn.store(true, Ordering::SeqCst);
                p.cancel.store(true, Ordering::SeqCst);
            }
        }
    }
}
