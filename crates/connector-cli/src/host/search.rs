//! `search`: ripgrep on the device, from ripgrep's own crates (`ignore` walker, `grep-searcher`, `grep-regex`), so a
//! bound session's `glob`/`grep` give what local `rg` gives (Cloud PROJECT-SERVICE §10.7 item 4): .gitignore,
//! .ignore and .rgignore inside a git repository, hidden files skipped unless asked, binary files (a NUL byte) quit
//! when walked and converted when named, BOM-sniffed UTF-16, the Rust `regex` flavour, `--glob` overrides, and the
//! display paths `rg` prints (relative to the cwd, or the named path joined with the rest).
//!
//! Two modes, the two `rg` command lines DSH's `dsh-tool-fs-search` runs:
//! - `files` = `rg --files [--glob…] [--hidden] [--no-ignore] [--sort=modified] [-- path]`
//! - `content` = `rg --json --regexp=<pattern> [--glob…] [-- path]`, as match records.
//!
//! One difference by design: results are in path order (or modification time, ascending, ties in path order); a
//! multi-threaded `rg` without `--sort` has no stable order at all.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use serde_json::{Value, json};

use agentrouter_device::util::b64;

use super::{Call, Host, arg_bool, arg_str, shown, tool};

const RESULTS_DEFAULT: u64 = 10_000;
const RESULTS_MAX: u64 = 100_000;
/// A matched line is cut here (on a character boundary) unless asked otherwise; DSH previews 2000 bytes.
const LINE_BYTES_DEFAULT: u64 = 2000;
const LINE_BYTES_MAX: u64 = 64 << 10;
/// The answer stays well under the 7 MiB result cap of the aggregator.
const ANSWER_BYTES_MAX: usize = 4 << 20;
const TIMEOUT_DEFAULT_S: u64 = 60;
const TIMEOUT_MAX_S: u64 = 600;

pub(super) fn tools() -> Vec<Value> {
    vec![tool(
        "search",
        "ripgrep on this computer. mode files = `rg --files` (paths whose glob matches); mode content = `rg --regexp` (matching lines, Rust regex syntax). Respects .gitignore/.ignore/.rgignore and skips hidden and binary files like rg unless hidden/no_ignore. globs are rg --glob overrides (!x excludes). Paths are shown as rg shows them: relative to cwd (default the first folder), or under the given path.",
        json!({"type": "object", "properties": {
            "mode": {"type": "string", "enum": ["files", "content"]},
            "pattern": {"type": "string"},
            "path": {"type": "string"},
            "cwd": {"type": "string"},
            "globs": {"type": "array", "items": {"type": "string"}},
            "hidden": {"type": "boolean"},
            "no_ignore": {"type": "boolean"},
            "ignore_case": {"type": "boolean"},
            "fixed_strings": {"type": "boolean"},
            "sort": {"type": "string", "enum": ["path", "modified"]},
            "max_results": {"type": "integer", "minimum": 1, "maximum": RESULTS_MAX},
            "max_line_bytes": {"type": "integer", "minimum": 16, "maximum": LINE_BYTES_MAX},
            "timeout_s": {"type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX_S},
        }}),
        true,
    )]
}

/// Cut `text` to at most `max` bytes on a character boundary.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// The path as `rg` prints it: relative to the search root's display form.
struct Display {
    /// The absolute root that was walked.
    root: PathBuf,
    /// What the caller named (`None`: the cwd itself, shown without a `./` prefix).
    named: Option<PathBuf>,
}

impl Display {
    fn show(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let joined = match &self.named {
            None => rel.to_path_buf(),
            Some(named) if rel.as_os_str().is_empty() => named.clone(),
            Some(named) => named.join(rel),
        };
        shown(&joined)
    }
}

/// Collects match records like `rg --json`'s `match` lines (line text without its terminator).
struct Matches<'a> {
    path: String,
    out: &'a mut Vec<Value>,
    bytes: &'a mut usize,
    count: &'a mut u64,
    max_results: u64,
    max_line: usize,
    full: &'a mut bool,
}

impl Sink for Matches<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        *self.count += 1;
        if *self.full || self.out.len() as u64 >= self.max_results {
            *self.full = true;
            return Ok(true);
        }
        let mut line = mat.bytes();
        if let Some(rest) = line.strip_suffix(b"\n") {
            line = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let mut record = json!({"path": self.path, "line_number": mat.line_number()});
        match std::str::from_utf8(line) {
            Ok(text) => {
                let (text, clipped) = cut(text, self.max_line);
                record["text"] = json!(text);
                if clipped {
                    record["clipped"] = json!(true);
                }
            }
            // `rg --json` sends such a line as base64 `bytes`.
            Err(_) => record["bytes"] = json!(b64(&line[..line.len().min(self.max_line)])),
        }
        *self.bytes += record.to_string().len();
        if *self.bytes > ANSWER_BYTES_MAX {
            *self.full = true;
            return Ok(true);
        }
        self.out.push(record);
        Ok(true)
    }
}

impl Host {
    pub(super) fn search(&self, args: &Value, call: &Call) -> Result<Value, String> {
        let content = match arg_str(args, "mode").unwrap_or("content") {
            "content" => true,
            "files" => false,
            other => return Err(format!("mode 只能是 files 或 content，不是 {other}")),
        };
        let cwd = match arg_str(args, "cwd") {
            Some(c) if !c.trim().is_empty() => self.resolve(c)?,
            _ => self.home(),
        };
        let named = arg_str(args, "path")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        let root = match &named {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => cwd.join(p),
            None => cwd.clone(),
        };
        let root_meta = std::fs::metadata(&root).map_err(|e| format!("{}：{e}", shown(&root)))?;
        let display = Display {
            root: root.clone(),
            named,
        };
        let hidden = arg_bool(args, "hidden");
        let no_ignore = arg_bool(args, "no_ignore");
        let by_mtime = match arg_str(args, "sort").unwrap_or("path") {
            "path" => false,
            "modified" => true,
            other => return Err(format!("sort 只能是 path 或 modified，不是 {other}")),
        };
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(RESULTS_DEFAULT)
            .clamp(1, RESULTS_MAX);
        let max_line = args
            .get("max_line_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(LINE_BYTES_DEFAULT)
            .clamp(16, LINE_BYTES_MAX) as usize;
        let deadline = Instant::now()
            + Duration::from_secs(
                args.get("timeout_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(TIMEOUT_DEFAULT_S)
                    .clamp(1, TIMEOUT_MAX_S),
            );

        // `--glob` overrides are matched relative to the root walked, like rg's (rooted at the cwd for rg; the same
        // when no path is named, and patterns without a slash match at any depth either way).
        let mut overrides = ignore::overrides::OverrideBuilder::new(&cwd);
        for glob in args
            .get("globs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let glob = glob.as_str().ok_or("globs 只能是字符串")?;
            overrides
                .add(glob)
                .map_err(|e| format!("glob {glob} 无效：{e}"))?;
        }
        let overrides = overrides.build().map_err(|e| e.to_string())?;
        let mut walk = ignore::WalkBuilder::new(&root);
        walk.hidden(!hidden)
            .ignore(!no_ignore)
            .git_ignore(!no_ignore)
            .git_global(!no_ignore)
            .git_exclude(!no_ignore)
            .parents(!no_ignore)
            .follow_links(false)
            .overrides(overrides)
            .sort_by_file_name(|a, b| a.cmp(b));
        if !no_ignore {
            walk.add_custom_ignore_filename(".rgignore");
        }

        // Files in walk order (with their mtime when sorting by it), only regular files, like rg.
        let mut files: Vec<(PathBuf, Option<std::time::SystemTime>)> = Vec::new();
        let mut timed_out = false;
        for item in walk.build() {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if call.cancelled() {
                return Err("已取消".into());
            }
            let Ok(entry) = item else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let mtime = if by_mtime {
                entry.metadata().ok().and_then(|m| m.modified().ok())
            } else {
                None
            };
            files.push((entry.into_path(), mtime));
        }
        if by_mtime {
            // rg --sort=modified: ascending, files without a time last; a stable sort keeps path order on ties.
            files.sort_by(|(_, a), (_, b)| match (a, b) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
        }

        if !content {
            let count = files.len() as u64;
            let mut size = 0usize;
            let mut paths = Vec::new();
            for (path, _) in files.iter().take(max_results as usize) {
                let p = display.show(path);
                size += p.len() + 4;
                if size > ANSWER_BYTES_MAX {
                    break;
                }
                paths.push(p);
            }
            let truncated = (paths.len() as u64) < count;
            return Ok(json!({
                "root": shown(&root), "paths": paths, "count": count, "truncated": truncated, "timed_out": timed_out,
            }));
        }

        let pattern = arg_str(args, "pattern").unwrap_or("");
        if pattern.is_empty() {
            return Err("content 模式需要 pattern".into());
        }
        // ripgrep's matcher settings: ^/$ at line boundaries, Unicode on, no octal escapes, `\n` refused in a
        // line-oriented search.
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .unicode(true)
            .octal(false)
            .case_insensitive(arg_bool(args, "ignore_case"))
            .fixed_strings(arg_bool(args, "fixed_strings"))
            .line_terminator(Some(b'\n'))
            .build(pattern)
            .map_err(|e| format!("pattern 无效：{e}"))?;
        // A file named directly is searched with NULs converted (rg's explicit-path rule); walked files stop at the
        // first NUL (binary).
        let binary = if root_meta.is_file() {
            BinaryDetection::convert(b'\x00')
        } else {
            BinaryDetection::quit(b'\x00')
        };
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(binary)
            .bom_sniffing(true)
            .build();
        let mut matches = Vec::new();
        let mut bytes = 0usize;
        let mut count = 0u64;
        let mut full = false;
        let mut files_matched = 0u64;
        for (path, _) in &files {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            if call.cancelled() {
                return Err("已取消".into());
            }
            let before = count;
            let mut sink = Matches {
                path: display.show(path),
                out: &mut matches,
                bytes: &mut bytes,
                count: &mut count,
                max_results,
                max_line,
                full: &mut full,
            };
            // An unreadable file is skipped (rg reports it on stderr and goes on).
            let _ = searcher.search_path(&matcher, path, &mut sink);
            if count > before {
                files_matched += 1;
            }
        }
        Ok(json!({
            "root": shown(&root), "matches": matches, "count": count, "files": files_matched,
            "truncated": (matches.len() as u64) < count, "timed_out": timed_out,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Call;
    use super::super::tests::{host_in, temp};
    use super::*;

    fn search(host: &Host, args: Value) -> Result<Value, String> {
        host.search(&args, &Call::detached())
    }

    fn tree() -> PathBuf {
        let dir = temp("search");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn main() {}\nlet needle = 1;\r\n").unwrap();
        std::fs::write(dir.join("src/nested/b.ts"), "const needle = 2\n").unwrap();
        std::fs::write(dir.join("build/out.js"), "needle\n").unwrap();
        std::fs::write(dir.join(".hidden.txt"), "needle\n").unwrap();
        std::fs::write(dir.join("bin.dat"), b"needle\0\x01\x02").unwrap();
        // UTF-16LE with a BOM: rg transcodes it.
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "needle16\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(dir.join("src/wide.txt"), utf16).unwrap();
        dir
    }

    fn paths(v: &Value, key: &str) -> Vec<String> {
        v[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e.as_str()
                    .unwrap_or_else(|| e["path"].as_str().unwrap())
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn content_search_follows_ripgreps_rules() {
        let dir = tree();
        let host = host_in(&dir);
        let out = search(&host, json!({"pattern": "needle"})).unwrap();
        // .gitignore'd build/, the hidden file and the binary file are skipped; UTF-16 is transcoded.
        assert_eq!(
            paths(&out, "matches"),
            ["src/a.rs", "src/nested/b.ts", "src/wide.txt"]
        );
        assert_eq!(out["matches"][0]["line_number"], 2);
        assert_eq!(out["matches"][0]["text"], "let needle = 1;");
        assert_eq!(out["count"], 3);
        assert_eq!(out["files"], 3);
        // Named path: shown under it; a glob narrows; hidden/no_ignore widen.
        let src = search(
            &host,
            json!({"pattern": "needle", "path": "src", "globs": ["*.ts"]}),
        )
        .unwrap();
        assert_eq!(paths(&src, "matches"), ["src/nested/b.ts"]);
        let wide = search(
            &host,
            json!({"pattern": "needle", "hidden": true, "no_ignore": true, "globs": ["!.git"]}),
        )
        .unwrap();
        assert_eq!(
            paths(&wide, "matches"),
            [
                ".hidden.txt",
                "build/out.js",
                "src/a.rs",
                "src/nested/b.ts",
                "src/wide.txt"
            ]
        );
        // A binary file named directly is searched (NULs converted).
        let named = search(&host, json!({"pattern": "needle", "path": "bin.dat"})).unwrap();
        assert_eq!(paths(&named, "matches"), ["bin.dat"]);
        // Rust regex flavour; a literal newline is refused like rg.
        let re = search(&host, json!({"pattern": r"^const \w+ = \d$"})).unwrap();
        assert_eq!(paths(&re, "matches"), ["src/nested/b.ts"]);
        assert!(search(&host, json!({"pattern": "a\nb"})).is_err());
        let capped = search(&host, json!({"pattern": "needle", "max_results": 1})).unwrap();
        assert_eq!(capped["matches"].as_array().unwrap().len(), 1);
        assert_eq!(capped["truncated"], true);
        assert_eq!(capped["count"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_mode_is_rg_files_with_globs_and_sorting() {
        let dir = tree();
        let host = host_in(&dir);
        let out = search(&host, json!({"mode": "files", "globs": ["*.rs", "*.ts"]})).unwrap();
        assert_eq!(paths(&out, "paths"), ["src/a.rs", "src/nested/b.ts"]);
        // DSH's glob: --no-ignore --hidden with the VCS folders excluded, oldest first.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(dir.join("src/nested/b.ts"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let dsh = search(
            &host,
            json!({"mode": "files", "globs": ["**/*.{ts,rs,js}", "!**/.git", "!**/.git/**"],
                "hidden": true, "no_ignore": true, "sort": "modified"}),
        )
        .unwrap();
        let got = paths(&dsh, "paths");
        assert_eq!(got[0], "src/nested/b.ts");
        assert!(got.contains(&"build/out.js".to_string()));
        assert_eq!(got.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
