//! Ubuntu's AppArmor restriction on unprivileged user namespaces (device #122, Cloud #2383). Ubuntu 23.10+ enables
//! `kernel.apparmor_restrict_unprivileged_userns`. An unconfined process that makes a user namespace then gets no
//! capabilities in it, so the device's own bubblewrap cannot mount anything. The packaged `/usr/bin/bwrap` is exempt
//! through Ubuntu's `bwrap-userns-restrict` profile, but that profile stacks every child with `unpriv_bwrap` (`audit deny
//! capability`). srt's seccomp helper (`apply-seccomp`) runs inside bubblewrap and makes a nested namespace, so it fails
//! (`setgroups … Permission denied`).
//!
//! Least privilege (coordinator decision; srt README, "Ubuntu 24.04+ note"): never the global sysctl, and never the
//! seccomp layer off. One AppArmor profile, in the pattern of Ubuntu's own per-application profiles (`abi <abi/4.0>`,
//! `flags=(unconfined)`, `userns,`, as `/etc/apparmor.d/chrome` and `ch-run`), grants `userns` to exactly one program.
//! That program is a **root-owned** copy of the device's pinned bubblewrap ([`runtimes::BWRAP`]) under [`ROOT`].
//! Everything bubblewrap starts inherits the profile:
//! - srt's relay script and socat;
//! - `apply-seccomp`, which makes the nested namespace;
//! - the command, which runs under the helper's filter. The filter refuses new user namespaces, so the grant ends at
//!   the helper.
//!
//! The copy is never in a folder the person's processes can write: the restriction exists to keep arbitrary programs
//! away from capability-bearing namespaces, and a profile naming a path under the home folder would hand that to
//! anything that drops a file there.
//!
//! `sudo agentrouter sandbox userns install` sets it up ([`install`]; idempotent) and `… userns remove` undoes it
//! ([`remove`]). [`crate::Runtime::status`] names the line ([`admin_line`]) when the restriction is on and the copy or
//! the profile is missing.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::runtimes;

/// Where [`install`] puts the root-owned bubblewrap (`<ROOT>/bwrap-<version>/bwrap`).
pub const ROOT: &str = "/usr/local/libexec/agentrouter";
/// The profile [`install`] writes. The apparmor service loads it again at every boot, like Ubuntu's own.
pub const PROFILE_FILE: &str = "/etc/apparmor.d/agentrouter-bwrap";
/// The profile's name (`cat /proc/<pid>/attr/current` inside the sandbox shows `agentrouter-bwrap (unconfined)`).
pub const PROFILE_NAME: &str = "agentrouter-bwrap";
const RESTRICT_SYSCTL: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Whether AppArmor restricts unprivileged user namespaces on this computer (Ubuntu 23.10+ by default).
pub fn restricted() -> bool {
    read_trim(RESTRICT_SYSCTL).as_deref() == Some("1")
}

/// The root-owned bubblewrap the profile names.
pub fn bwrap() -> PathBuf {
    runtimes::BWRAP.bin_dir(Path::new(ROOT)).join("bwrap")
}

/// The profile for this pin: one exact path, no glob.
pub fn profile() -> String {
    format!(
        "# AgentRouter folder mode (agentrouter-device #122). Written by `agentrouter sandbox userns install`;\n\
         # `agentrouter sandbox userns remove` deletes it again.\n\
         #\n\
         # Ubuntu restricts unprivileged user namespaces. AgentRouter confines every local tool with bubblewrap and\n\
         # the sandbox-runtime seccomp helper, and both need a user namespace with capabilities. This profile grants\n\
         # that to one program only: AgentRouter's pinned, root-owned bubblewrap below. Like Ubuntu's own per-application\n\
         # profiles, it allows everything else and exists only to add `userns`.\n\
         \n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile {PROFILE_NAME} {path} flags=(unconfined) {{\n\
         \x20 userns,\n\
         \n\
         \x20 # Site-specific additions and overrides. See local/README for details.\n\
         \x20 include if exists <local/{PROFILE_NAME}>\n\
         }}\n",
        path = bwrap().display()
    )
}

/// Owned by root and writable by nobody else: the file, and every folder from it up to `/`.
#[cfg(unix)]
fn root_only(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0))
}

#[cfg(not(unix))]
fn root_only(_: &Path) -> bool {
    false
}

/// The root-owned copy of this pin and its profile are in place. Whether the profile is loaded only root can read; a
/// confined start that still fails says so ([`crate::Runtime::status`]), and [`install`] loads it again.
pub fn installed() -> bool {
    runtimes::BWRAP.installed(Path::new(ROOT))
        && root_only(&bwrap())
        && std::fs::read_to_string(PROFILE_FILE).is_ok_and(|t| t == profile())
}

/// The line the person runs (`sudo <this program> sandbox userns install`), with the absolute path of the running
/// executable (`sudo` does not search the person's PATH).
pub fn admin_line(action: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "agentrouter".into());
    format!("sudo {} sandbox userns {action}", shell_word(&exe))
}

/// `word` as one shell word (single quotes when it holds anything but plain path characters).
fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+@:".contains(c))
    {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// A kernel switch (not AppArmor) that refuses unprivileged user namespaces, and the persistent line that allows them
/// (older Debian's `unprivileged_userns_clone`, `max_user_namespaces` set to 0).
pub fn kernel_line() -> Option<String> {
    let line = |setting: &str| {
        format!(
            "echo {setting} | sudo tee /etc/sysctl.d/60-agentrouter-userns.conf && sudo sysctl -p /etc/sysctl.d/60-agentrouter-userns.conf"
        )
    };
    if read_trim("/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
        return Some(line("kernel.unprivileged_userns_clone=1"));
    }
    if read_trim("/proc/sys/user/max_user_namespaces").as_deref() == Some("0") {
        return Some(line("user.max_user_namespaces=10000"));
    }
    None
}

/// The effective user is root (`sudo`).
fn is_root() -> bool {
    read_trim("/proc/self/status")
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|ids| ids.split_whitespace().nth(1).map(|e| e == "0"))
        })
        .unwrap_or(false)
}

fn apparmor_parser() -> Result<PathBuf, String> {
    ["/usr/sbin/apparmor_parser", "/sbin/apparmor_parser"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .or_else(|| crate::find_program("apparmor_parser", None))
        .ok_or_else(|| {
            "这台电脑没有 apparmor_parser（AppArmor 的工具），用不着这一步：只有开着 AppArmor 用户命名空间限制的系统（Ubuntu 23.10 起）才需要".to_string()
        })
}

fn parser_run(parser: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(parser)
        .args(args)
        .output()
        .map_err(|e| format!("apparmor_parser 起不来：{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "apparmor_parser {} 没成功（退出码 {:?}）：{}{}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("改不了 {} 的权限：{e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<(), String> {
    Ok(())
}

/// `sudo agentrouter sandbox userns install`: the pinned bubblewrap, root-owned under [`ROOT`] (downloaded and checked
/// against its pinned SHA-256 as `sandbox setup` does; an older pin's folder is removed), [`PROFILE_FILE`], and the
/// profile loaded (`apparmor_parser --replace`). Idempotent: run again, it changes nothing and loads the profile again.
pub fn install() -> Result<Vec<String>, String> {
    if !is_root() {
        return Err(format!(
            "要用管理员权限运行，在终端输入这一行（会要电脑的登录密码）：{}",
            admin_line("install")
        ));
    }
    let parser = apparmor_parser()?;
    let mut done = Vec::new();
    let root = Path::new(ROOT);
    std::fs::create_dir_all(root).map_err(|e| format!("建不了 {ROOT}：{e}"))?;
    set_mode(root, 0o755)?;
    if runtimes::BWRAP.ensure(root)? {
        done.push(format!(
            "装好了 AgentRouter 自己的 bubblewrap {}：{}（root 所有，别的程序改不了）",
            runtimes::BWRAP.version,
            bwrap().display()
        ));
    }
    let dir = runtimes::BWRAP.dir(root);
    set_mode(&dir, 0o755)?;
    set_mode(&bwrap(), 0o755)?;
    if !root_only(&bwrap()) {
        return Err(format!(
            "{} 或它上面的文件夹不是只有 root 能改，不给它加 AppArmor 规则",
            bwrap().display()
        ));
    }
    let text = profile();
    if std::fs::read_to_string(PROFILE_FILE).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(PROFILE_FILE, &text).map_err(|e| format!("写不了 {PROFILE_FILE}：{e}"))?;
        set_mode(Path::new(PROFILE_FILE), 0o644)?;
        done.push(format!(
            "写好了 AppArmor 规则 {PROFILE_FILE}：只让上面这一个程序建用户命名空间"
        ));
    }
    parser_run(&parser, &["--replace", PROFILE_FILE])?;
    done.push(format!(
        "AppArmor 已加载规则 {PROFILE_NAME}（开机自动加载）。撤销：{}",
        admin_line("remove")
    ));
    Ok(done)
}

/// `sudo agentrouter sandbox userns remove`: unloads and deletes the profile and deletes [`ROOT`]. Folder mode then
/// asks for [`install`] again wherever the restriction is on.
pub fn remove() -> Result<Vec<String>, String> {
    if !is_root() {
        return Err(format!(
            "要用管理员权限运行，在终端输入这一行（会要电脑的登录密码）：{}",
            admin_line("remove")
        ));
    }
    let mut done = Vec::new();
    if Path::new(PROFILE_FILE).exists() {
        let parser = apparmor_parser()?;
        // Not loaded (after a reboot without the apparmor service, say) is fine: the file still goes.
        let unloaded = parser_run(&parser, &["--remove", PROFILE_FILE]);
        std::fs::remove_file(PROFILE_FILE).map_err(|e| format!("删不掉 {PROFILE_FILE}：{e}"))?;
        done.push(match unloaded {
            Ok(()) => format!("卸下并删掉了 AppArmor 规则 {PROFILE_NAME}（{PROFILE_FILE}）"),
            Err(e) => format!("删掉了 {PROFILE_FILE}（它本来就没加载：{e}）"),
        });
    }
    if Path::new(ROOT).exists() {
        std::fs::remove_dir_all(ROOT).map_err(|e| format!("删不掉 {ROOT}：{e}"))?;
        done.push(format!("删掉了 {ROOT}"));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_grants_userns_to_one_exact_root_owned_path() {
        let text = profile();
        let path = format!("{ROOT}/bwrap-{}/bwrap", runtimes::BWRAP.version);
        assert!(text.contains("abi <abi/4.0>,"));
        assert!(text.contains(&format!(
            "profile {PROFILE_NAME} {path} flags=(unconfined) {{"
        )));
        assert!(text.contains("\n  userns,\n"));
        assert!(text.contains(&format!("include if exists <local/{PROFILE_NAME}>")));
        // One program: no glob in the attachment, nothing under a home folder.
        assert!(!path.contains(['*', '?', '{', '[']));
        assert!(path.starts_with("/usr/local/libexec/"));
        assert_eq!(bwrap(), PathBuf::from(&path));
        // Only Ubuntu's restriction lifted for it: no capability, file or network rule of its own.
        let rules: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(
            rules,
            [
                "abi <abi/4.0>,",
                "include <tunables/global>",
                &*format!("profile {PROFILE_NAME} {path} flags=(unconfined) {{"),
                "userns,",
                &*format!("include if exists <local/{PROFILE_NAME}>"),
                "}",
            ]
        );
    }

    #[test]
    fn the_admin_line_is_one_shell_command_with_the_absolute_program() {
        assert_eq!(
            shell_word(
                "/home/a/.npm/_npx/1/node_modules/@agentrouter-top/cli-linux-x64/bin/agentrouter"
            ),
            "/home/a/.npm/_npx/1/node_modules/@agentrouter-top/cli-linux-x64/bin/agentrouter"
        );
        assert_eq!(
            shell_word("/home/a b/agentrouter"),
            "'/home/a b/agentrouter'"
        );
        assert_eq!(shell_word("/x/it's"), r"'/x/it'\''s'");
        let line = admin_line("install");
        assert!(
            line.starts_with("sudo /") || line.starts_with("sudo '/"),
            "{line}"
        );
        assert!(line.ends_with(" sandbox userns install"));
    }
}
