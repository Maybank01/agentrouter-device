//! 「正在被控制」 (LINKED-DEVICES.md §17, minimal): while a conversation looks at or acts on the screen,
//! a small always-on-top bar at the top of the primary display says so and what was done last. It lets
//! clicks through, never takes focus, and is left out of screenshots (display affinity), so the AI
//! neither sees nor clicks it. It hides itself a while after the last action.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, Once};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DeleteObject,
    DrawTextW, EndPaint, FillRect, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, HWND_TOPMOST,
    LWA_ALPHA, MSG, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SetLayeredWindowAttributes, SetTimer, SetWindowDisplayAffinity, SetWindowPos,
    ShowWindow, TranslateMessage, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::util::now_ms;

/// How long the bar stays after the last screen action.
const SHOW_MS: i64 = 20_000;
const WIDTH: i32 = 560;
const HEIGHT: i32 = 30;

static START: Once = Once::new();
static TEXT: Mutex<String> = Mutex::new(String::new());
static LAST: AtomicI64 = AtomicI64::new(0);
static SHOWN: AtomicBool = AtomicBool::new(false);
static WINDOW: AtomicI64 = AtomicI64::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Show (or refresh) the bar with what the AI just did.
pub fn note(text: &str) {
    *TEXT.lock().unwrap() = format!("AgentRouter：AI 正在操作这台电脑 · {text}");
    LAST.store(now_ms(), Ordering::SeqCst);
    START.call_once(|| {
        std::thread::spawn(run);
    });
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            InvalidateRect(hwnd as HWND, std::ptr::null(), 1);
        }
    }
}

/// The bar's rectangle in physical pixels, when it is showing (actions never land on it).
pub fn area() -> Option<super::Rect> {
    if !SHOWN.load(Ordering::SeqCst) {
        return None;
    }
    let primary = super::win::displays().into_iter().find(|d| d.primary)?;
    Some(super::Rect {
        x: primary.rect.x + (primary.rect.width - WIDTH as i64) / 2,
        y: primary.rect.y,
        width: WIDTH as i64,
        height: HEIGHT as i64,
    })
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let brush = CreateSolidBrush(0x0000_3AD9); // orange-red (BGR)
            FillRect(dc, &rc, brush);
            DeleteObject(brush);
            let face = wide("Microsoft YaHei UI");
            let font = CreateFontW(-16, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
            let old = SelectObject(dc, font);
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, 0x00FF_FFFF);
            let mut text = wide(&TEXT.lock().unwrap());
            DrawTextW(
                dc,
                text.as_mut_ptr(),
                -1,
                &mut rc,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
            SelectObject(dc, old);
            DeleteObject(font);
            EndPaint(hwnd, &ps);
            0
        },
        WM_TIMER => {
            let active = now_ms() - LAST.load(Ordering::SeqCst) < SHOW_MS;
            if active != SHOWN.load(Ordering::SeqCst) {
                SHOWN.store(active, Ordering::SeqCst);
                unsafe {
                    ShowWindow(hwnd, if active { SW_SHOWNOACTIVATE } else { SW_HIDE });
                }
            }
            if active {
                unsafe {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    InvalidateRect(hwnd, std::ptr::null(), 1);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn run() {
    super::win::init();
    let Some(primary) = super::win::displays().into_iter().find(|d| d.primary) else {
        return;
    };
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide("AgentRouterControlBar");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = wide("AgentRouter");
        let x = primary.rect.x as i32 + (primary.rect.width as i32 - WIDTH) / 2;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            x,
            primary.rect.y as i32,
            WIDTH,
            HEIGHT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        SetLayeredWindowAttributes(hwnd, 0, 235, LWA_ALPHA);
        // WDA_EXCLUDEFROMCAPTURE (Windows 10 2004+): not in screenshots.
        SetWindowDisplayAffinity(hwnd, 0x11);
        WINDOW.store(hwnd as i64, Ordering::SeqCst);
        SetTimer(hwnd, 1, 300, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
