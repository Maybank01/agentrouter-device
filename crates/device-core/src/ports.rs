//! Who listens on a loopback port (device #139, HOSTD-MCP §3.8): in folder mode on Windows and macOS the sandbox shares
//! the computer's loopback, so `hostd` can dial a dev server the Agent started — but a preview stream may only reach a
//! server of the link's own sandbox, never one of the person's own programs (their database admin page, a local
//! service). The listeners of a port come from the OS's socket table through the `listeners` crate (MIT;
//! `GetExtendedTcpTable` on Windows, libproc on macOS); the caller says which processes are the sandbox's (Windows: the
//! link's Job objects, #129; macOS: the servers' process trees and sessions, [`in_trees`]).
//!
//! This keeps a preview from naming the person's port by accident or by a guess. It is not a wall against a command
//! that relays such a port itself: with loopback open to the sandbox (macOS, and Windows since #139), a command can
//! already connect to it, as in the person's own terminal.

use std::collections::HashSet;
use std::net::IpAddr;

/// Who listens on one TCP port of this computer's loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listening {
    /// Nothing (on loopback or every address).
    Nothing,
    /// Only processes `ours` accepts.
    Ours,
    /// At least one other process (its pids).
    Foreign(Vec<u32>),
}

/// The pids of the TCP listeners a connect to `127.0.0.1:<port>` or `[::1]:<port>` can land on (bound to loopback or
/// to every address).
pub fn listeners(port: u16) -> Result<Vec<u32>, String> {
    let all = listeners::get_all().map_err(|e| format!("读不了本机端口表：{e}"))?;
    let mut pids: Vec<u32> = all
        .into_iter()
        .filter(|l| {
            l.protocol == listeners::Protocol::TCP
                && l.state == listeners::SocketState::Listen
                && l.socket.port() == port
                && reaches_loopback(l.socket.ip())
        })
        .map(|l| l.process.pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn reaches_loopback(ip: IpAddr) -> bool {
    ip.is_loopback() || ip.is_unspecified()
}

/// [`listeners`] judged by `ours`.
pub fn who_listens(port: u16, ours: impl Fn(u32) -> bool) -> Result<Listening, String> {
    Ok(judge(&listeners(port)?, ours))
}

fn judge(pids: &[u32], ours: impl Fn(u32) -> bool) -> Listening {
    if pids.is_empty() {
        return Listening::Nothing;
    }
    let foreign: Vec<u32> = pids.iter().copied().filter(|p| !ours(*p)).collect();
    if foreign.is_empty() {
        Listening::Ours
    } else {
        Listening::Foreign(foreign)
    }
}

/// Unix: the processes of the servers started as `roots` — each one whose parent chain reaches a root, or whose
/// session leader's does (a dev server whose starting shell exited is re-parented to init but keeps its session; the
/// servers start in sessions of their own, device #139).
#[cfg(unix)]
pub fn in_trees(roots: &[u32]) -> impl Fn(u32) -> bool + use<> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let roots: HashSet<u32> = roots.iter().copied().filter(|p| *p > 1).collect();
    move |pid: u32| {
        let reaches = |start: Pid| {
            let mut cur = Some(start);
            for _ in 0..128 {
                let Some(p) = cur else { return false };
                if roots.contains(&p.as_u32()) {
                    return true;
                }
                cur = sys.process(p).and_then(|x| x.parent());
            }
            false
        };
        let pid = Pid::from_u32(pid);
        reaches(pid)
            || sys
                .process(pid)
                .and_then(|p| p.session_id())
                .is_some_and(|sid| sid != pid && reaches(sid))
    }
}

/// Windows: the processes in the given Job objects' pid lists (#129: every confined server's tree is in its job).
pub fn in_sets(sets: Vec<Vec<u32>>) -> impl Fn(u32) -> bool {
    let all: HashSet<u32> = sets.into_iter().flatten().collect();
    move |pid| all.contains(&pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_ours_only_when_every_listener_is() {
        let ours = |p: u32| p == 10 || p == 11;
        assert_eq!(judge(&[], ours), Listening::Nothing);
        assert_eq!(judge(&[10, 11], ours), Listening::Ours);
        assert_eq!(judge(&[10, 42], ours), Listening::Foreign(vec![42]));
    }

    #[test]
    fn a_listener_on_this_process_is_found_and_judged() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id();
        assert_eq!(who_listens(port, |p| p == me).unwrap(), Listening::Ours);
        assert_eq!(
            who_listens(port, |_| false).unwrap(),
            Listening::Foreign(vec![me])
        );
        drop(l);
        assert_eq!(who_listens(port, |_| true).unwrap(), Listening::Nothing);
    }

    #[cfg(unix)]
    #[test]
    fn a_child_is_in_its_parent_s_tree() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let ours = in_trees(&[std::process::id()]);
        assert!(ours(child.id()));
        assert!(!in_trees(&[999_999])(child.id()));
        let _ = child.kill();
        let _ = child.wait();
    }
}
