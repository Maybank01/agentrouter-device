//! `info`: what a session bound to this computer must know before it acts (PROJECT-SERVICE §10.6 "Windows hosts"):
//! the OS, the shell commands run in (and so which syntax to write), the path separator, the default line ending
//! for new files, whether paths are case-insensitive, the linked folders, and which common programs are on PATH.

use serde_json::{Value, json};

use super::{Host, shell_name, shown, tool};

pub(super) fn tools() -> Vec<Value> {
    vec![tool(
        "info",
        "This computer: OS and architecture, the shell commands run in (PowerShell on Windows, /bin/sh elsewhere), path separator, default line ending for new files, case-insensitive paths, the linked folders (the first is home), temp dir, and common programs on PATH.",
        json!({"type": "object", "properties": {}}),
        true,
    )]
}

/// The OS's own name and version where it says so cheaply.
fn os_version() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/etc/os-release").ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
            .map(|v| v.trim_matches('"').to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        Some(format!(
            "macOS {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ))
    }
    #[cfg(windows)]
    {
        // `ver` prints the build ("Microsoft Windows [Version 10.0.26100.…]").
        let out = std::process::Command::new("cmd")
            .args(["/d", "/c", "ver"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

fn on_path(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| {
        let exe = if cfg!(windows) {
            format!("{n}.exe")
        } else {
            n.to_string()
        };
        agentrouter_device::shell::find_on_path(&exe)
            .or_else(|| {
                cfg!(windows)
                    .then(|| agentrouter_device::shell::find_on_path(&format!("{n}.cmd")))
                    .flatten()
            })
            .map(|p| shown(&p))
    })
}

impl Host {
    pub(super) fn info(&self) -> Result<Value, String> {
        let windows = cfg!(windows);
        let shell_program = if windows {
            shown(&agentrouter_device::shell::default_shell().program)
        } else {
            "/bin/sh".to_string()
        };
        let programs: serde_json::Map<String, Value> = [
            ("git", &["git"][..]),
            ("node", &["node"]),
            ("npm", &["npm"]),
            ("python", &["python3", "python"]),
            ("pwsh", &["pwsh"]),
            ("bash", &["bash"]),
            ("rg", &["rg"]),
        ]
        .iter()
        .map(|(key, names)| (key.to_string(), json!(on_path(names))))
        .collect();
        Ok(json!({
            "os": std::env::consts::OS,
            "family": std::env::consts::FAMILY,
            "arch": std::env::consts::ARCH,
            "os_version": os_version(),
            "shell": shell_name(),
            "shell_program": shell_program,
            "path_separator": std::path::MAIN_SEPARATOR.to_string(),
            "path_list_separator": if windows { ";" } else { ":" },
            "line_ending": if windows { "crlf" } else { "lf" },
            "case_insensitive_paths": windows || cfg!(target_os = "macos"),
            "folders": self.folders.iter().map(|f| shown(f)).collect::<Vec<_>>(),
            "home": shown(&self.home()),
            "temp_dir": shown(&std::env::temp_dir()),
            "programs": programs,
            "version": env!("CARGO_PKG_VERSION"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{host_in, temp};

    #[test]
    fn info_names_the_os_shell_separator_and_line_ending() {
        let dir = temp("info");
        let host = host_in(&dir);
        let info = host.info().unwrap();
        assert_eq!(info["os"], std::env::consts::OS);
        if cfg!(windows) {
            assert!(["pwsh", "powershell"].contains(&info["shell"].as_str().unwrap()));
            assert_eq!(info["path_separator"], "\\");
            assert_eq!(info["line_ending"], "crlf");
        } else {
            assert_eq!(info["shell"], "sh");
            assert_eq!(info["path_separator"], "/");
            assert_eq!(info["line_ending"], "lf");
        }
        assert_eq!(info["home"], info["folders"][0]);
        assert!(info["programs"].as_object().unwrap().contains_key("git"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
