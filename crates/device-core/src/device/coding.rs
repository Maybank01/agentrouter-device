//! The coding actions (DEVICE-PROTOCOL.md §5.4): `edit_file`, `apply_patch`, `search`, `read_files`,
//! `list_dir`. Same gate as the other file tools: every path is checked, writes follow the access
//! level (asked at 「每条都确认」), nothing is written until the whole edit or patch worked out, and a
//! file that changed since the AI read it is a `CONFLICT`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use super::{
    Allowance, Device, READ_MAX, WRITE_MAX, base_hash, check_writable, conflict, emit_to,
    file_sha256, folder_of, turn_of, with_checkpoint, write_checked,
};
use crate::approvals::Run;
use crate::checkpoint::Change;
use crate::config::Access;
use crate::consent::Ask;
use crate::edit::{Edit, FileOp, apply_chunks, apply_edits, parse_patch};
use crate::gate::{Scope, display};
use crate::protocol::DeviceError;
use crate::search::{SearchArgs, list_dir, search};
use crate::util::clip;

const READ_FILES_MAX: usize = 50;
const READ_FILES_DEFAULT: u64 = 64 * 1024;
const READ_FILES_TOTAL: u64 = 2 << 20;
const EDITS_MAX: usize = 50;

/// A text file as it is now: its bytes (`None` when it does not exist) and their hash.
fn current(scope: &Scope, path: &Path) -> Result<Option<(Vec<u8>, String)>, DeviceError> {
    if !path.exists() {
        return Ok(None);
    }
    if path.is_dir() {
        return Err(DeviceError::new("FAILED", "这是一个文件夹，不是文件"));
    }
    let mut file =
        File::open(path).map_err(|e| DeviceError::new("FAILED", format!("could not open: {e}")))?;
    scope.check_opened(&file)?;
    if file.metadata().map(|m| m.len()).unwrap_or(0) > WRITE_MAX as u64 {
        return Err(
            DeviceError::new("TOO_LARGE", "文件超过 8 MiB").next("大文件用 exec 跑命令处理。")
        );
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| DeviceError::new("FAILED", format!("could not read: {e}")))?;
    let hash = crate::util::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    Ok(Some((bytes, hash)))
}

fn text_of(bytes: Vec<u8>, shown: &str) -> Result<String, DeviceError> {
    String::from_utf8(bytes).map_err(|_| {
        DeviceError::new("NOT_TEXT", format!("{shown} 不是 UTF-8 文本"))
            .next("用 read_file 的 base64 读，用 write_file 整个写。")
    })
}

fn ensure_parent(path: &Path) -> Result<(), DeviceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| DeviceError::new("FAILED", format!("could not create the folder: {e}")))?;
    }
    Ok(())
}

/// The file still has the hash the change was computed from (`None`: it did not exist).
fn unchanged(path: &Path, before: &Option<String>) -> Result<(), DeviceError> {
    let now = if path.exists() {
        Some(file_sha256(path).unwrap_or_default())
    } else {
        None
    };
    if &now == before {
        Ok(())
    } else {
        Err(conflict("文件在这次修改算好之后又被改了"))
    }
}

enum Planned {
    Write {
        path: PathBuf,
        text: String,
        before: Option<String>,
        op: &'static str,
        from: Option<PathBuf>,
    },
    Delete {
        path: PathBuf,
        before: Option<String>,
    },
}

impl Device {
    pub(super) fn edit_file(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        if scope.access == Access::Readonly {
            return Err(DeviceError::denied("这台设备设成了只读，不能写文件"));
        }
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
        check_writable(scope, &path)?;
        let edits: Vec<Edit> = args
            .get("edits")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|e| Edit {
                        old: e.get("old").and_then(Value::as_str).unwrap_or("").into(),
                        new: e.get("new").and_then(Value::as_str).unwrap_or("").into(),
                        all: e.get("all").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if edits.is_empty() || edits.len() > EDITS_MAX {
            return Err(DeviceError::new("FAILED", "edits 要有 1 到 50 处"));
        }
        let shown = display(&path);
        let base = base_hash(args);
        let existing = current(scope, &path)?;
        let (text, replacements, before) = match existing {
            None => {
                if base.as_deref().is_some_and(|b| !b.is_empty()) {
                    return Err(conflict("文件已经不在了"));
                }
                if edits.len() == 1 && edits[0].old.is_empty() {
                    (edits[0].new.clone(), 0, None)
                } else {
                    return Err(DeviceError::new("NOT_FOUND", "没有这个文件")
                        .next("新建文件用 write_file。"));
                }
            }
            Some((bytes, hash)) => {
                if let Some(b) = &base
                    && *b != hash
                {
                    return Err(if b.is_empty() {
                        conflict("这个文件已经存在")
                    } else {
                        conflict("文件在你上次读之后被改过了")
                    });
                }
                let original = text_of(bytes, &shown)?;
                let (text, n) = apply_edits(&original, &edits)?;
                (text, n, Some(hash))
            }
        };
        let preview = edits
            .iter()
            .map(|e| format!("- {}\n+ {}", clip(&e.old, 300), clip(&e.new, 300)))
            .collect::<Vec<_>>()
            .join("\n");
        let run: Run = {
            let (scope, console, session, path, shown) = (
                scope.clone(),
                self.console.clone(),
                session.to_string(),
                path.clone(),
                shown.clone(),
            );
            let (checkpoints, folder, turn) = (
                self.checkpoints.clone(),
                folder_of(&scope, &path),
                turn_of(args),
            );
            Box::new(move || {
                unchanged(&path, &before)?;
                let checkpoint = folder.and_then(|f| {
                    checkpoints.before(
                        &session,
                        turn.as_deref(),
                        &f,
                        Change::Files(std::slice::from_ref(&path)),
                    )
                });
                ensure_parent(&path)?;
                let size = write_checked(&scope, &path, text.as_bytes(), false)?;
                emit_to(
                    &console,
                    "write",
                    &session,
                    None,
                    &format!("{shown} (edit, {replacements} replacements)"),
                );
                Ok(with_checkpoint(
                    json!({"path": shown, "size": size, "sha256": file_sha256(&path), "replacements": replacements}),
                    checkpoint,
                ))
            })
        };
        if self.writes_allowed(scope, session) {
            return run();
        }
        let ask = Ask {
            session: session.to_string(),
            action: "edit_file",
            text: format!("{shown}\n{preview}"),
            cwd: None,
            kind: Some("本对话里的写文件".into()),
        };
        self.gated(
            session,
            "edit_file",
            args,
            ask,
            Some(Allowance::Writes),
            run,
            cancel,
        )
    }

    pub(super) fn apply_patch(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<Value, DeviceError> {
        if scope.access == Access::Readonly {
            return Err(DeviceError::denied("这台设备设成了只读，不能写文件"));
        }
        let patch = args.get("patch").and_then(Value::as_str).unwrap_or("");
        let ops = parse_patch(patch)?;
        let base_dir = match args
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            Some(raw) => {
                let dir = scope.check(raw)?;
                if !dir.is_dir() {
                    return Err(DeviceError::new("NOT_FOUND", "没有这个文件夹"));
                }
                dir
            }
            None => scope
                .default_cwd(&self.home)
                .ok_or_else(|| DeviceError::denied("这台设备没有允许的文件夹"))?,
        };
        let resolve = |p: &str| -> Result<PathBuf, DeviceError> {
            let absolute = Path::new(p).is_absolute() || (!cfg!(windows) && p.starts_with('/'));
            let full = if absolute {
                p.to_string()
            } else {
                let sep = if cfg!(windows) { '\\' } else { '/' };
                format!(
                    "{}{sep}{}",
                    display(&base_dir).trim_end_matches(['/', '\\']),
                    p
                )
            };
            let real = scope.check(&full)?;
            check_writable(scope, &real)?;
            Ok(real)
        };
        let mut plan: Vec<Planned> = Vec::new();
        let mut seen: Vec<PathBuf> = Vec::new();
        let mut claim = |path: &PathBuf, name: &str| -> Result<(), DeviceError> {
            if seen
                .iter()
                .any(|p| crate::gate::inside(p, path) && crate::gate::inside(path, p))
            {
                return Err(DeviceError::new(
                    "PATCH_INVALID",
                    format!("补丁里 {name} 出现了不止一次"),
                ));
            }
            seen.push(path.clone());
            Ok(())
        };
        for op in &ops {
            let path = resolve(op.path())?;
            claim(&path, op.path())?;
            let shown = display(&path);
            match op {
                FileOp::Add { content, .. } => {
                    if path.exists() {
                        return Err(conflict(&format!("{shown} 已经存在"))
                            .next("改已有的文件用 *** Update File:。"));
                    }
                    plan.push(Planned::Write {
                        path,
                        text: content.clone(),
                        before: None,
                        op: "add",
                        from: None,
                    });
                }
                FileOp::Delete { .. } => {
                    let Some((_, hash)) = current(scope, &path)? else {
                        return Err(DeviceError::new("NOT_FOUND", format!("没有 {shown}")));
                    };
                    plan.push(Planned::Delete {
                        path,
                        before: Some(hash),
                    });
                }
                FileOp::Update {
                    move_to, chunks, ..
                } => {
                    let Some((bytes, hash)) = current(scope, &path)? else {
                        return Err(DeviceError::new("NOT_FOUND", format!("没有 {shown}"))
                            .next("新文件用 *** Add File:。"));
                    };
                    let original = text_of(bytes, &shown)?;
                    let text = if chunks.is_empty() {
                        original
                    } else {
                        apply_chunks(&original, chunks, op.path())?
                    };
                    match move_to {
                        Some(to) => {
                            let dest = resolve(to)?;
                            claim(&dest, to)?;
                            if dest.exists() {
                                return Err(conflict(&format!("{} 已经存在", display(&dest))));
                            }
                            plan.push(Planned::Write {
                                path: dest,
                                text,
                                before: None,
                                op: "move",
                                from: Some(path.clone()),
                            });
                            plan.push(Planned::Delete {
                                path,
                                before: Some(hash),
                            });
                        }
                        None => plan.push(Planned::Write {
                            path,
                            text,
                            before: Some(hash),
                            op: "update",
                            from: None,
                        }),
                    }
                }
            }
        }
        let listing = plan
            .iter()
            .filter_map(|p| match p {
                Planned::Write { path, op, from, .. } => Some(match from {
                    Some(f) => format!("移动 {} → {}", display(f), display(path)),
                    None if *op == "add" => format!("新建 {}", display(path)),
                    None => format!("修改 {}", display(path)),
                }),
                Planned::Delete { path, .. } => {
                    let moved = plan
                        .iter()
                        .any(|q| matches!(q, Planned::Write { from: Some(f), .. } if f == path));
                    (!moved).then(|| format!("删除 {}", display(path)))
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let run: Run = {
            let (scope, console, session) =
                (scope.clone(), self.console.clone(), session.to_string());
            let (checkpoints, turn) = (self.checkpoints.clone(), turn_of(args));
            Box::new(move || {
                for step in &plan {
                    match step {
                        Planned::Write { path, before, .. } | Planned::Delete { path, before } => {
                            unchanged(path, before)?
                        }
                    }
                }
                // One checkpoint per folder the patch touches, covering its files.
                let mut checkpoint = None;
                let mut by_folder: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
                for step in &plan {
                    let (Planned::Write { path, .. } | Planned::Delete { path, .. }) = step;
                    if let Some(f) = folder_of(&scope, path) {
                        match by_folder.iter_mut().find(|(g, _)| *g == f) {
                            Some((_, files)) => files.push(path.clone()),
                            None => by_folder.push((f, vec![path.clone()])),
                        }
                    }
                }
                for (folder, files) in &by_folder {
                    let id =
                        checkpoints.before(&session, turn.as_deref(), folder, Change::Files(files));
                    checkpoint = checkpoint.or(id);
                }
                let mut files = Vec::new();
                for step in &plan {
                    match step {
                        Planned::Write {
                            path,
                            text,
                            op,
                            from,
                            ..
                        } => {
                            ensure_parent(path)?;
                            write_checked(&scope, path, text.as_bytes(), false)?;
                            let mut entry = json!({"path": display(path), "op": op, "sha256": file_sha256(path)});
                            if let Some(f) = from {
                                entry = json!({"path": display(f), "op": "move", "to": display(path), "sha256": file_sha256(path)});
                            }
                            emit_to(
                                &console,
                                "write",
                                &session,
                                None,
                                &format!("{} ({op})", display(path)),
                            );
                            files.push(entry);
                        }
                        Planned::Delete { path, .. } => {
                            std::fs::remove_file(path).map_err(|e| {
                                DeviceError::new("FAILED", format!("could not delete: {e}"))
                            })?;
                            let moved = files
                                .iter()
                                .any(|f| f["op"] == "move" && f["path"] == display(path).as_str());
                            if !moved {
                                emit_to(
                                    &console,
                                    "write",
                                    &session,
                                    None,
                                    &format!("{} (delete)", display(path)),
                                );
                                files.push(json!({"path": display(path), "op": "delete"}));
                            }
                        }
                    }
                }
                Ok(with_checkpoint(json!({"files": files}), checkpoint))
            })
        };
        if self.writes_allowed(scope, session) {
            return run();
        }
        let ask = Ask {
            session: session.to_string(),
            action: "apply_patch",
            text: listing,
            cwd: Some(display(&base_dir)),
            kind: Some("本对话里的写文件".into()),
        };
        self.gated(
            session,
            "apply_patch",
            args,
            ask,
            Some(Allowance::Writes),
            run,
            cancel,
        )
    }

    pub(super) fn search(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
    ) -> Result<Value, DeviceError> {
        let parsed = SearchArgs::from(args)?;
        let roots = match args
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        {
            Some(raw) => vec![scope.check(raw)?],
            None if scope.folders.is_empty() => {
                return Err(if scope.access == Access::Full {
                    DeviceError::new("PATH_INVALID", "完全访问时要写 path")
                } else {
                    DeviceError::denied("这台设备没有允许的文件夹")
                });
            }
            None => scope.folders.clone(),
        };
        if !roots[0].exists() {
            return Err(DeviceError::new("NOT_FOUND", "没有这个文件或文件夹"));
        }
        self.emit(
            "read",
            session,
            None,
            &format!("search {}", clip(&parsed.pattern, 200)),
        );
        search(&roots, scope, &parsed)
    }

    pub(super) fn read_files(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
    ) -> Result<Value, DeviceError> {
        let paths: Vec<&str> = args
            .get("paths")
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if paths.is_empty() || paths.len() > READ_FILES_MAX {
            return Err(DeviceError::new("FAILED", "paths 要有 1 到 50 个"));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(READ_FILES_DEFAULT)
            .clamp(1, READ_MAX);
        let mut budget = READ_FILES_TOTAL;
        let mut files = Vec::new();
        for raw in paths {
            let entry = if budget == 0 {
                Err(DeviceError::new(
                    "TOO_LARGE",
                    "这次一共读了 2 MiB，剩下的分开读",
                ))
            } else {
                read_one(scope, raw, limit.min(budget))
            };
            files.push(match entry {
                Ok((value, used)) => {
                    budget = budget.saturating_sub(used);
                    value
                }
                Err(e) => json!({"path": raw, "error": {"code": e.code, "message": e.message}}),
            });
        }
        self.emit("read", session, None, &format!("{} files", files.len()));
        Ok(json!({"files": files}))
    }

    pub(super) fn list_dir(
        &self,
        scope: &Scope,
        session: &str,
        args: &Value,
    ) -> Result<Value, DeviceError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
        let path = scope.check(raw)?;
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 5) as usize;
        let hidden = args.get("hidden").and_then(Value::as_bool).unwrap_or(false);
        let out = list_dir(&path, scope, depth, hidden)?;
        self.emit("read", session, None, &display(&path));
        Ok(out)
    }
}

/// One file of `read_files`, and the bytes it used of the budget.
fn read_one(scope: &Scope, raw: &str, limit: u64) -> Result<(Value, u64), DeviceError> {
    let path = scope.check(raw)?;
    if path.is_dir() {
        return Err(DeviceError::new("NOT_FOUND", "这是一个文件夹，不是文件"));
    }
    let mut file = File::open(&path).map_err(|_| DeviceError::new("NOT_FOUND", "没有这个文件"))?;
    scope.check_opened(&file)?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| DeviceError::new("FAILED", format!("could not read: {e}")))?;
    let shown = display(&path);
    let hash = file_sha256(&path);
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok((
            json!({"path": shown, "binary": true, "size": size, "sha256": hash}),
            0,
        ));
    }
    let used = bytes.len() as u64;
    Ok((
        json!({
            "path": shown, "size": size, "sha256": hash,
            "eof": used >= size,
            "content": String::from_utf8_lossy(&bytes),
        }),
        used,
    ))
}
