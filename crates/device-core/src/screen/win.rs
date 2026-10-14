//! Windows: per-monitor DPI awareness (physical pixels), the display list the bar is placed by, and whether the
//! secure desktop (UAC, lock screen) has the input.

use std::sync::Once;

use windows_sys::Win32::Foundation::{LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};
use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    SetThreadDpiAwarenessContext,
};

use super::{Display, Rect};

static DPI: Once = Once::new();

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
