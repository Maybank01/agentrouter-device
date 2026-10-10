//! Questions in the terminal (the command line in the foreground, all systems alike): the full command
//! or the files, where, and which conversation; `y` this once, `a` the same kind in this conversation,
//! `n` no, `d` no and disconnect. One question at a time; a question withdrawn or expired meanwhile is
//! taken off the screen.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use agentrouter_device::consent::{Ask, Confirm, Decision, action_label};
use agentrouter_device::util::clip;

struct Item {
    ask: Ask,
    answer: Mutex<Option<Decision>>,
    answered: Condvar,
    withdrawn: AtomicBool,
}

struct Prompter {
    queue: Mutex<VecDeque<Arc<Item>>>,
    added: Condvar,
}

/// The terminal's `Confirm`. Reads standard input on its own thread; when input ends, nobody can
/// answer any more and every question gets `Unavailable`.
pub fn terminal() -> Confirm {
    let prompter = Arc::new(Prompter {
        queue: Mutex::new(VecDeque::new()),
        added: Condvar::new(),
    });
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(Some(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(None);
    });
    let looper = prompter.clone();
    std::thread::spawn(move || looper.run(rx));
    Arc::new(move |ask: &Ask, cancel: &AtomicBool| prompter.ask(ask, cancel))
}

pub fn parse(line: &str) -> Option<Decision> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "是" => Some(Decision::Once),
        "a" | "all" | "always" => Some(Decision::Session),
        "n" | "no" | "否" => Some(Decision::Deny),
        "d" | "disconnect" => Some(Decision::Disconnect),
        _ => None,
    }
}

fn question_text(ask: &Ask) -> String {
    let mut text = format!(
        "\n────────────────────────────────────────\n云端对话想在这台电脑上{}：\n",
        action_label(ask.action)
    );
    for line in clip(&ask.text, 4000).lines() {
        text.push_str("  ");
        text.push_str(line);
        text.push('\n');
    }
    if let Some(cwd) = &ask.cwd {
        text.push_str(&format!("位置：{cwd}\n"));
    }
    text.push_str(&format!("对话：{}\n", ask.session));
    let same = ask.kind.as_deref().unwrap_or("本对话里完全相同的请求");
    text.push_str(&format!(
        "[y] 允许这一次   [a] 允许{same}   [n] 拒绝   [d] 断开\n> "
    ));
    text
}

impl Prompter {
    fn ask(&self, ask: &Ask, cancel: &AtomicBool) -> Decision {
        let item = Arc::new(Item {
            ask: ask.clone(),
            answer: Mutex::new(None),
            answered: Condvar::new(),
            withdrawn: AtomicBool::new(false),
        });
        self.queue.lock().unwrap().push_back(item.clone());
        self.added.notify_all();
        let mut answer = item.answer.lock().unwrap();
        loop {
            if let Some(decision) = *answer {
                return decision;
            }
            if cancel.load(Ordering::SeqCst) {
                item.withdrawn.store(true, Ordering::SeqCst);
                return Decision::Deny;
            }
            answer = item
                .answered
                .wait_timeout(answer, Duration::from_millis(200))
                .unwrap()
                .0;
        }
    }

    fn settle(&self, item: &Item, decision: Decision) {
        *item.answer.lock().unwrap() = Some(decision);
        item.answered.notify_all();
        let mut queue = self.queue.lock().unwrap();
        queue.retain(|i| !std::ptr::eq(i.as_ref(), item));
    }

    fn next(&self) -> Arc<Item> {
        let mut queue = self.queue.lock().unwrap();
        loop {
            while queue
                .front()
                .is_some_and(|i| i.withdrawn.load(Ordering::SeqCst))
            {
                queue.pop_front();
            }
            if let Some(item) = queue.front() {
                return item.clone();
            }
            queue = self.added.wait(queue).unwrap();
        }
    }

    fn run(&self, rx: Receiver<Option<String>>) {
        let mut input_gone = false;
        loop {
            let item = self.next();
            if input_gone {
                self.settle(&item, Decision::Unavailable);
                continue;
            }
            print!("{}", question_text(&item.ask));
            let _ = std::io::stdout().flush();
            loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(Some(line)) => match parse(&line) {
                        Some(decision) => {
                            let said = match decision {
                                Decision::Once => "允许了这一次",
                                Decision::Session => "允许了，本对话同类的不再问",
                                Decision::Deny => "拒绝了",
                                Decision::Disconnect => "拒绝并断开",
                                Decision::Unavailable => "",
                            };
                            println!("  {said}");
                            self.settle(&item, decision);
                            break;
                        }
                        None => {
                            print!("请输入 y、a、n 或 d：");
                            let _ = std::io::stdout().flush();
                        }
                    },
                    Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                        input_gone = true;
                        println!("\n（终端输入已关闭，之后的请求都会被拒绝）");
                        self.settle(&item, Decision::Unavailable);
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if item.withdrawn.load(Ordering::SeqCst) {
                            println!("\n  （这个请求已撤回或过期）");
                            self.settle(&item, Decision::Deny);
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert_eq!(parse(" y "), Some(Decision::Once));
        assert_eq!(parse("A"), Some(Decision::Session));
        assert_eq!(parse("n"), Some(Decision::Deny));
        assert_eq!(parse("d"), Some(Decision::Disconnect));
        assert_eq!(parse("maybe"), None);
    }
}
