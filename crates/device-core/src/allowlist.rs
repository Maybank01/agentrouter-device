//! The coding default (DEVICE-PROTOCOL.md §5.5): read-only commands that run without asking at the
//! 「只限这些文件夹」 level, and what 「本对话同类允许」 means for a command.
//!
//! A command is auto-approved only when it is one plain command (no `;`, `&`, `|`, redirection,
//! variables, subexpressions or backticks), its program is on the list, it has no option that writes
//! files or runs other programs, and every path it names is inside the allowed folders. Anything else
//! is asked as usual. The list is deliberately short; it can be turned off on the device.

use std::path::Path;

use crate::gate::{Scope, display};

/// Characters that make a command more than one plain command, or expand something.
const SHELL_SPECIAL: &[char] = &[
    ';', '&', '|', '<', '>', '`', '$', '(', ')', '{', '}', '@', '%', '^', '!', '\n', '\r', '\0',
];

/// The programs on the list (lower case, without `.exe`).
pub const READONLY_PROGRAMS: &[&str] = &[
    "git",
    "rg",
    "ls",
    "dir",
    "pwd",
    "cat",
    "type",
    "get-childitem",
    "get-content",
    "get-location",
];

const GIT_READ: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "ls-files",
    "rev-parse",
    "blame",
    "branch",
];
/// `git branch` only lists with these.
const GIT_BRANCH_LIST: &[&str] = &[
    "-a",
    "-r",
    "-v",
    "-vv",
    "--list",
    "--show-current",
    "--all",
    "--remotes",
    "--verbose",
    "--no-color",
];
/// Options that write a file, read one from anywhere, or start another program.
const GIT_DENIED: &[&str] = &[
    "--output",
    "--ext-diff",
    "--textconv",
    "--no-index",
    "--show-signature",
    "--contents",
    "--exec",
    "--upload-pack",
    "--open-files-in-pager",
];
const RG_DENIED: &[&str] = &[
    "--pre",
    "--search-zip",
    "--hostname-bin",
    "--file",
    "--ignore-file",
    "--files-from",
];

fn has_special(command: &str) -> bool {
    command.contains(SHELL_SPECIAL)
}

/// Split into words; quotes group words (no escapes inside). `None` for an unclosed quote.
fn words(command: &str) -> Option<Vec<(String, bool)>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut in_quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match in_quote {
            Some(q) if c == q => in_quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                in_quote = Some(c);
                quoted = true;
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push((std::mem::take(&mut current), quoted));
                    quoted = false;
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if in_quote.is_some() {
        return None;
    }
    if started {
        out.push((current, quoted));
    }
    Some(out)
}

fn program_name(word: &str) -> String {
    let lower = word.to_ascii_lowercase();
    lower
        .strip_suffix(".exe")
        .map(str::to_string)
        .unwrap_or(lower)
}

/// Is `word` a path inside the allowed folders (relative ones from `cwd`)? Wildcards check the part
/// before them.
fn path_inside(word: &str, cwd: &Path, scope: &Scope) -> bool {
    if word.starts_with('~') {
        return false;
    }
    let cut = word.find(['*', '?', '[']).unwrap_or(word.len());
    let base = &word[..cut];
    // The folder part before a wildcard (`src/*.rs` → `src/`).
    let base = match base.rfind(['/', '\\']) {
        Some(i) if cut < word.len() => &base[..=i],
        _ if cut < word.len() => "",
        _ => base,
    };
    let absolute = Path::new(base).is_absolute() || base.starts_with('/') || base.starts_with('\\');
    let full = if absolute {
        base.to_string()
    } else {
        let root = display(cwd);
        let sep = if cfg!(windows) { '\\' } else { '/' };
        if base.is_empty() {
            root
        } else {
            format!("{}{sep}{base}", root.trim_end_matches(['/', '\\']))
        }
    };
    let full = full.trim_end_matches(['/', '\\']);
    let full = if full.is_empty() || full.ends_with(':') {
        format!("{full}{}", std::path::MAIN_SEPARATOR)
    } else {
        full.to_string()
    };
    scope.check(&full).is_ok()
}

/// May this command run without asking (the read-only list)? `cwd` is the real folder it runs in.
pub fn readonly_allowed(command: &str, cwd: &Path, scope: &Scope) -> bool {
    if has_special(command) {
        return false;
    }
    let Some(words) = words(command) else {
        return false;
    };
    let Some(((first, first_quoted), rest)) = words.split_first() else {
        return false;
    };
    if *first_quoted {
        return false;
    }
    let program = program_name(first);
    if !READONLY_PROGRAMS.contains(&program.as_str()) {
        return false;
    }
    let is_flag = |w: &str, quoted: bool| !quoted && w.starts_with('-') && w.len() > 1;
    match program.as_str() {
        "git" => {
            let Some(((sub, false), args)) = rest.split_first() else {
                return false;
            };
            if !GIT_READ.contains(&sub.as_str()) {
                return false;
            }
            for (w, quoted) in args {
                if is_flag(w, *quoted) {
                    let lower = w.to_ascii_lowercase();
                    if GIT_DENIED.iter().any(|d| lower.starts_with(d)) {
                        return false;
                    }
                    if sub == "branch" && !GIT_BRANCH_LIST.contains(&lower.as_str()) {
                        return false;
                    }
                    continue;
                }
                if sub == "branch" {
                    return false; // `git branch <name>` creates one
                }
                // Revisions and pathspecs; only something that looks like a path elsewhere is checked.
                let looks_like_path = w.contains("..")
                    || w.starts_with(['/', '\\', '~'])
                    || Path::new(w).is_absolute();
                if looks_like_path && !w.contains(':') && !path_inside(w, cwd, scope) {
                    return false;
                }
                if w.starts_with('~') || (w.len() >= 2 && w.as_bytes()[1] == b':') {
                    return false;
                }
            }
            true
        }
        "rg" => {
            let explicit_pattern = rest
                .iter()
                .any(|(w, q)| !q && (w == "-e" || w.starts_with("--regexp")));
            let mut pattern_seen = explicit_pattern;
            for (w, quoted) in rest {
                if is_flag(w, *quoted) {
                    let lower = w.as_str();
                    let name = lower.split('=').next().unwrap_or(lower);
                    if RG_DENIED.contains(&name) {
                        return false;
                    }
                    // Short clusters with -f (patterns from a file) or -z (runs decompressors).
                    if !lower.starts_with("--") && lower[1..].contains(['f', 'z']) {
                        return false;
                    }
                    continue;
                }
                if !pattern_seen {
                    pattern_seen = true;
                    continue;
                }
                if !path_inside(w, cwd, scope) {
                    return false;
                }
            }
            true
        }
        _ => rest
            .iter()
            .all(|(w, quoted)| is_flag(w, *quoted) || path_inside(w, cwd, scope)),
    }
}

/// What 「本对话同类允许」 covers for this command: its leading plain words (`npm test`, `cargo build`,
/// `git commit`), at most three. `None` when only this exact command may repeat: combined commands,
/// and a single word followed by anything else (`python -c …`, `pytest tests/x`).
pub fn command_kind(command: &str) -> Option<String> {
    if has_special(command) {
        return None;
    }
    let words = words(command)?;
    let plain = |w: &str| {
        w.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '+' | '-'))
    };
    let lead: Vec<&str> = words
        .iter()
        .take_while(|(w, q)| !q && plain(w))
        .take(3)
        .map(|(w, _)| w.as_str())
        .collect();
    if lead.len() < 2 {
        return None;
    }
    Some(lead.join(" "))
}

/// Does `command` fall under an allowed kind?
pub fn matches_kind(command: &str, kind: &str) -> bool {
    match command_kind(command) {
        None => false,
        Some(_) => {
            let Some(words) = words(command) else {
                return false;
            };
            let want: Vec<&str> = kind.split(' ').collect();
            words.len() >= want.len()
                && words
                    .iter()
                    .zip(&want)
                    .all(|((w, quoted), k)| !quoted && w == k)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Access;

    fn scope_at(dir: &Path) -> Scope {
        Scope {
            access: Access::Folders,
            folders: vec![std::fs::canonicalize(dir).unwrap()],
            deny: Vec::new(),
        }
    }

    #[test]
    fn kinds() {
        assert_eq!(command_kind("npm test").as_deref(), Some("npm test"));
        assert_eq!(
            command_kind("npm run build").as_deref(),
            Some("npm run build")
        );
        assert_eq!(
            command_kind("cargo test --all").as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            command_kind("git commit -m 'x'").as_deref(),
            Some("git commit")
        );
        assert_eq!(command_kind("python -c 'import os'"), None);
        assert_eq!(command_kind("pytest tests/unit"), None);
        assert_eq!(command_kind("npm test; rm -rf /"), None);
        assert_eq!(command_kind("npm test && curl x"), None);
        assert!(matches_kind("cargo test -p x", "cargo test"));
        assert!(!matches_kind("cargo testx", "cargo test"));
        assert!(!matches_kind("cargo test | sh", "cargo test"));
        assert!(!matches_kind("cargo", "cargo test"));
    }

    #[test]
    fn readonly_list() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/unit-tmp/allowlist-unit");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let scope = scope_at(&dir);
        let cwd = std::fs::canonicalize(&dir).unwrap();
        let ok = |c: &str| readonly_allowed(c, &cwd, &scope);
        for c in [
            "git status",
            "git diff --stat",
            "git log --oneline -5",
            "git show HEAD~1:src/main.rs",
            "git diff main..feature",
            "git branch -a",
            "ls",
            "ls -la src",
            "dir src",
            "rg -n TODO",
            "rg -n 'fn main' src",
            "cat src/a.rs",
            "type src\\a.rs",
            "Get-ChildItem -Recurse src",
            "Get-Content src/*.rs",
            "pwd",
        ] {
            assert!(ok(c), "{c}");
        }
        for c in [
            "git push",
            "git branch new-feature",
            "git diff --output=x.txt",
            "git diff --ext-diff",
            "git diff --no-index a b",
            "git -C .. status",
            "git status; rm -rf x",
            "git status && echo",
            "ls | sh",
            "cat ../secret",
            "cat ~/.ssh/id_rsa",
            "cat $HOME/x",
            "cat /etc/passwd",
            "rg --pre sh x",
            "rg -z x",
            "rg -f list.txt",
            "rg x /etc",
            "rg x ..",
            "Get-Content (Get-Item x)",
            "npm test",
            "\"git\" status",
            "cat 'unclosed",
            "ls > out.txt",
        ] {
            assert!(!ok(c), "{c}");
        }
        if cfg!(windows) {
            for c in [
                "type C:\\Windows\\win.ini",
                "cat env:PATH",
                "ls Env:",
                "cat C:x",
            ] {
                assert!(!ok(c), "{c}");
            }
        }
        assert!(ok("rg --files"));
        assert!(ok("rg -l TODO"));
    }

    use std::path::PathBuf;
}
