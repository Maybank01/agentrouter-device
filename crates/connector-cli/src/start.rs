//! Double-click start (Cloud #2383 D1, owner 2026-10-21 「我想直接双击运行」): `agentrouter.exe` opened from
//! Explorer (no arguments, a console of its own) or `agentrouter start`. The same console link as `agentrouter link`
//! (`console.rs`), with the choices made for the person:
//! - the working folder comes from the native folder picker once (default: a new `AgentRouter` folder in Documents)
//!   and is remembered (`Config::start_folder`); the next start reuses it, Enter within a few seconds picks again;
//! - the device name is the computer's name (`Config::name`);
//! - the first time, the browser opens the pairing page with the code filled in, so the person only confirms there;
//!   the paired identity stays in the key store (no expiry), so later starts connect without pairing;
//! - folder mode on every OS: the local tools work only in that folder, inside the OS sandbox (owner 2026-10-10
//!   「folder-scoped full freedom」; the test builds used 完全访问 only while the Windows folder sandbox did not work,
//!   device #66/#112). 完全访问 stays the person's explicit choice (`agentrouter start --full`), said in one line.
//!
//! Started from a terminal with arguments, nothing here runs (`cli.rs`).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use agentrouter_device::config::Config;
use agentrouter_device::util::{data_dir, home_dir};

/// How long a remembered folder waits for Enter (= choose another) before the link starts.
const CHANGE_FOR: Duration = Duration::from_secs(3);

fn line(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// True when this process has a console of its own: Windows created it because the program was started from
/// Explorer (double-click), not from a terminal (cmd, PowerShell, npm keep their own process on the console).
#[cfg(windows)]
pub fn own_console() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    let mut ids = [0u32; 2];
    // SAFETY: the buffer holds `ids.len()` process ids; the call writes at most that many.
    let attached = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    attached == 1
}

#[cfg(not(windows))]
pub fn own_console() -> bool {
    false
}

/// The person's Documents folder (Windows: the known folder, which may live in OneDrive).
#[cfg(windows)]
fn documents_dir() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: on success `raw` is a NUL-terminated wide string the shell allocated; it is read up to the NUL and
    // freed with CoTaskMemFree exactly once (also on failure, where it may be null, which CoTaskMemFree accepts).
    let path = unsafe {
        let hr = SHGetKnownFolderPath(
            &FOLDERID_Documents,
            KF_FLAG_DEFAULT as _,
            std::ptr::null_mut(),
            &mut raw,
        );
        let path = (hr == 0 && !raw.is_null()).then(|| {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            PathBuf::from(std::ffi::OsString::from_wide(std::slice::from_raw_parts(
                raw, len,
            )))
        });
        CoTaskMemFree(raw as *const _);
        path
    };
    path.unwrap_or_else(|| home_dir().join("Documents"))
}

#[cfg(not(windows))]
fn documents_dir() -> PathBuf {
    home_dir().join("Documents")
}

/// The folder offered first: `Documents\AgentRouter`, created if missing.
fn default_folder() -> PathBuf {
    let folder = documents_dir().join("AgentRouter");
    let _ = std::fs::create_dir_all(&folder);
    folder
}

/// The native folder picker (Windows: the shell's IFileOpenDialog in folder mode, through `rfd`). None = cancelled.
#[cfg(windows)]
fn pick_folder(start: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("选一个文件夹给 AgentRouter 用")
        .set_directory(start)
        .pick_folder()
}

/// No picker outside Windows (nobody double-clicks there yet): the default folder.
#[cfg(not(windows))]
fn pick_folder(_start: &Path) -> Option<PathBuf> {
    None
}

/// Enter typed within `CHANGE_FOR` (only when there is a terminal to type in).
fn change_requested() -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let lines = crate::console::stdin_lines().lock().unwrap();
    while lines.try_recv().is_ok() {}
    lines.recv_timeout(CHANGE_FOR).is_ok()
}

/// The working folder: the remembered one, or one chosen in the picker (then remembered).
fn working_folder(cfg: &mut Config) -> Result<PathBuf, String> {
    let data = data_dir();
    if let Some(saved) = cfg.start_folder.clone()
        && let Ok(real) = agentrouter_aggregator::check_folder(Path::new(&saved), &data)
    {
        line(&format!(
            "  工作文件夹  {}（要换一个，{} 秒内按回车）",
            shown(&real),
            CHANGE_FOR.as_secs()
        ));
        if !change_requested() {
            return Ok(real);
        }
    }
    loop {
        let default = default_folder();
        line(&format!(
            "  工作文件夹  请在弹出的窗口里选一个（直接点「选择文件夹」就用 {}）",
            shown(&default)
        ));
        let (chosen, picked) = match pick_folder(&default) {
            Some(p) => (p, true),
            None => (default, false),
        };
        match agentrouter_aggregator::check_folder(&chosen, &data) {
            Ok(real) => {
                line(&format!("  工作文件夹  {}", shown(&real)));
                cfg.start_folder = Some(shown(&real));
                if let Err(e) = cfg.save() {
                    line(&format!("  （没能记住这个文件夹，下次还会问：{e}）"));
                }
                return Ok(real);
            }
            // Only a folder the person picked can be wrong (a drive root, the home folder…): pick again.
            Err(e) if picked => line(&format!("  {e}")),
            Err(e) => return Err(e),
        }
    }
}

/// Open the pairing page in the default browser. Only a web page is ever opened (`link::safe_url`).
pub fn open_page(url: &str) -> bool {
    agentrouter_device::link::safe_url(url) && open::that_detached(url).is_ok()
}

/// What the console says (and does) while the person pairs this computer in the browser.
pub fn show_pairing(pairing: &agentrouter_device::net::Pairing) {
    let minutes = pairing.expires_in / 60;
    let (url, prefilled) = match &pairing.verify_url_complete {
        Some(u) if agentrouter_device::link::safe_url(u) => (u.as_str(), true),
        _ => (pairing.verify_url.as_str(), false),
    };
    if prefilled {
        line(&format!(
            "  确认        第一次使用：浏览器里会打开「我的设备」，配对码 {} 已经填好，核对一下点「链接」（{minutes} 分钟内有效）",
            pairing.code
        ));
    } else {
        line(&format!(
            "  确认        第一次使用：浏览器里会打开「我的设备」，点「链接新设备」，输入配对码 {}（{minutes} 分钟内有效）",
            pairing.code
        ));
    }
    if !open_page(url) {
        line(&format!(
            "              浏览器没有自动打开：请自己打开 {url}"
        ));
    }
}

/// Double-click start: choose (or reuse) the folder, then the console link (folder mode; `full` = 完全访问).
pub fn run(full: bool, interrupted: &'static AtomicBool) -> i32 {
    let code = start(full, interrupted);
    // A window Windows opened for us closes with the process: keep it until the person has read why it stopped.
    if code != 0 && own_console() {
        line("\n按回车关闭这个窗口。");
        let _ = crate::console::stdin_lines()
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(600));
    }
    code
}

fn start(full: bool, interrupted: &'static AtomicBool) -> i32 {
    line("AgentRouter · 把这台电脑连到 AgentRouter（测试版）");
    let mut cfg = Config::load();
    let folder = match working_folder(&mut cfg) {
        Ok(f) => f,
        Err(e) => {
            line(&format!("没有可用的工作文件夹：{e}"));
            return 2;
        }
    };
    line(&format!("  这台电脑    {}", cfg.name));
    line(if full {
        "  注意        窗口开着的时候，云端 Agent 能读写这台电脑上的所有文件（完全访问）；不用的时候关掉这个窗口。"
    } else {
        "  访问        云端 Agent 只能在这个文件夹里干活，文件夹外的改动由系统拦下；关掉窗口就断开。"
    });
    crate::console::link(
        crate::console::LinkOptions {
            dirs: vec![shown(&folder)],
            read_only: false,
            full,
            yes_servers: false,
            gateway: None,
            name: None,
            double_click: true,
        },
        interrupted,
    )
}
