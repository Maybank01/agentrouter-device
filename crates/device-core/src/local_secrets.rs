//! Credentials that never leave this computer (Cloud CONNECTIONS.md §3.2 `local:<name>`, HOSTD-MCP.md §13.7): a
//! logged-in account of a local MCP server (a cookie its QR login wrote), kept for this OS user only — sealed with
//! DPAPI on Windows, an owner-only file elsewhere (the same protection as the device identity, [`crate::keystore`]).
//! The cloud learns only a name and a state, never a value.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::keystore::{seal_with, unseal_with, write_private};

const PURPOSE: &[u8] = b"agentrouter-device/local-secret/v1";
/// The largest credential kept (a cookie jar is a few KiB).
pub const MAX_BYTES: usize = 64 * 1024;

/// Where the values live inside the device data folder (hidden from every sandbox).
fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("secrets")
}

/// A name is `<owner>/<credential>`: the owner is a server name (one account per server on this computer) or a space,
/// `space:<space id>`, whose programs share one account (a space's runtime secret, Cloud PROJECT-SERVICE §9.8.5);
/// lower-case letters, digits, `_`, `-`, one `/`.
pub fn valid_name(name: &str) -> bool {
    let word = |p: &str, max: usize| {
        (1..=max).contains(&p.len())
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    };
    let Some((owner, credential)) = name.split_once('/') else {
        return false;
    };
    let owner_ok = match owner.strip_prefix("space:") {
        Some(space) => word(space, 64),
        None => word(owner, 48),
    };
    owner_ok && word(credential, 48)
}

fn path_in(dir: &Path, name: &str) -> PathBuf {
    let digest = Sha256::digest(name.as_bytes());
    let file: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{file}.bin"))
}

fn refuse(name: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "local secret name {name:?} is not <server>/<credential> or space:<id>/<credential>"
        ),
    )
}

/// The value, when this computer holds one.
pub fn get(data_dir: &Path, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    get_in(&dir(data_dir), name)
}

fn get_in(dir: &Path, name: &str) -> std::io::Result<Option<Vec<u8>>> {
    if !valid_name(name) {
        return Err(refuse(name));
    }
    match std::fs::read(path_in(dir, name)) {
        Ok(sealed) => unseal_with(PURPOSE, &sealed).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Keep (or replace) a value.
pub fn put(data_dir: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    put_in(&dir(data_dir), name, value)
}

fn put_in(dir: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    if !valid_name(name) {
        return Err(refuse(name));
    }
    if value.len() > MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "local secret {name} is {} bytes (at most {MAX_BYTES})",
                value.len()
            ),
        ));
    }
    std::fs::create_dir_all(dir)?;
    let sealed = seal_with(PURPOSE, value)?;
    // Write beside, then rename: a crash never leaves half a credential.
    let target = path_in(dir, name);
    let temp = target.with_extension("tmp");
    write_private(&temp, &sealed)?;
    std::fs::rename(&temp, &target)
}

/// Forget a value (logging out, removing the server). Missing is fine.
pub fn remove(data_dir: &Path, name: &str) -> std::io::Result<()> {
    remove_in(&dir(data_dir), name)
}

fn remove_in(dir: &Path, name: &str) -> std::io::Result<()> {
    if !valid_name(name) {
        return Err(refuse(name));
    }
    match std::fs::remove_file(path_in(dir, name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Hand a value to the one process that needs it: a file only this OS user can read, in a folder only it can list
/// (the server's own scratch folder). Never an argument or an environment value (AGENTS.md "No data through command
/// lines").
pub fn write_owner_only(file: &Path, value: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    write_private(file, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_server_slash_credential() {
        assert!(valid_name("bilibili/cookie"));
        assert!(valid_name("my-mcp/api_key2"));
        assert!(valid_name("space:wrk_0123abcd/bilibili"));
        assert!(!valid_name("space:/bilibili"));
        assert!(!valid_name("space:wrk_x/a/b"));
        assert!(!valid_name("Space:wrk_x/bilibili"));
        assert!(!valid_name("bilibili"));
        assert!(!valid_name("a/b/c"));
        assert!(!valid_name("../x/y"));
        assert!(!valid_name("A/b"));
        assert!(!valid_name("/b"));
    }

    #[test]
    fn round_trip_and_remove() {
        let home = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("ar-local-secrets-{}", std::process::id()));
        assert_eq!(get_in(&home, "s/c").unwrap(), None);
        put_in(&home, "s/c", b"one").unwrap();
        put_in(&home, "s/c", b"two").unwrap();
        assert_eq!(get_in(&home, "s/c").unwrap().as_deref(), Some(&b"two"[..]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path_in(&home, "s/c"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "owner only");
        }
        remove_in(&home, "s/c").unwrap();
        remove_in(&home, "s/c").unwrap();
        assert_eq!(get_in(&home, "s/c").unwrap(), None);
        assert!(put_in(&home, "bad", b"x").is_err());
        assert!(put_in(&home, "s/c", &vec![0u8; MAX_BYTES + 1]).is_err());
        let _ = std::fs::remove_dir_all(home);
    }
}
