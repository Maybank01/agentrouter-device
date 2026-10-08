//! The device's identity: its private key seed, its device id and the control plane key it pinned.
//! On Windows it is sealed with DPAPI (only this Windows user on this computer can open it); elsewhere it
//! is a file only the owner can read (macOS Keychain / Linux Secret Service come with those builds).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{ControlKey, DeviceKey};
use crate::util::{b64url, b64url_decode, data_dir};

#[derive(Serialize, Deserialize)]
struct Stored {
    v: u32,
    seed: String,
    device: Option<String>,
    control: Option<ControlKey>,
}

/// A linked device's identity.
pub struct Identity {
    pub key: DeviceKey,
    pub device: String,
    pub control: ControlKey,
}

fn path() -> PathBuf {
    data_dir().join("identity.bin")
}

pub fn save(key: &DeviceKey, device: &str, control: &ControlKey) -> std::io::Result<()> {
    let stored = Stored {
        v: 1,
        seed: b64url(&key.seed()),
        device: Some(device.to_string()),
        control: Some(control.clone()),
    };
    let plain = serde_json::to_vec(&stored).unwrap_or_default();
    let sealed = seal(&plain)?;
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(&p, &sealed)
}

pub fn load() -> Option<Identity> {
    let sealed = std::fs::read(path()).ok()?;
    let plain = unseal(&sealed).ok()?;
    let stored: Stored = serde_json::from_slice(&plain).ok()?;
    let seed = b64url_decode(&stored.seed).and_then(|s| <[u8; 32]>::try_from(s).ok())?;
    let control = stored.control?;
    if !control.is_consistent() {
        return None;
    }
    Some(Identity {
        key: DeviceKey::from_seed(seed),
        device: stored.device?,
        control,
    })
}

pub fn is_linked() -> bool {
    load().is_some()
}

/// Forget the identity (revoked, or unlinked by the person): only a new link brings the device back.
pub fn forget() {
    let p = path();
    if p.exists() {
        // Overwrite before removing, so the sealed key does not linger in the file's old blocks.
        if let Ok(len) = std::fs::metadata(&p).map(|m| m.len()) {
            let _ = std::fs::write(&p, vec![0u8; len as usize]);
        }
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(windows)]
fn write_private(p: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(p, bytes)
}

#[cfg(unix)]
fn write_private(p: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(bytes)
}

#[cfg(windows)]
const ENTROPY: &[u8] = b"agentrouter-device/identity/v1";

#[cfg(windows)]
fn seal(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: the blobs point at live buffers for the duration of the call; the output is freed below.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(take_blob(output))
}

#[cfg(windows)]
fn unseal(sealed: &[u8]) -> std::io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: sealed.len() as u32,
        pbData: sealed.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as in `seal`.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(take_blob(output))
}

#[cfg(windows)]
fn take_blob(blob: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    use windows_sys::Win32::Foundation::LocalFree;
    // SAFETY: DPAPI allocated `cbData` bytes at `pbData` with LocalAlloc; we copy them and free once.
    unsafe {
        let out = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        LocalFree(blob.pbData as _);
        out
    }
}

#[cfg(not(windows))]
fn seal(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    Ok(plain.to_vec())
}

#[cfg(not(windows))]
fn unseal(sealed: &[u8]) -> std::io::Result<Vec<u8>> {
    Ok(sealed.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_round_trip() {
        let plain = b"secret seed material";
        let sealed = seal(plain).unwrap();
        #[cfg(windows)]
        assert_ne!(sealed.as_slice(), plain.as_slice());
        assert_eq!(unseal(&sealed).unwrap(), plain);
    }
}
