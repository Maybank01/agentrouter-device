//! The screen switch and the 「正在操作」 bar (LINKED-DEVICES.md §17, §23.8). The device has no screen tools of
//! its own any more (owner 2026-10-14: 「就用现成的mcp就好了，就是默认安装的mcp」): looking at and operating the
//! screen is a local MCP server's job (Windows-MCP joins the default bundle when the person turns the screen on,
//! `agentrouter screen on`). What stays here is link-level and the same for every server: the bar at the top of
//! the screen while a call runs (left out of screenshots), and no calls while the secure desktop (lock screen,
//! UAC) has the input.

use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
mod banner;
#[cfg(windows)]
mod win;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone)]
pub struct Display {
    pub index: usize,
    pub rect: Rect,
    pub primary: bool,
}

/// Whether the person turned the screen on here (reported in `info.screen`; the cloud's bundle follows it).
#[derive(Default)]
pub struct Screen {
    enabled: AtomicBool,
}

impl Screen {
    pub fn set(&self, on: bool) {
        self.enabled.store(on && supported(), Ordering::SeqCst);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// The conversation ended or the person disconnected (nothing is kept here any more).
    pub fn forget(&self) {}
}

pub fn supported() -> bool {
    cfg!(windows)
}

/// The lock screen, UAC or another secure desktop is up (nothing acts there; local MCP servers included).
pub fn secure_desktop_active() -> bool {
    #[cfg(windows)]
    {
        !win::input_desktop_available()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The 「正在操作」 bar for a local MCP server's call.
pub fn note_banner(text: &str) {
    #[cfg(windows)]
    banner::note(text);
    #[cfg(not(windows))]
    let _ = text;
}
