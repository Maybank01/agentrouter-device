//! What this host can take (Cloud `docs/development/VISION.md` §3.1, §4 D1): the platform places work by what `hostd`
//! reports and never estimates it.
//!
//! - **Static capacity** ([`capacity`]) and **labels** ([`labels`]) go out from the first frame (`hello.link`, `info`)
//!   and in `host__info`: cores, memory, the disk of the home folder, GPUs, how many environments this host takes;
//!   the OS, the tools and runtimes on PATH, and the owner's own labels (`config.json` `labels`).
//! - **Live load** ([`LoadMeter::sample`]) goes with every heartbeat (`{"op":"load"}`): CPU use, the 1-minute load
//!   average where the OS has one, free memory and disk, running environments and the free slots left.
//!
//! An environment is one Agent's workplace on this host (VISION §3.1); hostd counts the conversations with an open MCP
//! session. How many it takes is the owner's (`config.json` `max_environments`), else one per two cores and 4 GiB of
//! memory, at least one.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sysinfo::{Disks, MemoryRefreshKind, RefreshKind, System};

const MIB: u64 = 1024 * 1024;

/// Programs a task's `requires.tools` can name, when on PATH.
const TOOLS: &[&str] = &[
    "git", "rg", "docker", "podman", "pwsh", "bash", "ffmpeg", "make", "cmake",
];
/// Runtimes a task's `requires.runtimes` can name, with the programs that stand for each.
const RUNTIMES: &[(&str, &[&str])] = &[
    ("node", &["node"]),
    ("python", &["python3", "python"]),
    ("uv", &["uv"]),
    ("java", &["java"]),
    ("go", &["go"]),
    ("rust", &["cargo"]),
    ("dotnet", &["dotnet"]),
    ("deno", &["deno"]),
    ("bun", &["bun"]),
];

struct Machine {
    cores: usize,
    physical_cores: Option<usize>,
    memory_mb: u64,
    gpus: Vec<Value>,
}

fn machine() -> &'static Machine {
    static MACHINE: OnceLock<Machine> = OnceLock::new();
    MACHINE.get_or_init(|| {
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        Machine {
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
            physical_cores: System::physical_core_count(),
            memory_mb: sys.total_memory() / MIB,
            gpus: gpus(),
        }
    })
}

/// How many environments this host takes: the owner's number, else one per two cores and 4 GiB, at least one.
pub fn max_environments(owner: Option<u32>) -> u32 {
    if let Some(n) = owner.filter(|n| *n > 0) {
        return n;
    }
    let m = machine();
    let by_cores = (m.cores / 2).max(1) as u64;
    let by_memory = (m.memory_mb / 4096).max(1);
    by_cores.min(by_memory).clamp(1, 64) as u32
}

/// Static capacity: `{cores, physical_cores, memory_mb, disk_mb, gpus: [{name, memory_mb}], environments}`.
/// `disk_mb` is the size of the disk that holds `home` (the first linked folder).
pub fn capacity(home: &Path, max_environments_owner: Option<u32>) -> Value {
    let m = machine();
    json!({
        "cores": m.cores,
        "physical_cores": m.physical_cores,
        "memory_mb": m.memory_mb,
        "disk_mb": disk_of(home).map(|(total, _)| total / MIB),
        "gpus": m.gpus,
        "environments": max_environments(max_environments_owner),
    })
}

/// Peripherals (Cloud DEVICES §2.4): `{display: [{id, width, height}], audio_out, audio_in, camera}`. A value the OS
/// cannot tell is `null` (unknown), never a guess; `display: []` = no interactive desktop (a service, a headless box).
pub fn resources() -> Value {
    peripherals()
}

#[cfg(windows)]
fn peripherals() -> Value {
    use windows_sys::Win32::Media::Audio::{waveInGetNumDevs, waveOutGetNumDevs};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CMONITORS, SM_CXSCREEN, SM_CYSCREEN,
    };
    // SAFETY: plain queries without arguments.
    let (monitors, w, h, out, inp) = unsafe {
        (
            GetSystemMetrics(SM_CMONITORS),
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            waveOutGetNumDevs(),
            waveInGetNumDevs(),
        )
    };
    let display: Vec<Value> = (0..monitors.max(0))
        .map(|i| {
            if i == 0 {
                json!({"id": 0, "width": w, "height": h})
            } else {
                json!({"id": i})
            }
        })
        .collect();
    json!({"display": display, "audio_out": out > 0, "audio_in": inp > 0, "camera": null})
}

#[cfg(target_os = "linux")]
fn peripherals() -> Value {
    let desktop = crate::shell_env::var_os("WAYLAND_DISPLAY").is_some()
        || crate::shell_env::var_os("DISPLAY").is_some();
    let display: Vec<Value> = if desktop {
        vec![json!({"id": 0})]
    } else {
        Vec::new()
    };
    // ALSA's PCM list: "00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1".
    let pcm = std::fs::read_to_string("/proc/asound/pcm").ok();
    let has = |what: &str| pcm.as_deref().map(|t| t.contains(what));
    let camera = std::fs::read_dir("/sys/class/video4linux")
        .ok()
        .map(|mut d| d.next().is_some());
    json!({"display": display, "audio_out": has("playback"), "audio_in": has("capture"), "camera": camera})
}

#[cfg(not(any(windows, target_os = "linux")))]
fn peripherals() -> Value {
    json!({"display": null, "audio_out": null, "audio_in": null, "camera": null})
}

/// Labels: `{os, arch, tools, runtimes, custom}` (the owner's own from `config.json`, e.g. `gpu`, `rack-2`).
pub fn labels(custom: &[String]) -> Value {
    static FOUND: OnceLock<(Vec<&'static str>, Vec<&'static str>)> = OnceLock::new();
    let (tools, runtimes) = FOUND.get_or_init(|| {
        let tools = TOOLS.iter().copied().filter(|t| on_path(t)).collect();
        let runtimes = RUNTIMES
            .iter()
            .filter(|(_, programs)| programs.iter().any(|p| on_path(p)))
            .map(|(name, _)| *name)
            .collect();
        (tools, runtimes)
    });
    let custom: Vec<String> = custom
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| valid_label(l))
        .take(32)
        .collect();
    json!({
        "os": crate::util::os_name(),
        "arch": crate::util::arch_name(),
        "tools": tools,
        "runtimes": runtimes,
        "custom": custom,
    })
}

/// An owner label: 1–40 characters, letters (any script), digits, `-`, `_`, `.`, `:`.
pub fn valid_label(l: &str) -> bool {
    (1..=40).contains(&l.chars().count())
        && l.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// Live load between heartbeats (CPU use needs two readings, so the meter keeps the last one).
pub struct LoadMeter {
    sys: Mutex<System>,
    max_environments: u32,
}

impl LoadMeter {
    pub fn new(max_environments_owner: Option<u32>) -> LoadMeter {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        LoadMeter {
            sys: Mutex::new(sys),
            max_environments: max_environments(max_environments_owner),
        }
    }

    /// `{cpu_percent, load1, free_memory_mb, free_disk_mb, environments, free_slots}`; `load1` is null on Windows.
    pub fn sample(&self, home: &Path, environments: usize) -> Value {
        let mut sys = self.sys.lock().unwrap_or_else(|e| e.into_inner());
        sys.refresh_cpu_usage();
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let load1 = (!cfg!(windows)).then(|| (System::load_average().one * 100.0).round() / 100.0);
        json!({
            "cpu_percent": (sys.global_cpu_usage() * 10.0).round() / 10.0,
            "load1": load1,
            "free_memory_mb": sys.available_memory() / MIB,
            "free_disk_mb": disk_of(home).map(|(_, free)| free / MIB),
            "environments": environments,
            "free_slots": (self.max_environments as usize).saturating_sub(environments),
        })
    }
}

/// (total, available) bytes of the disk whose mount point holds `path` (the longest match).
fn disk_of(path: &Path) -> Option<(u64, u64)> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let real = Path::new(
        real.to_str()
            .map(|s| s.strip_prefix(r"\\?\").unwrap_or(s))
            .unwrap_or_default(),
    )
    .to_path_buf();
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| real.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| (d.total_space(), d.available_space()))
}

fn on_path(name: &str) -> bool {
    if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .any(|ext| crate::shell::find_on_path(&format!("{name}.{ext}")).is_some())
    } else {
        crate::shell::find_on_path(name).is_some()
    }
}

/// GPUs: NVIDIA through `nvidia-smi` when it is on PATH (any OS); Apple silicon's integrated GPU. Others are not
/// listed yet (no portable query without a vendor tool).
fn gpus() -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(smi) = crate::shell::find_on_path(if cfg!(windows) {
        "nvidia-smi.exe"
    } else {
        "nvidia-smi"
    }) {
        let mut cmd = Command::new(smi);
        cmd.args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ]);
        if let Some(text) = output_within(cmd, Duration::from_secs(5)) {
            for line in text.lines() {
                let mut parts = line.splitn(2, ',').map(str::trim);
                let name = parts.next().unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let memory_mb = parts.next().and_then(|m| m.parse::<u64>().ok());
                out.push(json!({"vendor": "nvidia", "name": name, "memory_mb": memory_mb}));
            }
        }
    }
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        out.push(json!({"vendor": "apple", "name": "Apple silicon GPU", "memory_mb": null, "unified_memory": true}));
    }
    out
}

/// A program's stdout, or None when it fails or takes longer than `limit` (killed then).
fn output_within(mut cmd: Command, limit: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut text = String::new();
                std::io::Read::read_to_string(child.stdout.as_mut()?, &mut text).ok()?;
                return Some(text);
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_and_load_describe_this_machine() {
        let home = crate::util::home_dir();
        let c = capacity(&home, None);
        assert!(c["cores"].as_u64().unwrap() >= 1, "{c}");
        assert!(c["memory_mb"].as_u64().unwrap() > 0, "{c}");
        assert!(c["environments"].as_u64().unwrap() >= 1, "{c}");
        assert!(c["gpus"].is_array(), "{c}");
        assert_eq!(capacity(&home, Some(7))["environments"], 7);

        let meter = LoadMeter::new(Some(3));
        let l = meter.sample(&home, 1);
        assert_eq!(l["environments"], 1, "{l}");
        assert_eq!(l["free_slots"], 2, "{l}");
        assert!(l["free_memory_mb"].as_u64().unwrap() > 0, "{l}");
        assert_eq!(meter.sample(&home, 5)["free_slots"], 0);
        assert_eq!(l["load1"].is_null(), cfg!(windows), "{l}");
    }

    #[test]
    fn labels_carry_the_os_and_only_valid_owner_labels() {
        let l = labels(&[
            "gpu".into(),
            "rack-2".into(),
            "我的渲染机".into(),
            "bad label".into(),
            "".into(),
        ]);
        assert_eq!(l["os"], crate::util::os_name());
        assert_eq!(l["custom"], json!(["gpu", "rack-2", "我的渲染机"]));
        assert!(l["tools"].is_array() && l["runtimes"].is_array(), "{l}");
    }
}
