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
    pub text: String,
}

/// Decide an ask: true to allow. `cancel` is set when nobody waits for the answer any more.
pub type Confirm = Arc<dyn Fn(&Ask, &AtomicBool) -> bool + Send + Sync>;

/// How long a question stays open before it counts as "no".
pub const ASK_TIMEOUT: Duration = Duration::from_secs(120);

pub const TITLE: &str = "AgentRouter 设备";

pub fn ask_text(ask: &Ask) -> String {
    let what = match ask.action {
        "exec" => "运行命令",
        "exec_full" => "运行命令（完全访问：允许后，这个对话之后的命令不再逐条询问）",
        "write" => "写入文件",
        "input" => "给正在运行的命令输入",
        other => other,
    };
    format!(
        "云端对话想在这台电脑上{what}：\n\n{}\n\n对话：{}\n\n确定：允许这一次\n取消：拒绝",
        crate::util::clip(&ask.text, 1500),
        ask.session
    )
}

/// The native dialog on Windows; elsewhere nobody can answer, so the answer is no.
pub fn native() -> Confirm {
    Arc::new(|ask: &Ask, cancel: &AtomicBool| question(TITLE, &ask_text(ask), cancel))
}

/// Unattended machines: the operator approved commands on this machine when linking it (`link
/// --unattended`); every request is still checked against the folders and audited.
pub fn preapproved() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| true)
}

/// Never allow (tests, and command-line mode without a person at the screen).
pub fn deny_all() -> Confirm {
    Arc::new(|_: &Ask, _: &AtomicBool| false)
}

/// A yes/no question in a native dialog (OK = yes, Cancel = no; Cancel is the default button). It closes
/// as "no" when `cancel` is set or after [`ASK_TIMEOUT`].
#[cfg(windows)]
pub fn question(title: &str, body: &str, cancel: &AtomicBool) -> bool {
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
                if cancel.load(Ordering::SeqCst) || started.elapsed() > ASK_TIMEOUT {
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
pub fn question(_title: &str, _body: &str, _cancel: &AtomicBool) -> bool {
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
