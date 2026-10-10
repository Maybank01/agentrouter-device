//! The write boundary for commands at the folder levels (owner rule 2026-10-10: inside the linked
//! folders the AI works freely without questions, outside them it is blocked; undo is the safety net).
//!
//! On Windows the boundary is the operating system's, not the command text (Dev check 2026-10-11:
//! the same `[IO.File]::WriteAllText` outside the folders ran once and was refused once, because the
//! text check only sees paths spelled out in the command). Commands run at low integrity: a restricted
//! copy of the person's own token carrying the Low mandatory label (jobs.rs). The linked folders get a
//! Low label that everything in them inherits. Windows refuses a low process every write, delete,
//! rename and file creation on an object labelled higher, which is every other place on NTFS
//! (unlabelled objects count as Medium), and the registry outside `HKCU\Software\AppDataLow`. The
//! kernel checks the object actually opened, so it holds for writes from inside interpreters
//! (PowerShell .NET calls, python, node), for relative, 8.3, junction and symbolic-link paths, and for
//! input typed into a running job.
//!
//! Not covered (SECURITY.md): volumes without NTFS security (FAT32/exFAT sticks), network shares (the
//! server decides), a file moved into a folder that keeps its own label (it stays read-only to the
//! command until relabelled), requests brokered to programs already running at medium integrity
//! (COM servers, the task scheduler, other open programs), and reading, which stays allowed as before.
//! Programs built on the MSYS2/Cygwin runtime (Git Bash's `sh`, `bash` and Unix tools, so also git
//! hooks written in sh) do not start at low integrity; `git` itself, PowerShell, cmd, python and node
//! do. Writes to the person's profile outside the folders (global installs, `git config --global`, tool
//! caches not redirected below) fail, which is the point. On macOS and Linux only the text check
//! (scope_guard.rs) applies for now.
//!
//! The labels are this app's change to the person's folders, so it is recorded (`confined.json` in the
//! data folder) and taken back when a folder is unlinked, the device is unlinked or revoked.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::gate::{Scope, inside};
use crate::protocol::DeviceError;

const RECORD: &str = "confined.json";

/// How a confined command runs: environment overrides that point the temporary folder and common
/// tool caches into a folder the command may write (`scratch_root`).
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    pub env: Vec<(String, PathBuf)>,
}

/// Tool caches that live in the person's profile by default: inside a confined command they move to
/// the scratch folder, so installs and builds in the linked folders keep working.
const CACHES: &[(&str, &str)] = &[
    ("npm_config_cache", "npm"),
    ("YARN_CACHE_FOLDER", "yarn"),
    ("YARN_GLOBAL_FOLDER", "yarn-berry"),
    ("COREPACK_HOME", "corepack"),
    ("BUN_INSTALL_CACHE_DIR", "bun"),
    ("PIP_CACHE_DIR", "pip"),
    ("UV_CACHE_DIR", "uv"),
    ("POETRY_CACHE_DIR", "poetry"),
    ("GOCACHE", "go-build"),
    ("GOMODCACHE", "go-mod"),
    ("NUGET_PACKAGES", "nuget"),
    ("DENO_DIR", "deno"),
    ("XDG_CACHE_HOME", "xdg-cache"),
    // pnpm keeps its store under XDG_DATA_HOME when set.
    ("XDG_DATA_HOME", "xdg-data"),
];

/// Where confined commands keep temporary files and caches: `AppData\LocalLow\AgentRouter\Device`
/// (LocalLow is the per-user folder Windows labels Low for exactly this). Not the app's data folder,
/// which a command must never write.
pub fn scratch_root() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })?;
    Some(
        local
            .parent()?
            .join("LocalLow")
            .join("AgentRouter")
            .join("Device"),
    )
}

/// The scratch folder's temporary folder (what TEMP and TMP point to in a confined command).
pub fn scratch_tmp() -> Option<PathBuf> {
    scratch_root().map(|r| r.join("tmp"))
}

/// The boundary of one device: which folders it labelled, prepared again before each command.
pub struct Confinement {
    data_dir: PathBuf,
    lock: Mutex<()>,
}

impl Confinement {
    pub fn new(data_dir: &Path) -> Confinement {
        Confinement {
            data_dir: data_dir.to_path_buf(),
            lock: Mutex::new(()),
        }
    }

    /// `release` for this device, in turn with `prepare`.
    pub fn release(&self, keep: &[PathBuf]) {
        let _guard = self.lock.lock().unwrap();
        for failed in release(&self.data_dir, keep) {
            crate::util::log(&format!("could not take back a folder label: {failed}"));
        }
    }

    /// Make the boundary hold for a command in `scope`'s folders: every linked folder labelled Low
    /// (checked each time, labelled once), the app's own data inside one of them kept Medium, labels on
    /// folders no longer linked taken back. `None` where this system has no boundary (macOS, Linux).
    /// An error means the command must not run: without the label the folder would not be writable,
    /// and the person must know why.
    pub fn prepare(&self, scope: &Scope) -> Result<Option<Sandbox>, DeviceError> {
        if !cfg!(windows) {
            return Ok(None);
        }
        let _guard = self.lock.lock().unwrap();
        let fail = |what: &str, path: &Path, e: std::io::Error| {
            DeviceError::new(
                "FAILED",
                format!(
                    "没能给{what}（{}）设好写入边界，命令没有运行：{e}",
                    crate::gate::display(path)
                ),
            )
            .next("Tell the user the device could not protect the folder (it may be on a USB stick or network drive, or owned by another account); they can link a folder on an NTFS disk they own.")
        };
        let mut record = Record::load(&self.data_dir);
        let root = scratch_root().ok_or_else(|| {
            DeviceError::new("FAILED", "找不到这台电脑的用户文件夹，命令没有运行")
        })?;
        let tmp = root.join("tmp");
        for dir in [&tmp, &root.join("cache")] {
            std::fs::create_dir_all(dir).map_err(|e| fail("临时文件夹", dir, e))?;
        }
        if !os::is_low(&root) {
            os::label_low(&root).map_err(|e| fail("临时文件夹", &root, e))?;
        }
        for folder in &scope.folders {
            if !os::is_low(folder) {
                os::label_low(folder).map_err(|e| fail("链接的文件夹", folder, e))?;
                record.add_labelled(folder);
            }
        }
        // The app's own data (key, audit log, job output) stays out of reach even inside a linked folder.
        for own in &scope.deny {
            if scope.folders.iter().any(|f| inside(own, f)) && !os::is_protected(own) {
                os::protect(own).map_err(|e| fail("设备自己的数据", own, e))?;
                record.add_protected(own);
            }
        }
        record.release_except(&scope.folders);
        record.save(&self.data_dir);
        let cache = root.join("cache");
        let mut env: Vec<(String, PathBuf)> =
            vec![("TEMP".into(), tmp.clone()), ("TMP".into(), tmp.clone())];
        env.extend(
            CACHES
                .iter()
                .map(|(var, dir)| (var.to_string(), cache.join(dir))),
        );
        Ok(Some(Sandbox { env }))
    }
}

/// Take back the labels this app put on folders that are not in `keep` (a folder unlinked, or the
/// whole device unlinked with `keep` empty). What could not be undone stays recorded, is tried again
/// next time and is returned (path and reason) for the log.
pub fn release(data_dir: &Path, keep: &[PathBuf]) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut record = Record::load(data_dir);
    let failed = record.release_except(keep);
    record.save(data_dir);
    failed
}

/// Does `folder` carry the boundary label (everything in it writable by confined commands)?
pub fn labelled(folder: &Path) -> bool {
    os::is_low(folder)
}

/// What this app changed on the person's folders.
struct Record {
    labelled: Vec<PathBuf>,
    protected: Vec<PathBuf>,
}

impl Record {
    fn load(data_dir: &Path) -> Record {
        let value: Value = std::fs::read(data_dir.join(RECORD))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let list = |key: &str| -> Vec<PathBuf> {
            value[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default()
        };
        Record {
            labelled: list("labelled"),
            protected: list("protected"),
        }
    }

    fn save(&self, data_dir: &Path) {
        let text = |l: &[PathBuf]| -> Vec<String> {
            l.iter().map(|p| p.to_string_lossy().into_owned()).collect()
        };
        let path = data_dir.join(RECORD);
        if self.labelled.is_empty() && self.protected.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(
            path,
            json!({"labelled": text(&self.labelled), "protected": text(&self.protected)})
                .to_string(),
        );
    }

    fn add_labelled(&mut self, path: &Path) {
        if !self.labelled.iter().any(|p| same(p, path)) {
            self.labelled.push(path.to_path_buf());
        }
    }

    fn add_protected(&mut self, path: &Path) {
        if !self.protected.iter().any(|p| same(p, path)) {
            self.protected.push(path.to_path_buf());
        }
    }

    /// Unlabel what is not one of `keep` (a folder inside a kept one keeps its inherited label).
    fn release_except(&mut self, keep: &[PathBuf]) -> Vec<String> {
        let mut failed = Vec::new();
        // Kept in the record while it still needs undoing (gone already: nothing left to undo).
        let mut undo = |p: &Path, unprotect: bool| match os::unlabel(p, unprotect) {
            Ok(()) => false,
            Err(_) if !p.exists() => false,
            Err(e) => {
                failed.push(format!("{}: {e}", crate::gate::display(p)));
                true
            }
        };
        self.labelled
            .retain(|p| keep.iter().any(|k| same(k, p)) || undo(p, false));
        self.protected
            .retain(|p| keep.iter().any(|k| inside(p, k)) || undo(p, true));
        failed
    }
}

fn same(a: &Path, b: &Path) -> bool {
    inside(a, b) && inside(b, a)
}

#[cfg(windows)]
mod os {
    //! Mandatory labels on files and folders (SDDL `ML` entries in the SACL).
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, ACL_REVISION, CONTAINER_INHERIT_ACE, EqualSid, GetAce,
        GetSecurityDescriptorSacl, INHERIT_ONLY_ACE, INHERITED_ACE, InitializeAcl,
        LABEL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PROTECTED_SACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SYSTEM_MANDATORY_LABEL_ACE,
        UNPROTECTED_SACL_SECURITY_INFORMATION,
    };

    const MANDATORY_LABEL_ACE: u8 = 0x11;
    const NO_WRITE_UP: u32 = 0x1;
    const LOW: &str = "S-1-16-4096";
    const MEDIUM: &str = "S-1-16-8192";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    /// The label entries of `path` (sid, flags, mask), explicit or inherited.
    fn labels(path: &Path) -> Vec<(String, u8, u32)> {
        let mut out = Vec::new();
        // SAFETY: GetNamedSecurityInfoW allocates `sd` (freed below); `sacl` points into it.
        unsafe {
            let mut sacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                wide_path(path).as_ptr(),
                SE_FILE_OBJECT,
                LABEL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sacl,
                &mut sd,
            );
            if rc != ERROR_SUCCESS {
                return out;
            }
            if !sacl.is_null() {
                for i in 0..(*sacl).AceCount as u32 {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(sacl, i, &mut ace) == 0 {
                        continue;
                    }
                    let header = &*(ace as *const ACE_HEADER);
                    if header.AceType != MANDATORY_LABEL_ACE {
                        continue;
                    }
                    let label = &*(ace as *const SYSTEM_MANDATORY_LABEL_ACE);
                    let sid = &label.SidStart as *const u32 as PSID;
                    let name = [LOW, MEDIUM]
                        .into_iter()
                        .find(|s| sid_equals(sid, s))
                        .unwrap_or("other");
                    out.push((name.to_string(), header.AceFlags, label.Mask));
                }
            }
            LocalFree(sd as _);
        }
        out
    }

    fn sid_equals(sid: PSID, text: &str) -> bool {
        // SAFETY: ConvertStringSidToSidW allocates `other` (freed below).
        unsafe {
            let mut other: PSID = std::ptr::null_mut();
            if ConvertStringSidToSidW(wide(text).as_ptr(), &mut other) == 0 {
                return false;
            }
            let equal = EqualSid(sid, other) != 0;
            LocalFree(other as _);
            equal
        }
    }

    /// Does everything created in `path` (and `path` itself) carry the Low label?
    pub fn is_low(path: &Path) -> bool {
        let inherits = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
        labels(path).iter().any(|(sid, flags, mask)| {
            sid == LOW
                && flags & inherits == inherits
                && flags & INHERIT_ONLY_ACE as u8 == 0
                && mask & NO_WRITE_UP != 0
        })
    }

    /// Is `path` kept at Medium by its own, non-inherited label?
    pub fn is_protected(path: &Path) -> bool {
        labels(path)
            .iter()
            .any(|(sid, flags, _)| sid == MEDIUM && flags & INHERITED_ACE as u8 == 0)
    }

    fn set(path: &Path, sddl: &str, info: u32) -> std::io::Result<()> {
        // SAFETY: the descriptor is allocated by ConvertString… and freed below; `sacl` points into it.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide(sddl).as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let (mut present, mut defaulted) = (0, 0);
            let mut sacl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorSacl(sd, &mut present, &mut sacl, &mut defaulted);
            // Children inherit the change (SetNamedSecurityInfoW propagates inheritable entries).
            let rc = SetNamedSecurityInfoW(
                wide_path(path).as_ptr(),
                SE_FILE_OBJECT,
                info,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                sacl,
            );
            LocalFree(sd as _);
            if rc != ERROR_SUCCESS {
                return Err(std::io::Error::from_raw_os_error(rc as i32));
            }
        }
        Ok(())
    }

    /// Label a folder Low, inherited by everything in it (`icacls <folder> /setintegritylevel (OI)(CI)low`).
    pub fn label_low(path: &Path) -> std::io::Result<()> {
        set(path, "S:(ML;OICI;NW;;;LW)", LABEL_SECURITY_INFORMATION)
    }

    /// Keep a folder at Medium whatever its parent says.
    pub fn protect(path: &Path) -> std::io::Result<()> {
        set(
            path,
            "S:P(ML;OICI;NW;;;ME)",
            LABEL_SECURITY_INFORMATION | PROTECTED_SACL_SECURITY_INFORMATION,
        )
    }

    /// Remove this app's label: the folder goes back to what its parent gives it (usually none, Medium).
    /// `unprotect` also lets a protected folder inherit again (changing the SACL's protection, which an
    /// elevated caller may need the security privilege for, so only where it was set).
    pub fn unlabel(path: &Path, unprotect: bool) -> std::io::Result<()> {
        // SAFETY: an empty ACL in a local buffer of the right size and alignment.
        unsafe {
            let mut buf = [0u64; 2];
            let acl = buf.as_mut_ptr() as *mut ACL;
            if InitializeAcl(acl, std::mem::size_of_val(&buf) as u32, ACL_REVISION) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let rc = SetNamedSecurityInfoW(
                wide_path(path).as_ptr(),
                SE_FILE_OBJECT,
                if unprotect {
                    LABEL_SECURITY_INFORMATION | UNPROTECTED_SACL_SECURITY_INFORMATION
                } else {
                    LABEL_SECURITY_INFORMATION
                },
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
                acl,
            );
            if rc != ERROR_SUCCESS {
                return Err(std::io::Error::from_raw_os_error(rc as i32));
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod os {
    use std::path::Path;

    pub fn is_low(_: &Path) -> bool {
        false
    }
    pub fn is_protected(_: &Path) -> bool {
        false
    }
    pub fn label_low(_: &Path) -> std::io::Result<()> {
        Ok(())
    }
    pub fn protect(_: &Path) -> std::io::Result<()> {
        Ok(())
    }
    pub fn unlabel(_: &Path, _: bool) -> std::io::Result<()> {
        Ok(())
    }
}
