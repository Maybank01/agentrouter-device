//! The local gate for paths (LINKED-DEVICES.md §6): every path a request touches is checked here
//! before anything is opened, and the real location of an opened file is checked again afterwards.
//!
//! - Syntax first: absolute paths only; no `\\?\`, `\\.\` or UNC; no alternate data streams; no device
//!   names (`CON`, `NUL`, `COM1`…); no names ending in a dot or space.
//! - Then the real location: the nearest existing ancestor is resolved with links followed (junctions,
//!   symbolic links, 8.3 short names), so a link inside an allowed folder cannot lead outside it.
//! - Below `full`, that location must be inside one of the allowed folders. The app's own data folder
//!   (key, audit log, settings) is never reachable through the file tools.

use std::fs::File;
use std::path::{Component, Path, PathBuf};

use crate::config::Access;
use crate::protocol::DeviceError;

#[derive(Debug, Clone)]
pub struct Scope {
    pub access: Access,
    /// The allowed folders, resolved (real paths).
    pub folders: Vec<PathBuf>,
    /// Never reachable through the file tools (the app's own data).
    pub deny: Vec<PathBuf>,
}

/// Resolve the folders the person chose (missing ones are dropped).
pub fn resolve_folders(folders: &[String]) -> Vec<PathBuf> {
    folders
        .iter()
        .filter_map(|f| std::fs::canonicalize(f).ok())
        .filter(|p| p.is_dir())
        .collect()
}

/// A path as people write it (`C:\Users\me`, not `\\?\C:\Users\me`).
pub fn display(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return rest.to_string();
        }
    }
    text.into_owned()
}

fn invalid(message: &str) -> DeviceError {
    DeviceError::new("PATH_INVALID", message)
}

/// Check the syntax of a raw path (before touching the file system).
pub fn check_syntax(raw: &str) -> Result<(), DeviceError> {
    if raw.is_empty() {
        return Err(invalid("path required"));
    }
    if raw.len() > 4000 || raw.contains('\0') {
        return Err(invalid("path is not valid"));
    }
    let b = raw.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && sep(b[0]) && sep(b[1]) {
        return Err(DeviceError::denied(
            "不允许 UNC 和设备路径（\\\\?\\、\\\\.\\、\\\\服务器）",
        ));
    }
    if cfg!(windows) {
        if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && sep(b[2])) {
            return Err(invalid("需要完整路径，例如 D:\\folder\\file.txt"));
        }
        if raw[2..].contains(':') {
            return Err(DeviceError::denied("不允许备用数据流（路径里的冒号）"));
        }
        for segment in raw[3..].split(['\\', '/']).filter(|s| !s.is_empty()) {
            if segment == "." || segment == ".." {
                continue;
            }
            if segment
                .chars()
                .any(|c| (c as u32) < 32 || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
            {
                return Err(invalid("路径里有不允许的字符"));
            }
            if is_device_name(segment) {
                return Err(DeviceError::denied("不允许设备名（CON、NUL、COM1 等）"));
            }
            if segment.ends_with('.') || segment.ends_with(' ') {
                return Err(DeviceError::denied("不允许以点或空格结尾的名字"));
            }
        }
    } else if !raw.starts_with('/') {
        return Err(invalid("需要完整路径，例如 /home/me/file.txt"));
    }
    Ok(())
}

fn is_device_name(segment: &str) -> bool {
    let stem = segment
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    if matches!(
        stem.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) {
        return true;
    }
    let mut chars = stem.chars();
    let head: String = chars.by_ref().take(3).collect();
    let tail: String = chars.collect();
    (head == "com" || head == "lpt")
        && tail.chars().count() == 1
        && tail
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
}

/// Normalise `.` and `..` without touching the file system.
fn lexical(raw: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never above the root.
                let is_root = out.parent().is_none()
                    || matches!(out.components().next_back(), Some(Component::RootDir));
                if !is_root {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The real location: the nearest existing ancestor resolved (links followed), then the rest.
fn real_location(path: &Path) -> Result<PathBuf, DeviceError> {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(real) => {
                let mut out = real;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            Err(_) => {
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Err(invalid("没有这个盘或位置"));
                };
                rest.push(name);
                if !existing.pop() {
                    return Err(invalid("没有这个盘或位置"));
                }
            }
        }
    }
}

fn component_key(c: Component<'_>) -> String {
    let text = c.as_os_str().to_string_lossy();
    if cfg!(windows) || cfg!(target_os = "macos") {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// Is `path` the folder `base` or inside it (component-wise; case-insensitive on Windows and macOS).
pub fn inside(path: &Path, base: &Path) -> bool {
    let p: Vec<String> = path.components().map(component_key).collect();
    let b: Vec<String> = base.components().map(component_key).collect();
    !b.is_empty() && p.len() >= b.len() && p[..b.len()] == b[..]
}

impl Scope {
    /// A path a request may touch, as its real location.
    pub fn check(&self, raw: &str) -> Result<PathBuf, DeviceError> {
        check_syntax(raw)?;
        let real = real_location(&lexical(raw))?;
        self.check_real(&real)?;
        Ok(real)
    }

    fn check_real(&self, real: &Path) -> Result<(), DeviceError> {
        if self.deny.iter().any(|d| inside(real, d)) {
            return Err(DeviceError::denied(
                "这是 AgentRouter 设备程序自己的数据，不能经工具读写",
            ));
        }
        if self.access != Access::Full && !self.folders.iter().any(|f| inside(real, f)) {
            return Err(DeviceError::denied("这个路径不在允许的文件夹里"));
        }
        Ok(())
    }

    /// After opening: where the handle really points must pass the same check (closes the gap between check and open).
    pub fn check_opened(&self, file: &File) -> Result<(), DeviceError> {
        match opened_path(file) {
            Some(real) => self.check_real(&real),
            None => Ok(()),
        }
    }

    /// The folder a command starts in.
    pub fn default_cwd(&self, home: &Path) -> Option<PathBuf> {
        if self.access == Access::Full {
            Some(home.to_path_buf())
        } else {
            self.folders.first().cloned()
        }
    }
}

#[cfg(windows)]
fn opened_path(file: &File) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: the handle is a live file handle; the buffer is writable for its length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as _,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])));
        }
        buf.resize(n + 1, 0);
    }
}

#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn opened_path(_file: &File) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names() {
        for name in [
            "CON", "nul.txt", "Com1", "LPT9.log", "conin$", "COM¹", "aux ",
        ] {
            assert!(is_device_name(name), "{name}");
        }
        for name in ["console", "com10", "nullable", "lpt", "file.txt"] {
            assert!(!is_device_name(name), "{name}");
        }
    }

    #[test]
    fn lexical_never_climbs_above_root() {
        if cfg!(windows) {
            assert_eq!(lexical(r"C:\a\..\..\b"), PathBuf::from(r"C:\b"));
            assert_eq!(lexical(r"C:\a\.\b\..\c"), PathBuf::from(r"C:\a\c"));
        } else {
            assert_eq!(lexical("/a/../../b"), PathBuf::from("/b"));
        }
    }

    #[test]
    fn inside_is_component_wise() {
        if cfg!(windows) {
            assert!(inside(
                Path::new(r"\\?\C:\Mods\x"),
                Path::new(r"\\?\c:\mods")
            ));
            assert!(!inside(
                Path::new(r"\\?\C:\Mods2\x"),
                Path::new(r"\\?\C:\Mods")
            ));
        } else {
            assert!(inside(Path::new("/mods/x"), Path::new("/mods")));
            assert!(!inside(Path::new("/mods2/x"), Path::new("/mods")));
        }
    }

    #[cfg(windows)]
    #[test]
    fn syntax_rules_on_windows() {
        let denied = |p: &str| check_syntax(p).unwrap_err().code;
        assert_eq!(denied(r"\\?\C:\x"), "DENIED");
        assert_eq!(denied(r"\\.\PhysicalDrive0"), "DENIED");
        assert_eq!(denied(r"\\localhost\C$\x"), "DENIED");
        assert_eq!(denied(r"//server/share"), "DENIED");
        assert_eq!(denied(r"C:\x\file.txt:secret"), "DENIED");
        assert_eq!(denied(r"C:\x\NUL"), "DENIED");
        assert_eq!(denied(r"C:\x\a."), "DENIED");
        assert_eq!(denied(r"C:\x\a "), "DENIED");
        assert_eq!(denied(r"x\y"), "PATH_INVALID");
        assert_eq!(denied(r"\x\y"), "PATH_INVALID");
        assert_eq!(denied(r"C:x"), "PATH_INVALID");
        assert_eq!(denied(r"C:\a|b"), "PATH_INVALID");
        assert!(check_syntax(r"C:\x\..\y\file.txt").is_ok());
    }
}
