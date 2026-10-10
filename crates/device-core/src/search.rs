//! `search` and `list_dir` (DEVICE-PROTOCOL.md §5.4): ripgrep's own libraries, so the person does not
//! need `rg` installed. Like `rg`: `.gitignore` / `.ignore` respected, hidden and binary files skipped,
//! links not followed. Every root is checked by the gate first; the app's own data is never walked.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde_json::{Value, json};

use crate::gate::{Scope, display, inside};
use crate::protocol::DeviceError;

const LINE_MAX: usize = 500;
const RESULTS_DEFAULT: usize = 200;
const RESULTS_MAX: usize = 1000;
const LIST_MAX: usize = 2000;
const SEARCH_TIME: Duration = Duration::from_secs(20);
const FILE_MAX: u64 = 64 << 20;

pub struct SearchArgs {
    pub pattern: String,
    pub literal: bool,
    pub case_sensitive: Option<bool>,
    pub globs: Vec<String>,
    pub hidden: bool,
    pub files_only: bool,
    pub context: usize,
    pub max_results: usize,
}

impl SearchArgs {
    pub fn from(args: &Value) -> Result<SearchArgs, DeviceError> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if pattern.is_empty() {
            return Err(DeviceError::new("FAILED", "pattern required"));
        }
        let globs = match args.get("glob") {
            Some(Value::String(g)) => vec![g.clone()],
            Some(Value::Array(list)) => list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .take(20)
                .collect(),
            _ => Vec::new(),
        };
        Ok(SearchArgs {
            pattern,
            literal: args
                .get("literal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            case_sensitive: args.get("caseSensitive").and_then(Value::as_bool),
            globs,
            hidden: args.get("hidden").and_then(Value::as_bool).unwrap_or(false),
            files_only: args
                .get("filesOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            context: args
                .get("context")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(10) as usize,
            max_results: args
                .get("maxResults")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(RESULTS_DEFAULT)
                .clamp(1, RESULTS_MAX),
        })
    }
}

fn clip_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\r', '\n']);
    if text.chars().count() > LINE_MAX {
        let head: String = text.chars().take(LINE_MAX).collect();
        format!("{head}…")
    } else {
        text.to_string()
    }
}

struct Collect<'a> {
    path: String,
    out: &'a mut Vec<Value>,
    before: Vec<String>,
    max: usize,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let line = m.line_number().unwrap_or(0);
        let mut entry = json!({"path": self.path, "line": line, "text": clip_line(m.bytes())});
        if !self.before.is_empty() {
            entry["before"] = json!(std::mem::take(&mut self.before));
        }
        self.out.push(entry);
        Ok(self.out.len() < self.max)
    }

    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        let text = clip_line(c.bytes());
        match c.kind() {
            SinkContextKind::Before => self.before.push(text),
            SinkContextKind::After => {
                if let Some(last) = self.out.last_mut()
                    && last["path"] == self.path.as_str()
                {
                    match last.get_mut("after").and_then(Value::as_array_mut) {
                        Some(after) => after.push(json!(text)),
                        None => last["after"] = json!([text]),
                    }
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }
}

struct FirstMatch(bool);

impl Sink for FirstMatch {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.0 = true;
        Ok(false)
    }
}

fn walker(
    root: &Path,
    scope: &Scope,
    hidden: bool,
    globs: &[String],
) -> Result<WalkBuilder, DeviceError> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!hidden)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .follow_links(false);
    if !globs.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for g in globs {
            overrides
                .add(g)
                .map_err(|e| DeviceError::new("FAILED", format!("glob 写得不对（{g}）：{e}")))?;
        }
        builder.overrides(
            overrides
                .build()
                .map_err(|e| DeviceError::new("FAILED", format!("glob 写得不对：{e}")))?,
        );
    }
    let deny = scope.deny.clone();
    builder.filter_entry(move |entry| {
        entry.file_name() != ".git" && !deny.iter().any(|d| inside(entry.path(), d))
    });
    Ok(builder)
}

/// Search `roots` (real, gate-checked paths).
pub fn search(roots: &[PathBuf], scope: &Scope, args: &SearchArgs) -> Result<Value, DeviceError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .fixed_strings(args.literal)
        .line_terminator(Some(b'\n'));
    match args.case_sensitive {
        Some(true) => builder.case_insensitive(false),
        Some(false) => builder.case_insensitive(true),
        None => builder.case_smart(true),
    };
    let matcher = builder.build(&args.pattern).map_err(|e| {
        DeviceError::new("FAILED", format!("pattern 不是有效的正则：{e}"))
            .next("按字面找就设 literal: true。")
    })?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .before_context(args.context)
        .after_context(args.context)
        .build();
    let started = Instant::now();
    let mut matches: Vec<Value> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut truncated = false;
    'roots: for root in roots {
        for entry in walker(root, scope, args.hidden, &args.globs)?.build() {
            if started.elapsed() > SEARCH_TIME {
                truncated = true;
                break 'roots;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.metadata().map(|m| m.len() > FILE_MAX).unwrap_or(true) {
                continue;
            }
            let shown = display(entry.path());
            if args.files_only {
                let mut found = FirstMatch(false);
                let _ = searcher.search_path(&matcher, entry.path(), &mut found);
                if found.0 {
                    files.push(shown);
                    if files.len() >= args.max_results {
                        truncated = true;
                        break 'roots;
                    }
                }
                continue;
            }
            let mut sink = Collect {
                path: shown,
                out: &mut matches,
                before: Vec::new(),
                max: args.max_results,
            };
            let _ = searcher.search_path(&matcher, entry.path(), &mut sink);
            if matches.len() >= args.max_results {
                truncated = true;
                break 'roots;
            }
        }
    }
    Ok(if args.files_only {
        json!({"files": files, "truncated": truncated})
    } else {
        json!({"matches": matches, "truncated": truncated})
    })
}

fn kind_of(meta: &std::fs::Metadata) -> &'static str {
    if meta.file_type().is_symlink() {
        "link"
    } else if meta.is_dir() {
        "dir"
    } else {
        "file"
    }
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// List a folder (a real, gate-checked path).
pub fn list_dir(
    root: &Path,
    scope: &Scope,
    depth: usize,
    hidden: bool,
) -> Result<Value, DeviceError> {
    if !root.is_dir() {
        return Err(DeviceError::new("NOT_FOUND", "没有这个文件夹"));
    }
    let mut entries: Vec<(bool, String, Value)> = Vec::new();
    let mut truncated = false;
    if depth <= 1 {
        let read = std::fs::read_dir(root)
            .map_err(|e| DeviceError::new("FAILED", format!("could not list: {e}")))?;
        for entry in read.flatten() {
            let path = entry.path();
            if scope.deny.iter().any(|d| inside(&path, d)) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if entries.len() >= LIST_MAX {
                truncated = true;
                break;
            }
            entries.push(entry_value(&path, root, &meta));
        }
    } else {
        let mut builder = walker(root, scope, hidden, &[])?;
        builder.max_depth(Some(depth.min(5)));
        for entry in builder.build().flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if entries.len() >= LIST_MAX {
                truncated = true;
                break;
            }
            entries.push(entry_value(entry.path(), root, &meta));
        }
    }
    entries.sort_by(|a, b| {
        if depth <= 1 {
            b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))
        } else {
            a.1.cmp(&b.1)
        }
    });
    Ok(json!({
        "path": display(root),
        "entries": entries.into_iter().map(|e| e.2).collect::<Vec<_>>(),
        "truncated": truncated,
    }))
}

fn entry_value(path: &Path, root: &Path, meta: &std::fs::Metadata) -> (bool, String, Value) {
    let rel = relative(path, root);
    let kind = kind_of(meta);
    let mut value = json!({"path": rel, "type": kind});
    if kind == "file" {
        value["size"] = json!(meta.len());
    }
    (kind == "dir", rel.to_lowercase(), value)
}
