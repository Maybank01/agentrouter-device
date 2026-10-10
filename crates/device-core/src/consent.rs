//! Local confirmation. It is only ever asked on this device, never on the web: a cloud that was
//! impersonated or broken into could fake a web "yes".

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::Ordering;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

/// What a request wants the person to approve.
#[derive(Debug, Clone)]
pub struct Ask {
    pub session: String,
    pub action: &'static str,
    /// The full command, or the files a write changes.
    pub text: String,
    /// Where it runs (commands) or the folder it writes in.
    pub cwd: Option<String>,
    /// What 「本对话同类允许」 would allow, in words (`None`: only this exact request).
    pub kind: Option<String>,
}

/// The person's answer: `y` this once, `a` the same kind for the rest of this conversation, `n` no,
/// `d` no and disconnect. `Unavailable`: nobody can answer here (a background helper on macOS/Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    Session,
    Deny,
    Disconnect,
    Unavailable,
}

impl Decision {
    pub fn allows(self) -> bool {
        matches!(self, Decision::Once | Decision::Session)
    }
}

impl From<bool> for Decision {
    fn from(yes: bool) -> Self {
        if yes { Decision::Once } else { Decision::Deny }
    }
}

/// Decide an ask. `cancel` is set when the question is withdrawn or expires (then answer `Deny`).
pub type Confirm = Arc<dyn Fn(&Ask, &AtomicBool) -> Decision + Send + Sync>;

/// How long a plain yes/no question stays open before it counts as "no".
pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);

pub const TITLE: &str = "AgentRouter 设备";

pub fn action_label(action: &str) -> &str {
    match action {
        "exec" => "运行命令",
        "exec_full" => "运行命令（完全访问：允许后，这个对话之后的命令不再逐条询问）",
        "exec_beyond" => "运行一条超出链接文件夹的命令",
        "write" | "write_file" => "写入文件",
        "edit_file" => "修改文件",
        "apply_patch" => "按补丁修改文件",
        "input" => "给正在运行的命令输入",
        other => other,
    }
}

pub fn ask_text(ask: &Ask) -> String {
    let place = ask
        .cwd
        .as_deref()
        .map(|c| {
            format!(
                "

位置：{c}"
            )
        })
        .unwrap_or_default();
    format!(
        "云端对话想在这台电脑上{}：

{}{place}

对话：{}

确定：允许这一次
取消：拒绝",
        action_label(ask.action),
        crate::util::clip(&ask.text, 1500),
        ask.session
    )
}

/// The native dialog on Windows (OK = this once, Cancel = no); elsewhere nobody can answer.
pub fn native() -> Confirm {
    Arc::new(|ask: &Ask, cancel: &AtomicBool| {
        if cfg!(windows) {
            question_for(TITLE, &ask_text(ask), cancel, APPROVAL_TTL).into()
        } else {
            Decision::Unavailable
        }
    })
}

/// How long a pending approval waits for the person (DEVICE-PROTOCOL.md §5.5).
pub const APPROVAL_TTL: Duration = Duration::from_secs(600);

/// Background helper on macOS/Linux: no window to ask in, so nothing that needs a yes runs.
pub fn unavailable() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| Decision::Unavailable)
}

/// Unattended machines: the operator approved commands on this machine when linking it (`link
/// --unattended`); every request is still checked against the folders and audited.
pub fn preapproved() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| Decision::Once)
}

/// Never allow (tests).
pub fn deny_all() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| Decision::Deny)
}

/// A yes/no question in a native dialog that closes as "no" after [`ASK_TIMEOUT`].
pub fn question(title: &str, body: &str, cancel: &AtomicBool) -> bool {
    question_for(title, body, cancel, ASK_TIMEOUT)
}

/// A yes/no question in a native dialog (OK = yes, Cancel = no; Cancel is the default button). It closes
/// as "no" when `cancel` is set or after `max`.
#[cfg(windows)]
pub fn question_for(title: &str, body: &str, cancel: &AtomicBool, max: Duration) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IDOK, MB_DEFBUTTON2, MB_ICONWARNING, MB_OKCANCEL, MB_SETFOREGROUND,
        MB_TOPMOST, MessageBoxW, PostMessageW, WM_CLOSE,
    };
    // A title nobody else uses, so the dialog can be found and closed.
    let unique = format!("{title} · {}", &crate::util::short_id("")[..6]);
    let (tx, rx) = std::sync::mpsc::channel();
    let (t, b) = (wide(&unique), wide(body));
    std::thread::spawn(move || {
        // SAFETY: the strings are NUL-terminated UTF-16 that outlive the call.
        let answer = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                b.as_ptr(),
                t.as_ptr(),
                MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND,
            )
        };
        let _ = tx.send(answer == IDOK);
    });
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(answer) => return answer,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(_) => {
                if cancel.load(Ordering::SeqCst) || started.elapsed() > max {
                    let t = wide(&unique);
                    // SAFETY: closing the dialog we opened, found by its unique title.
                    unsafe {
                        let hwnd = FindWindowW(std::ptr::null(), t.as_ptr());
                        if !hwnd.is_null() {
                            PostMessageW(hwnd, WM_CLOSE, 0, 0);
                        }
                    }
                    return false;
                }
            }
        }
    }
}

#[cfg(not(windows))]
pub fn question_for(_title: &str, _body: &str, _cancel: &AtomicBool, _max: Duration) -> bool {
    false
}

/// An information dialog that does not block the caller.
#[cfg(windows)]
pub fn inform(title: &str, body: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW,
    };
    let (t, b) = (wide(title), wide(body));
    std::thread::spawn(move || {
        // SAFETY: NUL-terminated UTF-16 strings owned by this thread.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                b.as_ptr(),
                t.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
            );
        }
    });
}

#[cfg(not(windows))]
pub fn inform(title: &str, body: &str) {
    eprintln!("{title}\n{body}");
}

#[cfg(windows)]
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
