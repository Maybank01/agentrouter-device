//! Codes from the one error catalog (Cloud `contracts/errors/catalog.json`, #2449; owner 2026-10-21
//! 「还有，错误那一块，是不是应该有一种统一的方式？我不想错误都散落在各处」). The pinned copy is
//! `error-catalog.json` (`scripts/sync-error-catalog.mjs`); Control fills in component, retry and next
//! from it, and the device's own words travel whole as the cause. Never a bare `FAILED`. Every code literal in the
//! crates is an entry there (a test below scans them, Cloud #2735).

/// The arguments of a call are not what the tool takes; the message names the argument and what it must be.
pub const HOSTD_ARGS_INVALID: &str = "HOSTD_ARGS_INVALID";
/// The write boundary (Windows integrity label) could not be set, so the command did not run.
pub const HOSTD_BOUNDARY_FAILED: &str = "HOSTD_BOUNDARY_FAILED";
/// A fault on this computer (a file could not be written, …); retrying will not help.
pub const HOSTD_FAULT: &str = "HOSTD_FAULT";

/// The source the pinned catalog copy was taken from (`agentrouter-cloud@<commit>`).
pub fn catalog_source() -> String {
    let catalog: serde_json::Value =
        serde_json::from_str(include_str!("error-catalog.json")).unwrap_or_default();
    catalog["source"].as_str().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    fn catalog_codes() -> BTreeSet<String> {
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("error-catalog.json")).unwrap();
        catalog["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["code"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn every_code_is_in_the_pinned_catalog() {
        let codes = catalog_codes();
        for code in [HOSTD_ARGS_INVALID, HOSTD_BOUNDARY_FAILED, HOSTD_FAULT] {
            assert!(codes.contains(code), "{code} is not in the pinned catalog");
        }
        assert!(catalog_source().starts_with("agentrouter-cloud@"));
    }

    /// Upper-case literals in the crates that are not error codes: environment variable names, git refs, Windows
    /// names, Node's module error codes we read, and the audit label of an MCP error that carried no code.
    const NOT_CODES: &[&str] = &[
        "APPDATA",
        "BUN_INSTALL_CACHE_DIR",
        "CI",
        "CLAUDE_CODE_TMPDIR",
        "COMMIT", // SQL (hostd-link outbox transaction)
        "COMPUTERNAME",
        "COREPACK_HOME",
        "CSV",
        "DENO_DIR",
        "ERROR",
        "ERR_MODULE_NOT_FOUND",
        "GOCACHE",
        "GOMODCACHE",
        "HEAD",
        "HOME",
        "HOSTNAME",
        "LC_ALL",
        "LOCALAPPDATA",
        "MAVEN_ARGS",
        "MODULE_NOT_FOUND",
        "NPM_CONFIG_USERCONFIG",
        "NUGET_PACKAGES",
        "NUL",
        "OLDPWD",
        "OUT_DIR",
        "PATH",
        "POETRY_CACHE_DIR",
        "PWD",
        "PYTHONIOENCODING",
        "PYTHONUTF8",
        "ROLLBACK", // SQL (hostd-link outbox transaction)
        "RUSTUP_HOME",
        "SHELL",
        "SHLVL",
        "SRT_",
        "TEMP",
        "TERM",
        "TMP",
        "TMPDIR",
        "USERDOMAIN",
        "USERNAME",
        "USERPROFILE",
        "ZDOTDIR",
    ];
    /// Families of environment variable names.
    const NOT_CODE_PREFIXES: &[&str] = &[
        "AGENTROUTER_",
        "CARGO_",
        "GIT_",
        "PIP_",
        "UV_",
        "XDG_",
        "YARN_",
    ];

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `"UPPER_SNAKE"` literals in a source text, up to its first `#[cfg(test)] mod … {`.
    fn code_literals(text: &str) -> Vec<String> {
        // Cut at the first inline test module (`#[cfg(test)] mod x {`); `mod x;` declarations are skipped over.
        let mut body = text;
        let mut from = 0;
        while let Some(at) = text[from..].find("#[cfg(test)]\nmod ") {
            let at = from + at;
            let rest = &text[at..];
            if rest.find('{').unwrap_or(usize::MAX) < rest.find(';').unwrap_or(usize::MAX) {
                body = &text[..at];
                break;
            }
            from = at + 1;
        }
        let bytes = body.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len()
                    && (bytes[j].is_ascii_uppercase()
                        || bytes[j].is_ascii_digit()
                        || bytes[j] == b'_')
                {
                    j += 1;
                }
                if j < bytes.len()
                    && bytes[j] == b'"'
                    && j - start >= 2
                    && bytes[start].is_ascii_uppercase()
                {
                    out.push(body[start..j].to_string());
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// One catalog (Cloud #2449, #2735): every code this repository can send is an entry of Cloud's
    /// contracts/errors/catalog.json. A new code starts there; then `node scripts/sync-error-catalog.mjs`.
    #[test]
    fn every_code_literal_in_the_crates_is_a_catalog_entry() {
        let codes = catalog_codes();
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for krate in std::fs::read_dir(&crates).unwrap().flatten() {
            let src = krate.path().join("src");
            if src.is_dir() {
                rust_sources(&src, &mut files);
            }
        }
        assert!(
            files.len() > 20,
            "found only {} source files under {}",
            files.len(),
            crates.display()
        );
        let mut missing = BTreeSet::new();
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            for literal in code_literals(&text) {
                let exempt = NOT_CODES.contains(&literal.as_str())
                    || NOT_CODE_PREFIXES.iter().any(|p| literal.starts_with(p))
                    || literal.ends_with("_PROXY");
                if !exempt && !codes.contains(&literal) {
                    missing.insert(format!("{literal} ({})", file.display()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "codes that are not in the pinned catalog (add them to Cloud contracts/errors/catalog.json, then node scripts/sync-error-catalog.mjs; or, if not an error code, to NOT_CODES): {missing:?}"
        );
    }

    #[test]
    fn literals_are_found_and_test_modules_skipped() {
        let text = "#[cfg(test)]\nmod other;\nlet a = \"NOT_FOUND\"; let b = \"x Y\"; let c = \"lower\";\n#[cfg(test)]\nmod tests { \"ONLY_IN_TESTS\" }";
        assert_eq!(code_literals(text), vec!["NOT_FOUND".to_string()]);
    }
}
