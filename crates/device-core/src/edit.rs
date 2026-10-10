//! Text edits computed in memory (DEVICE-PROTOCOL.md §5.4): `edit_file`'s string replacements and
//! `apply_patch`'s Codex patch format. Nothing here touches the disk; the device checks every path
//! and writes only after the whole edit or patch worked out.

use crate::protocol::DeviceError;

/// One `edit_file` replacement.
#[derive(Debug, Clone)]
pub struct Edit {
    pub old: String,
    pub new: String,
    pub all: bool,
}

const REREAD: &str = "重新读一遍这个文件，按现在的内容再改。";

fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Apply the edits in order, each on the result of the one before. Returns the new text and how many
/// replacements were made. A file with CRLF line ends also matches `old` written with LF, and keeps CRLF.
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<(String, usize), DeviceError> {
    let windows_ends = content.contains("\r\n");
    let mut text = content.to_string();
    let mut total = 0;
    for (i, edit) in edits.iter().enumerate() {
        let n = i + 1;
        if edit.old.is_empty() {
            return Err(
                DeviceError::new("EDIT_NOT_FOUND", format!("第 {n} 处的 old 是空的"))
                    .next("old 要写文件里现有的一段文字；新建文件用 write_file。"),
            );
        }
        if edit.old == edit.new {
            return Err(DeviceError::new(
                "FAILED",
                format!("第 {n} 处的 old 和 new 一样"),
            ));
        }
        let (mut old, mut new) = (edit.old.clone(), edit.new.clone());
        let mut count = text.matches(old.as_str()).count();
        if count == 0 && windows_ends && old.contains('\n') {
            old = crlf(&old);
            new = crlf(&new);
            count = text.matches(old.as_str()).count();
        }
        if count == 0 {
            return Err(DeviceError::new(
                "EDIT_NOT_FOUND",
                format!("第 {n} 处要替换的文字在文件里没找到"),
            )
            .next(REREAD));
        }
        if count > 1 && !edit.all {
            return Err(DeviceError::new(
                "EDIT_AMBIGUOUS",
                format!("第 {n} 处要替换的文字在文件里出现了 {count} 次"),
            )
            .next("多带几行上下文让它只出现一次，或者设 all: true 全部替换。"));
        }
        text = if edit.all {
            text.replace(old.as_str(), &new)
        } else {
            text.replacen(old.as_str(), &new, 1)
        };
        total += if edit.all { count } else { 1 };
    }
    Ok((text, total))
}

// ---- apply_patch (Codex format) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The `@@ …` header: a line to find first (the hunk follows it).
    pub context: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

impl FileOp {
    pub fn path(&self) -> &str {
        match self {
            FileOp::Add { path, .. } | FileOp::Delete { path } | FileOp::Update { path, .. } => {
                path
            }
        }
    }
}

fn invalid(line: usize, why: &str) -> DeviceError {
    DeviceError::new("PATCH_INVALID", format!("补丁第 {line} 行：{why}")).next(
        "按 Codex 的补丁格式重写：*** Begin Patch，*** Add File: / *** Update File: / *** Delete File:，@@ 开头的块，*** End Patch。",
    )
}

/// Parse a patch. Lenient like Codex: markers may carry surrounding spaces, a heredoc wrapper is
/// dropped, the first hunk of a file may omit `@@`, and an empty line inside a hunk is an empty context line.
pub fn parse_patch(patch: &str) -> Result<Vec<FileOp>, DeviceError> {
    let mut lines: Vec<&str> = patch.lines().collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let mut start = 0;
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }
    // `apply_patch <<'EOF'` … `EOF`
    if start < lines.len() && lines[start].trim_start().starts_with("apply_patch") {
        start += 1;
        if lines.last().is_some_and(|l| {
            let t = l.trim();
            !t.is_empty()
                && t.chars()
                    .all(|c| c.is_ascii_uppercase() || c == '\'' || c == '"')
        }) {
            lines.pop();
        }
    }
    if start >= lines.len() || lines[start].trim() != "*** Begin Patch" {
        return Err(invalid(start + 1, "要以 *** Begin Patch 开头"));
    }
    if lines.len() < start + 2 || lines[lines.len() - 1].trim() != "*** End Patch" {
        return Err(invalid(lines.len(), "要以 *** End Patch 结尾"));
    }
    let body = &lines[start + 1..lines.len() - 1];
    let base = start + 2; // 1-based line number of body[0]
    let mut ops = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let line = body[i].trim();
        if line.is_empty() {
            i += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File:") {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(invalid(base + i, "Add File 后面要写路径"));
            }
            i += 1;
            let mut content = String::new();
            while i < body.len() && !body[i].trim_start().starts_with("*** ") {
                match body[i].strip_prefix('+') {
                    Some(text) => {
                        content.push_str(text);
                        content.push('\n');
                    }
                    None if body[i].trim().is_empty() => content.push('\n'),
                    None => return Err(invalid(base + i, "Add File 里每一行都要以 + 开头")),
                }
                i += 1;
            }
            ops.push(FileOp::Add { path, content });
        } else if let Some(path) = line.strip_prefix("*** Delete File:") {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(invalid(base + i, "Delete File 后面要写路径"));
            }
            ops.push(FileOp::Delete { path });
            i += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File:") {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(invalid(base + i, "Update File 后面要写路径"));
            }
            i += 1;
            let mut move_to = None;
            if i < body.len()
                && let Some(to) = body[i].trim().strip_prefix("*** Move to:")
            {
                move_to = Some(to.trim().to_string());
                i += 1;
            }
            let mut chunks = Vec::new();
            let mut first = true;
            while i < body.len() {
                let t = body[i].trim();
                if t.starts_with("*** ") && t != "*** End of File" {
                    break;
                }
                let header = if t == "@@" {
                    i += 1;
                    None
                } else if let Some(h) = body[i].trim_start().strip_prefix("@@ ") {
                    i += 1;
                    Some(h.trim().to_string()).filter(|h| !h.is_empty())
                } else if first {
                    None
                } else if t.is_empty() {
                    i += 1;
                    continue;
                } else {
                    return Err(invalid(base + i, "每一块要以 @@ 开头"));
                };
                first = false;
                let mut chunk = Chunk {
                    context: header,
                    old: Vec::new(),
                    new: Vec::new(),
                    end_of_file: false,
                };
                let mut changed = 0;
                while i < body.len() {
                    let raw = body[i];
                    let t = raw.trim();
                    if t == "*** End of File" {
                        chunk.end_of_file = true;
                        i += 1;
                        break;
                    }
                    if t.starts_with("*** ") || raw.trim_start().starts_with("@@") {
                        break;
                    }
                    if raw.is_empty() {
                        chunk.old.push(String::new());
                        chunk.new.push(String::new());
                    } else if let Some(text) = raw.strip_prefix(' ') {
                        chunk.old.push(text.to_string());
                        chunk.new.push(text.to_string());
                    } else if let Some(text) = raw.strip_prefix('-') {
                        chunk.old.push(text.to_string());
                        changed += 1;
                    } else if let Some(text) = raw.strip_prefix('+') {
                        chunk.new.push(text.to_string());
                        changed += 1;
                    } else {
                        return Err(invalid(base + i, "块里的行要以空格、- 或 + 开头"));
                    }
                    i += 1;
                }
                if changed == 0 && chunk.old.is_empty() {
                    return Err(invalid(base + i, "空的块"));
                }
                chunks.push(chunk);
            }
            if chunks.is_empty() && move_to.is_none() {
                return Err(invalid(base + i, "Update File 里没有改动"));
            }
            ops.push(FileOp::Update {
                path,
                move_to,
                chunks,
            });
        } else {
            return Err(invalid(
                base + i,
                "这里要是 *** Add File: / *** Update File: / *** Delete File:",
            ));
        }
    }
    if ops.is_empty() {
        return Err(invalid(base, "补丁里没有文件"));
    }
    Ok(ops)
}

/// Where `pattern` starts in `lines` at or after `start`: exact, then ignoring trailing spaces, then
/// ignoring surrounding spaces (Codex's order). An end-of-file chunk is tried at the end first.
fn seek(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start.min(lines.len()));
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last = lines.len() - pattern.len();
    let passes: [fn(&str) -> &str; 3] = [|s| s, |s| s.trim_end(), |s| s.trim()];
    let mut starts = Vec::new();
    if eof {
        starts.push(last);
    }
    starts.push(start);
    for from in starts {
        for norm in passes {
            for i in from..=last {
                if lines[i..i + pattern.len()]
                    .iter()
                    .zip(pattern)
                    .all(|(a, b)| norm(a) == norm(b))
                {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// Apply an `Update File`'s chunks to the file's text. `which` names the file in errors.
pub fn apply_chunks(content: &str, chunks: &[Chunk], which: &str) -> Result<String, DeviceError> {
    let windows_ends = content.contains("\r\n");
    let normalized = content.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    // A final newline leaves an empty last piece; it is not a line.
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let not_applied = |n: usize, why: &str| {
        DeviceError::new("PATCH_NOT_APPLIED", format!("{which} 的第 {n} 块{why}"))
            .next(REREAD.replace("再改", "再出补丁"))
    };
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut at = 0usize;
    for (k, chunk) in chunks.iter().enumerate() {
        let n = k + 1;
        if let Some(ctx) = &chunk.context {
            match seek(&lines, std::slice::from_ref(ctx), at, false) {
                Some(i) => at = i + 1,
                None => return Err(not_applied(n, &format!("的 @@ 行「{ctx}」没找到"))),
            }
        }
        if chunk.old.is_empty() {
            // Pure addition: at the end of the file.
            replacements.push((lines.len(), 0, chunk.new.clone()));
            continue;
        }
        let mut old = chunk.old.clone();
        let mut new = chunk.new.clone();
        let mut found = seek(&lines, &old, at, chunk.end_of_file);
        if found.is_none() && old.last().is_some_and(String::is_empty) {
            old.pop();
            if new.last().is_some_and(String::is_empty) {
                new.pop();
            }
            found = seek(&lines, &old, at, chunk.end_of_file);
        }
        let Some(i) = found else {
            return Err(not_applied(n, "的上下文和文件现在的内容对不上"));
        };
        replacements.push((i, old.len(), new));
        at = i + old.len();
    }
    replacements.sort_by_key(|r| r.0);
    for w in replacements.windows(2) {
        if w[0].0 + w[0].1 > w[1].0 {
            return Err(not_applied(1, "和别的块重叠"));
        }
    }
    for (i, len, new) in replacements.into_iter().rev() {
        lines.splice(i..i + len, new);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(if windows_ends { crlf(&out) } else { out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_replace_once_or_all() {
        let e = |old: &str, new: &str, all| Edit {
            old: old.into(),
            new: new.into(),
            all,
        };
        assert_eq!(
            apply_edits("a b a", &[e("b", "c", false)]).unwrap(),
            ("a c a".into(), 1)
        );
        assert_eq!(
            apply_edits("a b a", &[e("a", "x", true)]).unwrap(),
            ("x b x".into(), 2)
        );
        assert_eq!(
            apply_edits("a b a", &[e("a", "x", false)])
                .unwrap_err()
                .code,
            "EDIT_AMBIGUOUS"
        );
        assert_eq!(
            apply_edits("a b a", &[e("z", "x", false)])
                .unwrap_err()
                .code,
            "EDIT_NOT_FOUND"
        );
        assert_eq!(
            apply_edits("one\r\ntwo\r\n", &[e("one\ntwo", "1\n2", false)]).unwrap(),
            ("1\r\n2\r\n".into(), 1)
        );
    }

    #[test]
    fn patch_parses_and_applies() {
        let patch = "*** Begin Patch\n*** Update File: src/a.txt\n@@ fn main\n a\n-b\n+B\n c\n*** Add File: new.txt\n+hello\n*** Delete File: old.txt\n*** End Patch\n";
        let ops = parse_patch(patch).unwrap();
        assert_eq!(ops.len(), 3);
        let FileOp::Update { chunks, .. } = &ops[0] else {
            panic!()
        };
        assert_eq!(
            apply_chunks("x\nfn main\na\nb\nc\n", chunks, "src/a.txt").unwrap(),
            "x\nfn main\na\nB\nc\n"
        );
        assert_eq!(
            apply_chunks("x\r\nfn main\r\na\r\nb\r\nc\r\n", chunks, "a").unwrap(),
            "x\r\nfn main\r\na\r\nB\r\nc\r\n"
        );
        assert_eq!(
            apply_chunks("q\nr\n", chunks, "a").unwrap_err().code,
            "PATCH_NOT_APPLIED"
        );
        assert_eq!(
            ops[1],
            FileOp::Add {
                path: "new.txt".into(),
                content: "hello\n".into()
            }
        );
        assert_eq!(
            parse_patch("*** Update File: x\n").unwrap_err().code,
            "PATCH_INVALID"
        );
    }
}
