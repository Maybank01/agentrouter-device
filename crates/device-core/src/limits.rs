//! Resource limits of an environment (Cloud `docs/development/VISION.md` §3.1, §4 D10; device #127): memory, processes
//! and CPU for everything one local server runs. The `host` server is where the Agent's commands, jobs and terminals
//! run, so its tree is the environment's work. `hostd` enforces them from **outside** the sandbox, around each server
//! it starts: inside, seccomp refuses `AF_UNIX` (no systemd bus) and the Agent's own processes could reach whatever
//! enforces them.
//!
//! | OS | How | Memory | Processes | CPU |
//! | --- | --- | --- | --- | --- |
//! | Linux with a user systemd and cgroup v2 | a transient scope per server (`systemd-run --user --scope`, `OOMPolicy=continue`) | `MemoryMax` + `MemorySwapMax=0`: the kernel's OOM killer ends the largest process in the scope, the rest go on | `TasksMax` = processes × [`TASKS_PER_PROCESS`] (the kernel counts threads) | `CPUQuota` |
//! | Windows | a kill-on-close Job object per server (完全访问; folder mode waits for device #129: [`WINDOWS_CONFINED`]) | job committed memory: allocations beyond it fail | active processes: a start beyond it fails | hard CPU-rate cap |
//! | macOS, Linux without them | sampling every [`SAMPLE`] (sysinfo) | over it, the largest process ends | over it, the newest processes end | not capped |
//!
//! The values are the platform's (`link__limits`, from the environment record) capped field by field by the owner's
//! ceiling (`config.json` `limits`). Neither set: no limit, as in the person's own terminal. They change live, without
//! restarting a server (its background jobs keep running). Disk is not limited per folder (a PC has no per-folder
//! quota without an administrator); the output folder's `storage_low` event covers it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The smallest memory limit (a Node server alone needs ~60 MB).
pub const MEMORY_MIN_MB: u64 = 128;
pub const MEMORY_MAX_MB: u64 = 16 << 20;
/// The fewest processes (the server, a shell, a command and its helpers).
pub const PROCESSES_MIN: u32 = 8;
pub const PROCESSES_MAX: u32 = 100_000;
/// Linux counts tasks (threads included) in `pids.max`; a Node or Java process runs 10–30 threads.
pub const TASKS_PER_PROCESS: u64 = 32;
/// How often the sampler looks (macOS, Linux without systemd).
pub const SAMPLE: Duration = Duration::from_secs(2);
/// How often the cgroup watcher reads the scope's event counters (two small files).
pub const EVENTS: Duration = Duration::from_millis(500);

/// One environment's limits. `None` = no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub memory_mb: Option<u64>,
    pub processes: Option<u32>,
    /// Percent of the whole machine (all cores), 1–100.
    pub cpu_percent: Option<u32>,
}

fn bounded<T: PartialOrd + Copy>(v: Option<T>, min: T, max: T) -> Option<T> {
    v.filter(|v| *v >= min && *v <= max)
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.memory_mb.is_none() && self.processes.is_none() && self.cpu_percent.is_none()
    }

    /// The platform's limits (`link__limits {memoryMb, processes, cpuPercent}`): each optional, `null` or absent = no
    /// limit. A value out of range is an error naming the field and the range, never silently clipped.
    pub fn from_platform(args: &Value) -> Result<Limits, String> {
        fn field(args: &Value, key: &str, min: u64, max: u64) -> Result<Option<u64>, String> {
            match args.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => match v.as_u64() {
                    Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                    _ => Err(format!("{key} 要是 {min}–{max} 的整数，或 null（不限）")),
                },
            }
        }
        Ok(Limits {
            memory_mb: field(args, "memoryMb", MEMORY_MIN_MB, MEMORY_MAX_MB)?,
            processes: field(
                args,
                "processes",
                PROCESSES_MIN as u64,
                PROCESSES_MAX as u64,
            )?
            .map(|n| n as u32),
            cpu_percent: field(args, "cpuPercent", 1, 100)?.map(|n| n as u32),
        })
    }

    /// The owner's ceiling as `config.json` holds it, without values out of range (they are ignored, not clipped).
    pub fn sane(self) -> Limits {
        Limits {
            memory_mb: bounded(self.memory_mb, MEMORY_MIN_MB, MEMORY_MAX_MB),
            processes: bounded(self.processes, PROCESSES_MIN, PROCESSES_MAX),
            cpu_percent: bounded(self.cpu_percent, 1, 100),
        }
    }

    /// Field by field, the lower of the two (a field set on one side only keeps that value).
    pub fn capped_by(self, ceiling: Limits) -> Limits {
        fn min<T: Ord>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        Limits {
            memory_mb: min(self.memory_mb, ceiling.memory_mb),
            processes: min(self.processes, ceiling.processes),
            cpu_percent: min(self.cpu_percent, ceiling.cpu_percent),
        }
    }

    /// `{memoryMb, processes, cpuPercent}`, `null` = no limit.
    pub fn view(&self) -> Value {
        json!({"memoryMb": self.memory_mb, "processes": self.processes, "cpuPercent": self.cpu_percent})
    }

    /// 「内存 2048 MB、进程 256 个、CPU 50%」, or 「不限」.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(m) = self.memory_mb {
            parts.push(format!("内存 {m} MB"));
        }
        if let Some(p) = self.processes {
            parts.push(format!("进程 {p} 个"));
        }
        if let Some(c) = self.cpu_percent {
            parts.push(format!("CPU {c}%"));
        }
        if parts.is_empty() {
            "不限".to_string()
        } else {
            parts.join("、")
        }
    }
}

/// How this computer enforces limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mechanism {
    /// A transient systemd user scope (cgroup v2). `cpu`: the user manager has the cpu controller (else CPU is not
    /// capped).
    Systemd {
        run: std::path::PathBuf,
        ctl: std::path::PathBuf,
        cpu: bool,
    },
    JobObject,
    /// Sampling; `why` says why not the kernel (macOS, or what Linux lacks).
    Sampled {
        why: String,
    },
}

impl Mechanism {
    pub fn name(&self) -> &'static str {
        match self {
            Mechanism::Systemd { .. } => "systemd-scope",
            Mechanism::JobObject => "job-object",
            Mechanism::Sampled { .. } => "sampled",
        }
    }

    /// The fields this mechanism cannot enforce.
    pub fn unenforced(&self) -> Vec<&'static str> {
        match self {
            Mechanism::Systemd { cpu: false, .. } | Mechanism::Sampled { .. } => vec!["cpuPercent"],
            _ => Vec::new(),
        }
    }

    pub fn view(&self) -> Value {
        let mut v = json!({"enforcedBy": self.name(), "unenforced": self.unenforced()});
        if let Mechanism::Sampled { why } = self {
            v["why"] = json!(why);
        }
        v
    }
}

/// Windows folder mode (device #129): srt-win starts the confined server as the sandbox account through Secondary
/// Logon and puts it in a job of its own, so hostd's Job object holds only srt's launcher. Until the inner named job
/// lands, limits there are refused with this reason, never claimed.
pub const WINDOWS_CONFINED: &str = "Windows 文件夹模式下资源上限还不能生效：沙箱账户的进程由系统的二次登录服务启动，不在 hostd 的作业对象里（device #129）";

/// Whether limits can hold on a server started this way (`confined`: inside the folder sandbox).
pub fn enforceable(confined: bool) -> bool {
    !(cfg!(windows) && confined)
}

/// [`Mechanism::view`] for servers started this way: on Windows folder mode every field is unenforced, with why.
pub fn view(confined: bool) -> Value {
    let mut v = mechanism().view();
    if !enforceable(confined) {
        v["unenforced"] = json!(["memoryMb", "processes", "cpuPercent"]);
        v["why"] = json!(WINDOWS_CONFINED);
    }
    v
}

/// This computer's mechanism, found once.
pub fn mechanism() -> &'static Mechanism {
    static MECHANISM: OnceLock<Mechanism> = OnceLock::new();
    MECHANISM.get_or_init(find_mechanism)
}

#[cfg(windows)]
fn find_mechanism() -> Mechanism {
    Mechanism::JobObject
}

#[cfg(target_os = "macos")]
fn find_mechanism() -> Mechanism {
    Mechanism::Sampled {
        why: "macOS 没有 cgroups".into(),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn find_mechanism() -> Mechanism {
    linux::find().unwrap_or_else(|why| Mechanism::Sampled { why })
}

/// Called with one line when a limit is reached (for the server's log, the audit and the next call's answer).
pub type OnLimit = Arc<dyn Fn(String) + Send + Sync>;

/// The command to spawn for a server under limits: on Linux with systemd, `cmd` inside a transient user scope named
/// `unit` (systemd-run executes it in place, so its pid, process group and stdio stay the server's); elsewhere `cmd`
/// unchanged. Always a scope there, even without limits, so limits set later apply live.
pub fn command(cmd: std::process::Command, unit: &str, limits: &Limits) -> std::process::Command {
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Mechanism::Systemd { run, cpu, .. } = mechanism() {
        return linux::scoped(cmd, run, unit, limits, *cpu);
    }
    let _ = (unit, limits);
    cmd
}

/// The limits of one running server, enforced until dropped. Dropping it on Windows ends the whole tree (kill on
/// close); elsewhere the server's process group is ended by its caller.
pub struct Enforced {
    limits: Arc<Mutex<Limits>>,
    stop: Arc<AtomicBool>,
    kind: Kind,
    /// Limits cannot hold on this tree ([`enforceable`]): any non-empty limits are refused.
    refuse: bool,
}

enum Kind {
    #[cfg(all(unix, not(target_os = "macos")))]
    Scope {
        unit: String,
        ctl: std::path::PathBuf,
        cpu: bool,
    },
    #[cfg(windows)]
    Job(windows::Job),
    Sampled,
}

impl Enforced {
    /// Start enforcing `limits` on the server `pid` (just spawned by [`command`] with the same `unit`; `confined`:
    /// inside the folder sandbox).
    pub fn start(
        pid: u32,
        unit: &str,
        limits: Limits,
        on_limit: OnLimit,
        confined: bool,
    ) -> Result<Enforced, String> {
        let refuse = !enforceable(confined);
        if refuse && !limits.is_empty() {
            return Err(WINDOWS_CONFINED.to_string());
        }
        let shared = Arc::new(Mutex::new(limits));
        let stop = Arc::new(AtomicBool::new(false));
        let kind = match mechanism() {
            #[cfg(all(unix, not(target_os = "macos")))]
            Mechanism::Systemd { ctl, cpu, .. } => {
                linux::watch(pid, shared.clone(), stop.clone(), on_limit);
                Kind::Scope {
                    unit: unit.to_string(),
                    ctl: ctl.clone(),
                    cpu: *cpu,
                }
            }
            #[cfg(windows)]
            Mechanism::JobObject => {
                let _ = unit;
                Kind::Job(windows::Job::start(pid, shared.clone(), on_limit)?)
            }
            _ => {
                let _ = unit;
                sampler::start(pid, shared.clone(), stop.clone(), on_limit);
                Kind::Sampled
            }
        };
        Ok(Enforced {
            limits: shared,
            stop,
            kind,
            refuse,
        })
    }

    pub fn limits(&self) -> Limits {
        *self.limits.lock().unwrap()
    }

    /// New limits on the running tree (no restart).
    pub fn update(&self, limits: Limits) -> Result<(), String> {
        if self.refuse && !limits.is_empty() {
            return Err(WINDOWS_CONFINED.to_string());
        }
        match &self.kind {
            #[cfg(all(unix, not(target_os = "macos")))]
            Kind::Scope { unit, ctl, cpu } => linux::set(ctl, unit, &limits, *cpu)?,
            #[cfg(windows)]
            Kind::Job(job) => job.set(&limits)?,
            Kind::Sampled => {}
        }
        *self.limits.lock().unwrap() = limits;
        Ok(())
    }

    /// The pids in this server's Job object (Windows; the test checks the confined process is inside).
    #[cfg(windows)]
    pub fn job_pids(&self) -> Vec<u32> {
        match &self.kind {
            Kind::Job(job) => job.pids(),
            _ => Vec::new(),
        }
    }
}

impl Drop for Enforced {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The process tree under `root` (itself included), from one sysinfo snapshot.
fn tree(sys: &sysinfo::System, root: u32) -> Vec<u32> {
    use std::collections::HashMap;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children
                .entry(parent.as_u32())
                .or_default()
                .push(pid.as_u32());
        }
    }
    let mut all = vec![root];
    let mut i = 0;
    while i < all.len() {
        if let Some(c) = children.get(&all[i]) {
            for pid in c {
                if !all.contains(pid) {
                    all.push(*pid);
                }
            }
        }
        i += 1;
    }
    all.retain(|pid| sys.process(sysinfo::Pid::from_u32(*pid)).is_some());
    all
}

/// macOS, and Linux without a user systemd: what the kernel cannot do here, a sampler does after the fact.
mod sampler {
    use super::*;
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    pub fn start(root: u32, limits: Arc<Mutex<Limits>>, stop: Arc<AtomicBool>, on_limit: OnLimit) {
        std::thread::Builder::new()
            .name(format!("limits-{root}"))
            .spawn(move || {
                let mut sys = System::new();
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(SAMPLE);
                    let l = *limits.lock().unwrap();
                    if l.memory_mb.is_none() && l.processes.is_none() {
                        continue;
                    }
                    sys.refresh_processes_specifics(
                        ProcessesToUpdate::All,
                        true,
                        ProcessRefreshKind::nothing().with_memory(),
                    );
                    if sys.process(sysinfo::Pid::from_u32(root)).is_none() {
                        break;
                    }
                    for line in check(&sys, root, &l) {
                        on_limit(line);
                    }
                }
            })
            .ok();
    }

    /// One look: what it ended, as lines.
    pub fn check(sys: &System, root: u32, l: &Limits) -> Vec<String> {
        let mut said = Vec::new();
        let pids = tree(sys, root);
        let proc = |pid: u32| sys.process(sysinfo::Pid::from_u32(pid));
        if let Some(max) = l.processes
            && pids.len() > max as usize
        {
            // The newest go first; the server itself never.
            let mut extra: Vec<u32> = pids.iter().copied().filter(|p| *p != root).collect();
            extra.sort_by_key(|p| std::cmp::Reverse(proc(*p).map(|p| p.start_time()).unwrap_or(0)));
            let n = pids.len() - max as usize;
            for pid in extra.into_iter().take(n) {
                if let Some(p) = proc(pid) {
                    p.kill();
                }
            }
            said.push(format!(
                "进程数超过上限 {max} 个（有 {} 个）：结束了最新启动的 {n} 个",
                pids.len()
            ));
        }
        if let Some(max) = l.memory_mb {
            let used: u64 = pids
                .iter()
                .filter_map(|p| proc(*p))
                .map(|p| p.memory())
                .sum::<u64>()
                >> 20;
            if used > max {
                let biggest = pids
                    .iter()
                    .copied()
                    .filter_map(|pid| proc(pid).map(|p| (pid, p.memory())))
                    .max_by_key(|(_, m)| *m);
                if let Some((pid, mem)) = biggest
                    && let Some(p) = proc(pid)
                {
                    p.kill();
                    said.push(format!(
                        "内存超过上限 {max} MB（用了 {used} MB）：结束了占用最多的进程 {} (pid {pid}, {} MB)",
                        p.name().to_string_lossy(),
                        mem >> 20
                    ));
                }
            }
        }
        said
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn on_path(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .chain(["/usr/bin".into(), "/bin".into()])
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }

    /// systemd-run and systemctl, a reachable user manager, cgroup v2 with memory and pids delegated to it.
    pub fn find() -> Result<Mechanism, String> {
        let run = on_path("systemd-run").ok_or("没有 systemd-run")?;
        let ctl = on_path("systemctl").ok_or("没有 systemctl")?;
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
            return Err("不是 cgroup v2".into());
        }
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        let delegated = std::fs::read_to_string(format!(
            "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
        ))
        .map_err(|e| format!("读不到用户 systemd 的 cgroup（{e}）"))?;
        let has = |c: &str| delegated.split_whitespace().any(|d| d == c);
        if !has("memory") || !has("pids") {
            return Err(format!(
                "用户 systemd 没有 memory/pids 控制器（只有：{}）",
                delegated.trim()
            ));
        }
        let probe = Command::new(&run)
            .args(["--user", "--scope", "--quiet", "--collect", "--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("systemd-run 起不来：{e}"))?;
        if !probe.status.success() {
            let why = String::from_utf8_lossy(&probe.stderr);
            return Err(format!(
                "连不上用户 systemd：{}",
                why.lines().next().unwrap_or("").trim()
            ));
        }
        Ok(Mechanism::Systemd {
            run,
            ctl,
            cpu: has("cpu"),
        })
    }

    pub fn properties(l: &Limits, cpu: bool) -> Vec<String> {
        let mut p = match l.memory_mb {
            Some(m) => vec![format!("MemoryMax={m}M"), "MemorySwapMax=0".to_string()],
            None => vec![
                "MemoryMax=infinity".to_string(),
                "MemorySwapMax=infinity".to_string(),
            ],
        };
        p.push(match l.processes {
            Some(n) => format!("TasksMax={}", n as u64 * TASKS_PER_PROCESS),
            None => "TasksMax=infinity".to_string(),
        });
        if cpu {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as u32;
            p.push(match l.cpu_percent {
                Some(c) => format!("CPUQuota={}%", c * cores),
                None => "CPUQuota=".to_string(),
            });
        }
        p
    }

    pub fn scoped(cmd: Command, run: &Path, unit: &str, l: &Limits, cpu: bool) -> Command {
        let mut s = Command::new(run);
        // OOMPolicy=continue: the kernel ends the largest process in the scope and that is all; systemd's default
        // (stop) would then end the whole scope, the server with it.
        s.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--unit={unit}"))
            .args(["-p", "OOMPolicy=continue"]);
        for p in properties(l, cpu) {
            s.arg("-p").arg(p);
        }
        s.arg("--").arg(cmd.get_program()).args(cmd.get_args());
        if let Some(dir) = cmd.get_current_dir() {
            s.current_dir(dir);
        }
        for (k, v) in cmd.get_envs() {
            match v {
                Some(v) => s.env(k, v),
                None => s.env_remove(k),
            };
        }
        s
    }

    pub fn set(ctl: &Path, unit: &str, l: &Limits, cpu: bool) -> Result<(), String> {
        let out = Command::new(ctl)
            .args(["--user", "set-property", "--runtime"])
            .arg(format!("{unit}.scope"))
            .args(properties(l, cpu))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("systemctl 起不来：{e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl --user set-property {unit}.scope 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn counter(text: &str, key: &str) -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The scope's own events: the kernel enforces, this only says so (`memory.events` `oom_kill`, `pids.events`
    /// `max`).
    pub fn watch(pid: u32, limits: Arc<Mutex<Limits>>, stop: Arc<AtomicBool>, on_limit: OnLimit) {
        std::thread::Builder::new()
            .name(format!("limits-{pid}"))
            .spawn(move || {
                // systemd-run moves itself into the scope before it executes the server.
                let mut dir = None;
                for _ in 0..50 {
                    let path = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                        .ok()
                        .and_then(|t| {
                            t.lines()
                                .find_map(|l| l.strip_prefix("0::").map(str::to_string))
                        });
                    if let Some(p) = path.filter(|p| p.ends_with(".scope")) {
                        dir = Some(PathBuf::from(format!("/sys/fs/cgroup{p}")));
                        break;
                    }
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let Some(dir) = dir else { return };
                let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap_or_default();
                let (mut oom, mut forks) = (
                    counter(&read("memory.events"), "oom_kill "),
                    counter(&read("pids.events"), "max "),
                );
                while !stop.load(Ordering::SeqCst) && dir.is_dir() {
                    std::thread::sleep(EVENTS);
                    let l = *limits.lock().unwrap();
                    let now = counter(&read("memory.events"), "oom_kill ");
                    if now > oom {
                        on_limit(format!(
                            "内存超过上限 {} MB：系统结束了其中占用最多的进程（{} 次）",
                            l.memory_mb.map_or("?".to_string(), |m| m.to_string()),
                            now - oom
                        ));
                        oom = now;
                    }
                    let now = counter(&read("pids.events"), "max ");
                    if now > forks {
                        on_limit(format!(
                            "进程数到了上限 {} 个：新进程没能启动（{} 次）",
                            l.processes.map_or("?".to_string(), |n| n.to_string()),
                            now - forks
                        ));
                        forks = now;
                    }
                }
            })
            .ok();
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED, PostQueuedCompletionStatus,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_PROCESS_ID_LIST,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectAssociateCompletionPortInformation, JobObjectBasicProcessIdList,
        JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    const MSG_ACTIVE_PROCESS_LIMIT: u32 = 3;
    const MSG_JOB_MEMORY_LIMIT: u32 = 10;

    struct Handle(HANDLE);
    // SAFETY: kernel handles are usable from any thread; only Drop closes each, once.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: closes a handle this value owns, once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// A kill-on-close job with the limits, its completion port read by one thread (limit messages). Dropping it posts
    /// the thread's quit packet (the thread closes the port) and closes the job, which ends its processes.
    pub struct Job {
        job: Handle,
        port: HANDLE,
        /// CPU rate control was turned on once (Windows refuses to "turn off" what was never on: error 87).
        cpu_on: std::sync::atomic::AtomicBool,
    }

    const QUIT: usize = 1;

    // SAFETY: the port handle is only used to post the quit packet, which is thread-safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the port stays open until the thread reads this packet.
            unsafe {
                PostQueuedCompletionStatus(self.port, 0, QUIT, std::ptr::null());
            }
        }
    }

    impl Job {
        pub fn start(
            pid: u32,
            shared: Arc<Mutex<Limits>>,
            on_limit: OnLimit,
        ) -> Result<Job, String> {
            let l = *shared.lock().unwrap();
            // SAFETY: plain Win32 calls on handles owned here from creation on.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(format!(
                        "建不了作业对象：{}",
                        std::io::Error::last_os_error()
                    ));
                }
                let job = Handle(job);
                let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
                if port.is_null() {
                    return Err(format!(
                        "建不了完成端口：{}",
                        std::io::Error::last_os_error()
                    ));
                }
                let port = Handle(port);
                let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                    CompletionKey: std::ptr::null_mut(),
                    CompletionPort: port.0,
                };
                SetInformationJobObject(
                    job.0,
                    JobObjectAssociateCompletionPortInformation,
                    &assoc as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
                );
                let this = Job {
                    job,
                    port: port.0,
                    cpu_on: std::sync::atomic::AtomicBool::new(false),
                };
                this.set(&l)?;
                let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
                if process.is_null() {
                    return Err(format!(
                        "打不开进程 {pid}：{}",
                        std::io::Error::last_os_error()
                    ));
                }
                let process = Handle(process);
                if AssignProcessToJobObject(this.job.0, process.0) == 0 {
                    return Err(format!(
                        "进程 {pid} 进不了作业对象：{}",
                        std::io::Error::last_os_error()
                    ));
                }
                std::thread::Builder::new()
                    .name(format!("limits-{pid}"))
                    .spawn(move || {
                        let port = port;
                        loop {
                            let (mut msg, mut key, mut ov): (u32, usize, *mut OVERLAPPED) =
                                (0, 0, std::ptr::null_mut());
                            if GetQueuedCompletionStatus(
                                port.0,
                                &mut msg,
                                &mut key,
                                &mut ov,
                                u32::MAX,
                            ) == 0
                                || key == QUIT
                            {
                                break;
                            }
                            let limits = *shared.lock().unwrap();
                            match msg {
                                MSG_JOB_MEMORY_LIMIT => on_limit(format!(
                                    "内存到了上限 {} MB：进程 {} 要的内存没有给（分配失败）",
                                    limits.memory_mb.map_or("?".to_string(), |m| m.to_string()),
                                    ov as usize
                                )),
                                MSG_ACTIVE_PROCESS_LIMIT => on_limit(format!(
                                    "进程数到了上限 {} 个：新进程没能启动",
                                    limits.processes.map_or("?".to_string(), |n| n.to_string())
                                )),
                                _ => {}
                            }
                        }
                    })
                    .ok();
                Ok(this)
            }
        }

        pub fn set(&self, l: &Limits) -> Result<(), String> {
            // SAFETY: the job handle is ours; the structures live across each call.
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if let Some(m) = l.memory_mb {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                    info.JobMemoryLimit = (m as usize) << 20;
                }
                if let Some(n) = l.processes {
                    info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
                    info.BasicLimitInformation.ActiveProcessLimit = n;
                }
                if SetInformationJobObject(
                    self.job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(format!(
                        "作业对象的上限设不上：{}",
                        std::io::Error::last_os_error()
                    ));
                }
                // No CPU limit: nothing to set, or (once one was set) a cap at 100% of the machine.
                if l.cpu_percent.is_none() && !self.cpu_on.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
                cpu.ControlFlags =
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                cpu.Anonymous.CpuRate = l.cpu_percent.unwrap_or(100) * 100;
                self.cpu_on.store(true, Ordering::SeqCst);
                if SetInformationJobObject(
                    self.job.0,
                    JobObjectCpuRateControlInformation,
                    &cpu as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(format!(
                        "作业对象的 CPU 上限设不上：{}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Ok(())
        }

        pub fn pids(&self) -> Vec<u32> {
            const MAX: usize = 1024;
            #[repr(C)]
            struct List {
                head: JOBOBJECT_BASIC_PROCESS_ID_LIST,
                more: [usize; MAX],
            }
            // SAFETY: the buffer is large enough for MAX ids and lives across the call.
            unsafe {
                let mut list: List = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    self.job.0,
                    JobObjectBasicProcessIdList,
                    &mut list as *mut _ as *mut _,
                    std::mem::size_of::<List>() as u32,
                    std::ptr::null_mut(),
                );
                if ok == 0 {
                    return Vec::new();
                }
                let n = (list.head.NumberOfProcessIdsInList as usize).min(MAX + 1);
                let ids = std::slice::from_raw_parts(list.head.ProcessIdList.as_ptr(), n);
                ids.iter().map(|p| *p as u32).collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_sets_and_the_owner_caps_field_by_field() {
        let platform =
            Limits::from_platform(&json!({"memoryMb": 4096, "processes": 512, "cpuPercent": null}))
                .unwrap();
        let owner = Limits {
            memory_mb: Some(2048),
            processes: None,
            cpu_percent: Some(50),
        };
        assert_eq!(
            platform.capped_by(owner),
            Limits {
                memory_mb: Some(2048),
                processes: Some(512),
                cpu_percent: Some(50)
            }
        );
        assert!(Limits::default().capped_by(Limits::default()).is_empty());
        assert_eq!(Limits::default().capped_by(owner), owner);
    }

    #[test]
    fn out_of_range_values_are_named_never_clipped() {
        let e = Limits::from_platform(&json!({"memoryMb": 10})).unwrap_err();
        assert!(e.contains("memoryMb") && e.contains("128"), "{e}");
        assert!(Limits::from_platform(&json!({"cpuPercent": 0})).is_err());
        assert!(Limits::from_platform(&json!({"processes": "8"})).is_err());
        let owner: Limits =
            serde_json::from_str(r#"{"memory_mb": 1, "processes": 64, "cpu_percent": 300}"#)
                .unwrap();
        assert_eq!(
            owner.sane(),
            Limits {
                memory_mb: None,
                processes: Some(64),
                cpu_percent: None
            }
        );
    }

    #[test]
    fn describe_and_view() {
        let l = Limits {
            memory_mb: Some(2048),
            processes: Some(256),
            cpu_percent: Some(50),
        };
        assert_eq!(l.describe(), "内存 2048 MB、进程 256 个、CPU 50%");
        assert_eq!(Limits::default().describe(), "不限");
        assert_eq!(
            l.view(),
            json!({"memoryMb": 2048, "processes": 256, "cpuPercent": 50})
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn systemd_properties_reset_what_is_not_limited() {
        let p = linux::properties(&Limits::default(), true);
        assert_eq!(
            p,
            [
                "MemoryMax=infinity",
                "MemorySwapMax=infinity",
                "TasksMax=infinity",
                "CPUQuota="
            ]
        );
        let p = linux::properties(
            &Limits {
                memory_mb: Some(512),
                processes: Some(10),
                cpu_percent: None,
            },
            false,
        );
        assert_eq!(p, ["MemoryMax=512M", "MemorySwapMax=0", "TasksMax=320"]);
    }
}
