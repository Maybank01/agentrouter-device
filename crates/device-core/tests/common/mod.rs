//! Test helpers: a stand-in control plane that signs requests exactly as DEVICE-PROTOCOL.md §5.1 says.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use agentrouter_device::config::Access;
use agentrouter_device::consent::{Ask, Confirm};
use agentrouter_device::device::{Device, Options};
use agentrouter_device::jobs::Shell;
use agentrouter_device::keystore::Identity;
use agentrouter_device::protocol::{
    ControlKey, DeviceKey, args_digest, request_string, sha256_hex,
};
use agentrouter_device::util::{b64url_decode, hex, now_ms, random_bytes};
use serde_json::{Value, json};

pub const DEVICE_ID: &str = "lnd_0123456789abcdef0123456789abcdef";

pub struct ControlPlane {
    pub key: DeviceKey,
}

impl ControlPlane {
    pub fn new() -> ControlPlane {
        ControlPlane {
            key: DeviceKey::generate(),
        }
    }

    pub fn control_key(&self) -> ControlKey {
        let public_key = self.key.public_key();
        let raw = b64url_decode(&public_key).unwrap();
        ControlKey {
            kid: sha256_hex(&raw)[..16].to_string(),
            public_key,
        }
    }

    pub fn sign_at(
        &self,
        device: &str,
        session: &str,
        action: &str,
        args: &Value,
        exp: i64,
    ) -> Value {
        let digest = args_digest(args);
        let nonce = hex(&random_bytes::<16>());
        let sig = self.key.sign(&request_string(
            device, session, action, &digest, exp, &nonce,
        ));
        json!({"v": 1, "kid": self.control_key().kid, "device": device, "session": session, "action": action, "digest": digest, "exp": exp, "nonce": nonce, "sig": sig})
    }

    pub fn sign(&self, session: &str, action: &str, args: &Value) -> Value {
        self.sign_at(DEVICE_ID, session, action, args, now_ms() + 60_000)
    }
}

pub fn temp_dir(tag: &str) -> PathBuf {
    // Inside the build's own target folder (never the system temp folder).
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ar-device-test-{tag}-{}",
        agentrouter_device::util::short_id("")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    PathBuf::from(agentrouter_device::gate::display(
        &std::fs::canonicalize(dir).unwrap(),
    ))
}

/// Confirmation decided by the test, counting how often it was asked.
pub fn confirm_with(answer: bool, asked: Arc<AtomicUsize>) -> Confirm {
    Arc::new(move |_: &Ask, _: &AtomicBool| {
        asked.fetch_add(1, Ordering::SeqCst);
        answer.into()
    })
}

pub fn device(
    data: &Path,
    access: Access,
    folders: &[&Path],
    confirm: Confirm,
    cp: &ControlPlane,
) -> Device {
    let device = Device::new(Options {
        data_dir: data.to_path_buf(),
        access,
        folders: folders.iter().map(|f| f.display().to_string()).collect(),
        confirm,
        home: data.to_path_buf(),
        shell: Shell::default_for_os(),
    });
    device.set_identity(Some(Identity {
        key: DeviceKey::generate(),
        device: DEVICE_ID.to_string(),
        control: cp.control_key(),
    }));
    device
}

pub fn call(
    device: &Device,
    cp: &ControlPlane,
    action: &str,
    args: Value,
) -> Result<Value, (String, String)> {
    let request = cp.sign("ags_test", action, &args);
    device
        .serve(&request, &args, &AtomicBool::new(false))
        .map_err(|e| (e.code.to_string(), e.message))
}

/// A command that prints `text` in the device's default shell.
pub fn echo(text: &str) -> String {
    if cfg!(windows) {
        format!("Write-Output '{text}'")
    } else {
        format!("echo '{text}'")
    }
}

/// A command that runs about `seconds` and starts a child process that would outlive a plain kill.
pub fn long_tree(seconds: u32) -> String {
    if cfg!(windows) {
        format!(
            "Start-Process -NoNewWindow powershell -ArgumentList '-NoProfile','-Command','Start-Sleep {seconds}'; Start-Sleep {seconds}"
        )
    } else {
        format!("sleep {seconds} & sleep {seconds}")
    }
}
