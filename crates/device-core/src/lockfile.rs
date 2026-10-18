//! A small cross-process advisory lock (a lock file), used to serialize work on one project between
//! local Agent sessions and to let the audit log be appended by more than one device process.
//!
//! Windows: the lock file is opened without sharing, so a second opener gets
//! `ERROR_SHARING_VIOLATION`; closing the handle releases it. Unix: `flock(LOCK_EX | LOCK_NB)`.
//! The lock is held for as long as the returned value lives.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::protocol::DeviceError;

#[derive(Debug)]
pub struct FileLock {
    #[cfg(windows)]
    #[allow(dead_code)] // held for its lifetime; closing the handle releases the lock
    handle: std::os::windows::io::OwnedHandle,
    #[cfg(unix)]
    #[allow(dead_code)] // held for its lifetime; closing the file releases the flock
    file: std::fs::File,
}

/// Wait up to `timeout` for the exclusive lock on `path`; `LOCAL_SESSION_BUSY` on timeout.
pub fn acquire(path: &Path, timeout: Duration) -> Result<FileLock, DeviceError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| DeviceError::new("LOCAL_SESSION_BUSY", format!("建不了锁目录：{e}")))?;
    }
    let deadline = Instant::now() + timeout;
    loop {
        match try_lock(path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return Err(DeviceError::new(
                        "LOCAL_SESSION_BUSY",
                        "另一个本地会话正在这个项目里执行命令",
                    )
                    .next(
                        "等了一会儿仍被占用：稍后再试，或让两个本地会话错开执行；不会打断已经开始的命令。",
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(DeviceError::new(
                    "LOCAL_SESSION_BUSY",
                    format!("拿不到项目锁：{e}"),
                ));
            }
        }
    }
}

#[cfg(windows)]
fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{ERROR_SHARING_VIOLATION, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, OPEN_ALWAYS,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: plain Win32 call on a path we own; the handle is wrapped immediately.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            0,                         // no sharing: the next opener fails while we hold it
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) {
            return Ok(None);
        }
        return Err(error);
    }
    // SAFETY: a handle just returned to us, owned from here on.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    Ok(Some(FileLock { handle }))
}

#[cfg(unix)]
fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    // SAFETY: flock on our own file descriptor.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(Some(FileLock { file }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("target");
        let dir = target
            .join("tmp")
            .join(format!("lock-{tag}-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("project.lock")
    }

    #[test]
    fn a_held_lock_blocks_and_releases() {
        let path = tmp("held");
        let held = acquire(&path, Duration::from_millis(50)).unwrap();
        let busy = acquire(&path, Duration::from_millis(100)).unwrap_err();
        assert_eq!(busy.code, "LOCAL_SESSION_BUSY", "{busy:?}");
        drop(held);
        acquire(&path, Duration::from_millis(500)).unwrap();
    }
}
