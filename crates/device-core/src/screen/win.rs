//! Windows: capture the screen (GDI from the composed desktop, per-monitor DPI aware so every
//! coordinate is a physical pixel), list top-level windows, send mouse and keyboard input with
//! `SendInput`, launch and close GUI programs. No hooks, no injection into other processes
//! (LINKED-DEVICES.md §20.3, §20.6).

use std::ffi::c_void;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetDIBits, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    TerminateProcess,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetLastInputInfo, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, LASTINPUTINFO, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VkKeyScanW,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ES_PASSWORD, EnumWindows, GA_ROOT, GUITHREADINFO, GW_OWNER, GWL_EXSTYLE, GWL_STYLE,
    GetAncestor, GetForegroundWindow, GetGUIThreadInfo, GetSystemMetrics, GetWindow,
    GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, PostMessageW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, SW_SHOWNORMAL, SetCursorPos,
    SetForegroundWindow, ShowWindow, WM_CLOSE, WS_EX_TOOLWINDOW, WindowFromPoint,
};

use super::image::Frame;
use super::{Display, Rect, Window};

static DPI: Once = Once::new();
/// The tick of the last input this process sent (to tell the person's own input apart).
static OURS: AtomicU32 = AtomicU32::new(0);

/// Physical pixels everywhere (per-monitor DPI awareness v2), once per process.
pub fn init() {
    DPI.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
    // Threads that started before the process setting keep their own: set it for this one too.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// False while the secure desktop (UAC, lock screen, Ctrl+Alt+Del) has the input: nothing we send reaches it.
pub fn input_desktop_available() -> bool {
    unsafe {
        let desk = OpenInputDesktop(0, 0, 0x0100 /* DESKTOP_SWITCHDESKTOP */);
        if desk.is_null() {
            return false;
        }
        CloseDesktop(desk);
        true
    }
}

pub fn virtual_screen() -> Rect {
    init();
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN) as i64,
            y: GetSystemMetrics(SM_YVIRTUALSCREEN) as i64,
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN) as i64,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN) as i64,
        }
    }
}

unsafe extern "system" fn monitor_cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> i32 {
    let list = unsafe { &mut *(data as *mut Vec<Display>) };
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(m, &mut info) } != 0 {
        let r = info.rcMonitor;
        list.push(Display {
            index: 0,
            rect: Rect {
                x: r.left as i64,
                y: r.top as i64,
                width: (r.right - r.left) as i64,
                height: (r.bottom - r.top) as i64,
            },
            primary: info.dwFlags & 1 != 0,
        });
    }
    1
}

/// The displays, primary first, numbered from 0.
pub fn displays() -> Vec<Display> {
    init();
    let mut list: Vec<Display> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(monitor_cb),
            &mut list as *mut _ as LPARAM,
        );
    }
    list.sort_by_key(|d| (!d.primary, d.rect.x, d.rect.y));
    for (i, d) in list.iter_mut().enumerate() {
        d.index = i;
    }
    list
}

/// The pixels of a rectangle of the virtual screen (physical pixels).
pub fn capture(rect: Rect) -> Result<Frame, String> {
    init();
    let (w, h) = (rect.width.max(1) as i32, rect.height.max(1) as i32);
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return Err("no screen".into());
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let ok = BitBlt(
            mem,
            0,
            0,
            w,
            h,
            screen,
            rect.x as i32,
            rect.y as i32,
            SRCCOPY | CAPTUREBLT,
        );
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..std::mem::zeroed()
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            bgra.as_mut_ptr() as *mut c_void,
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if ok == 0 || lines == 0 {
            return Err("the screen could not be captured".into());
        }
        Ok(Frame {
            width: w as usize,
            height: h as usize,
            bgra,
        })
    }
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// The executable's file name for a process ("notepad.exe"), or empty.
pub fn process_name(pid: u32) -> String {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return String::new();
        }
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or("").to_string()
    }
}

fn frame_bounds(hwnd: HWND) -> Option<Rect> {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut r as *mut _ as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr != 0 {
            let mut w: RECT = std::mem::zeroed();
            if windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut w) == 0 {
                return None;
            }
            r = w;
        }
        Some(Rect {
            x: r.left as i64,
            y: r.top as i64,
            width: (r.right - r.left) as i64,
            height: (r.bottom - r.top) as i64,
        })
    }
}

fn cloaked(hwnd: HWND) -> bool {
    let mut value: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as u32,
            &mut value as *mut _ as *mut c_void,
            4,
        ) == 0
            && value != 0
    }
}

unsafe extern "system" fn window_cb(hwnd: HWND, data: LPARAM) -> i32 {
    let list = unsafe { &mut *(data as *mut Vec<HWND>) };
    list.push(hwnd);
    1
}

/// Visible top-level application windows, front to back (z-order).
pub fn windows() -> Vec<Window> {
    init();
    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        EnumWindows(Some(window_cb), &mut handles as *mut _ as LPARAM);
    }
    let foreground = unsafe { GetForegroundWindow() };
    let mut out = Vec::new();
    for hwnd in handles {
        unsafe {
            if IsWindowVisible(hwnd) == 0 || cloaked(hwnd) {
                continue;
            }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW != 0 || !GetWindow(hwnd, GW_OWNER).is_null() {
                continue;
            }
        }
        let title = window_text(hwnd);
        if title.is_empty() {
            continue;
        }
        let Some(rect) = frame_bounds(hwnd) else {
            continue;
        };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        let minimized = unsafe { IsIconic(hwnd) != 0 };
        if !minimized && (rect.width <= 1 || rect.height <= 1) {
            continue;
        }
        out.push(Window {
            id: hwnd as usize as u64,
            title,
            app: process_name(pid),
            pid,
            rect,
            minimized,
            focused: hwnd == foreground,
        });
    }
    out
}

pub fn is_window(id: u64) -> bool {
    unsafe { IsWindow(id as usize as HWND) != 0 }
}

/// Bring a window to the front (restoring it when minimized) before capturing or acting on it.
pub fn focus(id: u64) {
    let hwnd = id as usize as HWND;
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
    }
}

pub fn window_bounds(id: u64) -> Option<Rect> {
    frame_bounds(id as usize as HWND)
}

/// The top-level window under a point (physical pixels), as a window id.
pub fn window_at(x: i64, y: i64) -> Option<u64> {
    unsafe {
        let hwnd = WindowFromPoint(windows_sys::Win32::Foundation::POINT {
            x: x as i32,
            y: y as i32,
        });
        if hwnd.is_null() {
            return None;
        }
        let root = GetAncestor(hwnd, GA_ROOT);
        Some((if root.is_null() { hwnd } else { root }) as usize as u64)
    }
}

pub fn window_pid(id: u64) -> u32 {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(id as usize as HWND, &mut pid);
    }
    pid
}

/// Milliseconds since the last keyboard or mouse input that this process did not send, if recent.
pub fn person_input_age_ms() -> Option<u32> {
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if GetLastInputInfo(&mut info) == 0 {
            return None;
        }
        let ours = OURS.load(Ordering::SeqCst);
        // Our own input moves the last-input time too; anything later than ~80 ms after it is the person's.
        if ours != 0 && info.dwTime.wrapping_sub(ours) < 80 {
            return None;
        }
        Some(GetTickCount().wrapping_sub(info.dwTime))
    }
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    OURS.store(unsafe { GetTickCount() }, Ordering::SeqCst);
    if sent as usize != inputs.len() {
        return Err("SECURE_DESKTOP".into());
    }
    Ok(())
}

fn mouse(flags: u32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

pub fn move_to(x: i64, y: i64) -> Result<(), String> {
    init();
    let ok = unsafe { SetCursorPos(x as i32, y as i32) };
    OURS.store(unsafe { GetTickCount() }, Ordering::SeqCst);
    if ok == 0 {
        return Err("SECURE_DESKTOP".into());
    }
    Ok(())
}

pub fn button(button: &str, down: bool) -> Result<(), String> {
    let flag = match (button, down) {
        ("right", true) => MOUSEEVENTF_RIGHTDOWN,
        ("right", false) => MOUSEEVENTF_RIGHTUP,
        ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
        ("middle", false) => MOUSEEVENTF_MIDDLEUP,
        (_, true) => MOUSEEVENTF_LEFTDOWN,
        (_, false) => MOUSEEVENTF_LEFTUP,
    };
    send(&[mouse(flag, 0)])
}

/// Wheel clicks: `dy` > 0 scrolls down, `dx` > 0 scrolls right.
pub fn scroll(dx: i64, dy: i64) -> Result<(), String> {
    let mut inputs = Vec::new();
    if dy != 0 {
        inputs.push(mouse(MOUSEEVENTF_WHEEL, (-dy * 120) as i32));
    }
    if dx != 0 {
        inputs.push(mouse(MOUSEEVENTF_HWHEEL, (dx * 120) as i32));
    }
    if inputs.is_empty() {
        return Ok(());
    }
    send(&inputs)
}

/// Unicode text as typed characters (a newline is Enter).
pub fn type_text(text: &str) -> Result<(), String> {
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        if unit == '\n' as u16 {
            inputs.push(key(0x0D, 0, 0));
            inputs.push(key(0x0D, 0, KEYEVENTF_KEYUP));
        } else if unit == '\r' as u16 {
            continue;
        } else {
            inputs.push(key(0, unit, KEYEVENTF_UNICODE));
            inputs.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
    }
    for chunk in inputs.chunks(200) {
        send(chunk)?;
    }
    Ok(())
}

/// The virtual-key code of a key name (ctrl, enter, f5, a, 7, …).
pub fn virtual_key(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_lowercase();
    let vk = match n.as_str() {
        "ctrl" | "control" | "lctrl" => 0x11,
        "shift" | "lshift" => 0x10,
        "alt" | "option" | "menu" => 0x12,
        "win" | "super" | "meta" | "cmd" | "command" | "windows" => 0x5B,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" | "back" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "space" | " " => 0x20,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "right" | "arrowright" => 0x27,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "page_up" | "pgup" => 0x21,
        "pagedown" | "page_down" | "pgdn" => 0x22,
        "capslock" => 0x14,
        "printscreen" | "print" => 0x2C,
        "contextmenu" | "apps" => 0x5D,
        _ => {
            if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<u16>().ok())
                && (1..=24).contains(&f)
            {
                return Some(0x70 + f - 1);
            }
            let mut chars = n.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            if c.is_ascii_alphabetic() {
                return Some(c.to_ascii_uppercase() as u16);
            }
            if c.is_ascii_digit() {
                return Some(c as u16);
            }
            let scan = unsafe { VkKeyScanW(c as u16) };
            if scan == -1 {
                return None;
            }
            return Some((scan as u16) & 0xFF);
        }
    };
    Some(vk)
}

/// A key combination: every key down in order, then up in reverse.
pub fn press(keys: &[u16]) -> Result<(), String> {
    let mut inputs: Vec<INPUT> = keys.iter().map(|&vk| key(vk, 0, extended(vk))).collect();
    inputs.extend(
        keys.iter()
            .rev()
            .map(|&vk| key(vk, 0, extended(vk) | KEYEVENTF_KEYUP)),
    );
    send(&inputs)
}

fn extended(vk: u16) -> u32 {
    // Arrows, Home/End, PgUp/PgDn, Insert/Delete are extended keys.
    if matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5D) {
        1 // KEYEVENTF_EXTENDEDKEY
    } else {
        0
    }
}

/// Whether the focused control of the foreground window is a classic password box (best effort:
/// Win32 edit controls; browsers and custom UI are not seen).
pub fn focused_password() -> bool {
    unsafe {
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(0, &mut info) == 0 || info.hwndFocus.is_null() {
            return false;
        }
        GetWindowLongW(info.hwndFocus, GWL_STYLE) as u32 & ES_PASSWORD as u32 != 0
    }
}

/// Open a program, document or registered app name like the Run box does (`notepad`, `mspaint`,
/// a path, an `.lnk`); returns the new process id when Windows gives one.
pub fn launch(file: &str, args: &str, cwd: Option<&str>) -> Result<Option<u32>, String> {
    let (file_w, args_w, verb) = (wide(file), wide(args), wide("open"));
    let cwd_w = cwd.map(wide);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | 0x0000_0400 /* SEE_MASK_FLAG_NO_UI */;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file_w.as_ptr();
        info.lpParameters = if args.is_empty() {
            std::ptr::null()
        } else {
            args_w.as_ptr()
        };
        info.lpDirectory = cwd_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr());
        info.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if info.hProcess.is_null() {
            return Ok(None);
        }
        let pid = windows_sys::Win32::System::Threading::GetProcessId(info.hProcess);
        CloseHandle(info.hProcess);
        Ok(if pid == 0 { None } else { Some(pid) })
    }
}

/// Ask a window to close (like clicking ×; the program may ask to save).
pub fn close_window(id: u64) -> bool {
    unsafe { PostMessageW(id as usize as HWND, WM_CLOSE, 0, 0) != 0 }
}

/// End a process at once (unsaved work is lost).
pub fn kill(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}
