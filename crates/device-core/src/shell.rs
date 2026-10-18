//! Which shell `exec` uses on Windows (owner direction 2026-10-18: PowerShell 7 by default).
//!
//! Priority: an explicit local override (`AGENTROUTER_SHELL`), then `pwsh.exe` (PATH, then the
//! standard install directory), then Windows PowerShell 5.1 (`powershell.exe`) as the fallback.
//! Both shells get the same arguments (`-NoLogo -NoProfile -NonInteractive -Command <line>`).
//! The PSModulePath sanitising of #32 applies only to the 5.1 fallback; PowerShell 7 keeps the
//! environment it was started with, so its module discovery is not disturbed.
//!
//! The override is deliberately local-only (an environment variable of the connector process, like
//! the other device-side settings that are read from the machine); a config-file/UI field is a later
//! slice.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// PowerShell 7 (`pwsh.exe`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    WindowsPowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub kind: Kind,
    pub program: PathBuf,
}

impl Shell {
    pub fn is_pwsh(&self) -> bool {
        self.kind == Kind::Pwsh
    }

    /// The arguments both shells accept for a one-shot command line.
    pub fn args(&self, command: &str) -> Vec<String> {
        [
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            command,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
}

/// The shell choice: explicit override wins, then PowerShell 7, then Windows PowerShell 5.1.
pub fn pick_shell(
    explicit: Option<PathBuf>,
    pwsh: Option<PathBuf>,
    windows_powershell: Option<PathBuf>,
) -> Shell {
    if let Some(program) = explicit {
        return Shell {
            kind: kind_of(&program),
            program,
        };
    }
    if let Some(program) = pwsh {
        return Shell {
            kind: Kind::Pwsh,
            program,
        };
    }
    Shell {
        kind: Kind::WindowsPowerShell,
        program: windows_powershell.unwrap_or_else(|| PathBuf::from("powershell.exe")),
    }
}

fn kind_of(program: &Path) -> Kind {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("pwsh") {
        Kind::Pwsh
    } else {
        Kind::WindowsPowerShell
    }
}

fn existing(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

/// `pwsh.exe` from PATH, then the standard PowerShell 7 install locations.
pub fn find_pwsh() -> Option<PathBuf> {
    find_on_path("pwsh.exe").or_else(|| {
        let mut candidates = Vec::new();
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            candidates.push(
                PathBuf::from(program_files)
                    .join("PowerShell")
                    .join("7")
                    .join("pwsh.exe"),
            );
        }
        candidates.push(PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"));
        candidates.into_iter().find_map(|p| existing(&p))
    })
}

/// `powershell.exe` (Windows PowerShell 5.1) from PATH, then the system directory.
pub fn find_windows_powershell() -> Option<PathBuf> {
    find_on_path("powershell.exe").or_else(|| {
        let mut candidates = Vec::new();
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            candidates.push(
                PathBuf::from(system_root)
                    .join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe"),
            );
        }
        candidates.push(PathBuf::from(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
        ));
        candidates.into_iter().find_map(|p| existing(&p))
    })
}

/// A program named on PATH (an `.exe` name is expected on Windows).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| existing(&dir.join(name)))
}

/// Resolve an explicit override: an absolute/relative path, or a program name on PATH.
fn explicit(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim().trim_matches('"');
    if raw.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(raw);
    if candidate.is_file() {
        return Some(candidate);
    }
    if raw.contains(['\\', '/']) {
        return None;
    }
    find_on_path(raw).or_else(|| match raw.to_ascii_lowercase().as_str() {
        "pwsh" | "pwsh.exe" => find_pwsh(),
        "powershell" | "powershell.exe" => find_windows_powershell(),
        _ => None,
    })
}

/// The shell `exec` uses: `AGENTROUTER_SHELL` override, then PowerShell 7, then 5.1.
pub fn default_shell() -> Shell {
    let explicit = std::env::var("AGENTROUTER_SHELL")
        .ok()
        .and_then(|raw| explicit(&raw));
    pick_shell(explicit, find_pwsh(), find_windows_powershell())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pwsh7() -> PathBuf {
        PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe")
    }

    fn powershell51() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
    }

    #[test]
    fn powershell7_is_preferred_when_installed() {
        let shell = pick_shell(None, Some(pwsh7()), Some(powershell51()));
        assert_eq!(shell.kind, Kind::Pwsh);
        assert_eq!(shell.program, pwsh7());
        assert!(shell.is_pwsh());
        let args = shell.args("Get-Date");
        assert_eq!(args.first().map(String::as_str), Some("-NoLogo"));
        assert_eq!(args.last().map(String::as_str), Some("Get-Date"));
    }

    #[test]
    fn windows_powershell_is_the_fallback_without_pwsh() {
        let shell = pick_shell(None, None, Some(powershell51()));
        assert_eq!(shell.kind, Kind::WindowsPowerShell);
        assert_eq!(shell.program, powershell51());
        let bare = pick_shell(None, None, None);
        assert_eq!(bare.kind, Kind::WindowsPowerShell);
        assert_eq!(bare.program, PathBuf::from("powershell.exe"));
    }

    #[test]
    fn an_explicit_override_wins_and_is_typed_by_name() {
        let explicit_51 = pick_shell(Some(powershell51()), Some(pwsh7()), None);
        assert_eq!(explicit_51.kind, Kind::WindowsPowerShell);
        assert_eq!(explicit_51.program, powershell51());
        let explicit_pwsh = pick_shell(Some(pwsh7()), None, Some(powershell51()));
        assert_eq!(explicit_pwsh.kind, Kind::Pwsh);
    }

    #[test]
    fn resolution_of_names_uses_the_expected_kind() {
        // Resolution against this machine's real installation; the assertion is about typing only.
        if let Some(path) = find_pwsh() {
            assert_eq!(kind_of(&path), Kind::Pwsh);
        }
        if let Some(path) = find_windows_powershell() {
            assert_eq!(kind_of(&path), Kind::WindowsPowerShell);
        }
    }
}
