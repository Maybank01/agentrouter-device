//! Deploying built artifacts onto a registered external target (S2a): the Mod's files land in the
//! game install only through this path — a whitelisted target, a local approval, a backup of every
//! file that gets overwritten, a durable manifest, and automatic rollback on failure.
//!
//! Nothing here writes to an arbitrary directory: `deploy` only accepts a target registered by the
//! owner on this computer (`deploy-targets.json`), artifacts inside the workspace, and a relative
//! destination that cannot escape the target (no absolute path, no `..`, real-path checked against
//! junctions too). A game install is only ever a deploy target, never a workspace or a project.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::gate::{self, display, inside};
use crate::protocol::DeviceError;
use crate::util::{iso_now, now_ms, short_id};
use crate::workspace;

pub const TARGETS_FILE: &str = "deploy-targets.json";
pub const DEPLOYMENTS_DIR: &str = "deployments";
pub const SCHEMA: u32 = 1;
/// Keep this many deployments; older ones are pruned after a successful deploy.
pub const KEEP: usize = 20;
pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: u64 = 256 << 20;
pub const MAX_TOTAL_BYTES: u64 = 512 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub name: String,
    pub path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TargetList {
    schema_version: u32,
    #[serde(default)]
    targets: Vec<Target>,
}

fn targets_path(data: &Path) -> PathBuf {
    data.join(TARGETS_FILE)
}

fn load_targets(data: &Path) -> Result<TargetList, DeviceError> {
    let path = targets_path(data);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(TargetList {
            schema_version: SCHEMA,
            targets: Vec::new(),
        });
    };
    let list: TargetList = serde_json::from_str(&text).map_err(|e| {
        DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            format!("部署目标登记读不出来（{}）：{e}", display(&path)),
        )
    })?;
    if list.schema_version != SCHEMA {
        return Err(DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            format!("部署目标登记版本不认识（{}）", list.schema_version),
        ));
    }
    Ok(list)
}

fn save_targets(data: &Path, list: &TargetList) -> Result<(), DeviceError> {
    std::fs::create_dir_all(data).map_err(|e| {
        DeviceError::new(
            "DEPLOY_TARGET_FAILED",
            format!("建不了设备数据目录 {}：{e}", display(data)),
        )
    })?;
    let path = targets_path(data);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(list)
        .map_err(|e| DeviceError::new("DEPLOY_TARGET_FAILED", format!("写不出来：{e}")))?;
    std::fs::write(&tmp, bytes)
        .map_err(|e| DeviceError::new("DEPLOY_TARGET_FAILED", format!("写入失败：{e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| DeviceError::new("DEPLOY_TARGET_FAILED", format!("换名失败：{e}")))
}

fn target_name(raw: &str) -> Result<String, DeviceError> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() || name.len() > 32 {
        return Err(DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            "部署目标名要 1–32 个字符",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            "部署目标名只能用英文字母、数字和 -",
        ));
    }
    Ok(name)
}

fn same(a: &Path, b: &Path) -> bool {
    let a = PathBuf::from(display(a));
    let b = PathBuf::from(display(b));
    inside(&a, &b) && inside(&b, &a)
}

/// Add (once) an external deploy target. Only the owner runs this on the machine (CLI with local
/// consent); an AI cannot register a target.
pub fn add_target(data: &Path, raw_name: &str, raw_path: &str) -> Result<Target, DeviceError> {
    let name = target_name(raw_name)?;
    let shown = display(Path::new(raw_path.trim()));
    gate::check_syntax(&shown)?;
    let real = workspace::real(Path::new(&shown))?;
    if !real.is_dir() {
        return Err(DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            format!("部署目标不是目录：{}", display(&real)),
        ));
    }
    if inside(&real, data) || inside(data, &real) {
        return Err(DeviceError::denied("部署目标不能和设备自己的数据目录重叠"));
    }
    let registry = workspace::load(data)?;
    if !registry.root.trim().is_empty() {
        let root = workspace::real(Path::new(&registry.root))?;
        if inside(&real, &root) || inside(&root, &real) {
            return Err(DeviceError::denied(
                "部署目标不能和工作区根重叠（工作区里的东西不是外部部署目标）",
            ));
        }
    }
    let mut list = load_targets(data)?;
    if let Some(existing) = list.targets.iter().find(|t| t.name == name) {
        if same(Path::new(&existing.path), &real) {
            return Ok(existing.clone());
        }
        return Err(DeviceError::new(
            "DEPLOY_TARGET_INVALID",
            format!("部署目标 {name} 已经指向 {}", existing.path),
        )
        .next("先删掉旧的再登记，或换一个名字。"));
    }
    let target = Target {
        name,
        path: display(&real),
        created_at: iso_now(),
    };
    list.targets.push(target.clone());
    save_targets(data, &list)?;
    Ok(target)
}

pub fn remove_target(data: &Path, raw_name: &str) -> Result<bool, DeviceError> {
    let name = target_name(raw_name)?;
    let mut list = load_targets(data)?;
    let before = list.targets.len();
    list.targets.retain(|t| t.name != name);
    let removed = list.targets.len() != before;
    if removed {
        save_targets(data, &list)?;
    }
    Ok(removed)
}

pub fn list_targets(data: &Path) -> Result<Vec<Target>, DeviceError> {
    Ok(load_targets(data)?.targets)
}

fn find_target(data: &Path, raw: &str) -> Result<Target, DeviceError> {
    let name = raw.trim();
    let list = load_targets(data)?;
    list.targets
        .iter()
        .find(|t| t.name == name || t.path == name)
        .cloned()
        .ok_or_else(|| {
            DeviceError::new(
                "DEPLOY_TARGET_UNKNOWN",
                format!("没有登记过的部署目标：{name}"),
            )
            .next("请用户在这台电脑上运行 agentrouter workspace deploy target add <名字> <目录>。")
        })
}

fn sha256_file(path: &Path) -> Result<String, DeviceError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        DeviceError::new(
            "DEPLOY_ARTIFACT_INVALID",
            format!("打不开 {}：{e}", display(path)),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 << 10];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|e| DeviceError::new("DEPLOY_ARTIFACT_INVALID", format!("读不了文件：{e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::util::hex(&hasher.finalize()))
}

fn file_size(path: &Path) -> Result<u64, DeviceError> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| DeviceError::new("DEPLOY_ARTIFACT_INVALID", format!("看不到文件：{e}")))
}

/// The artifact the caller asked to copy: a source path inside the workspace and a relative
/// destination under the target.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    source: PathBuf,
    dest: String,
}

fn asked_artifacts(args: &Value) -> Result<Vec<Asked>, DeviceError> {
    let list = args
        .as_array()
        .ok_or_else(|| DeviceError::new("DEPLOY_ARTIFACT_INVALID", "artifacts 要是数组"))?;
    if list.is_empty() || list.len() > MAX_FILES {
        return Err(DeviceError::new(
            "DEPLOY_ARTIFACT_INVALID",
            format!("artifacts 要 1–{MAX_FILES} 个文件"),
        ));
    }
    let mut out = Vec::new();
    for item in list {
        let (source, dest) = match item {
            Value::String(source) => {
                let name = Path::new(source)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (source.clone(), name)
            }
            Value::Object(map) => {
                let source = map.get("source").and_then(Value::as_str).unwrap_or("");
                let dest = map.get("dest").and_then(Value::as_str).unwrap_or("");
                (source.to_string(), dest.to_string())
            }
            _ => {
                return Err(DeviceError::new(
                    "DEPLOY_ARTIFACT_INVALID",
                    "artifacts 每项要是 {source, dest} 或文件路径",
                ));
            }
        };
        if source.trim().is_empty() || dest.trim().is_empty() {
            return Err(DeviceError::new(
                "DEPLOY_ARTIFACT_INVALID",
                "artifact 的 source 和 dest 都不能为空",
            ));
        }
        out.push(Asked {
            source: PathBuf::from(source.trim()),
            dest: dest.trim().to_string(),
        });
    }
    Ok(out)
}

/// A relative destination under the target: forward slashes, no root, no `..`, no empty segment.
fn checked_dest(raw: &str) -> Result<String, DeviceError> {
    let text = raw.trim().replace('\\', "/");
    if text.starts_with('/') || text.len() >= 2 && text.as_bytes()[1] == b':' {
        return Err(DeviceError::new(
            "DEPLOY_DEST_INVALID",
            format!("dest 必须是目标下的相对路径：{raw}"),
        ));
    }
    let mut parts = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                return Err(DeviceError::new(
                    "DEPLOY_DEST_INVALID",
                    format!("dest 不能含 ..：{raw}"),
                ));
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Err(DeviceError::new("DEPLOY_DEST_INVALID", "dest 不能为空"));
    }
    Ok(parts.join("/"))
}

fn allowed_roots(data: &Path) -> Result<Vec<PathBuf>, DeviceError> {
    let registry = workspace::load(data)?;
    let mut roots = Vec::new();
    for project in &registry.projects {
        if let Ok(real) = workspace::real(Path::new(&project.path)) {
            roots.push(real);
        }
    }
    for worktree in crate::worktree::load_ledger(data)?.worktrees {
        if let Ok(real) = workspace::real(Path::new(&worktree.path)) {
            roots.push(real);
        }
    }
    Ok(roots)
}

/// The evaluated deploy: every source resolved and hashed, every destination safe under the target.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub source: String,
    pub source_sha256: String,
    pub source_size: u64,
    pub dest: String,
    pub target_path: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub id: String,
    pub at: String,
    pub session: String,
    pub project: String,
    pub message: String,
    pub target: Target,
    pub files: Vec<PlannedFile>,
}

/// Validate a deploy request and return what would happen. Reads only; the caller asks the person,
/// then calls [`apply`].
pub fn plan(
    data: &Path,
    session: &str,
    project_raw: &str,
    artifacts: &Value,
    target_raw: &str,
    message: &str,
) -> Result<Plan, DeviceError> {
    let registry = workspace::load(data)?;
    let project = crate::worktree::project_of(&registry, project_raw)?;
    let target = find_target(data, target_raw)?;
    let target_real = workspace::real(Path::new(&target.path))?;
    let target = Target {
        path: display(&target_real),
        ..target
    };
    let roots = allowed_roots(data)?;
    if roots.is_empty() {
        return Err(DeviceError::new(
            "DEPLOY_ARTIFACT_INVALID",
            "工作区里还没有项目",
        ));
    }
    let project_dir = workspace::real(Path::new(&project.path))?;
    let mut files: Vec<PlannedFile> = Vec::new();
    let mut total: u64 = 0;
    for asked in asked_artifacts(artifacts)? {
        let resolved = if asked.source.is_absolute() {
            asked.source.clone()
        } else {
            project_dir.join(&asked.source)
        };
        // Paths people (and JSON) write with `/` on Windows still have to become real Windows paths.
        let resolved =
            PathBuf::from(display(&resolved).replace('/', std::path::MAIN_SEPARATOR_STR));
        let source = workspace::real(&resolved).map_err(|_| {
            DeviceError::new(
                "DEPLOY_ARTIFACT_INVALID",
                format!("找不到 artifact：{}", display(&resolved)),
            )
        })?;
        if !source.is_file() {
            return Err(DeviceError::new(
                "DEPLOY_ARTIFACT_INVALID",
                format!("artifact 不是文件：{}", display(&source)),
            ));
        }
        let in_workspace = roots.iter().any(|root| inside(&source, root));
        if !in_workspace {
            return Err(DeviceError::denied(format!(
                "artifact 必须在工作区的项目/worktree 里：{}",
                display(&source)
            ))
            .next("先 worktree_create/project 里构建，再 deploy。"));
        }
        let dest = checked_dest(&asked.dest)?;
        let target_path = target_real.join(dest.replace('/', std::path::MAIN_SEPARATOR_STR));
        // The destination may not leave the target, also through a junction inside the target.
        if let Ok(real) = gate::real_location(&target_path)
            && !inside(&real, &target_real)
        {
            return Err(DeviceError::denied(format!(
                "dest 指向了部署目标外面（可能是链接）：{}",
                dest
            )));
        }
        let size = file_size(&source)?;
        if size > MAX_FILE_BYTES {
            return Err(DeviceError::new(
                "DEPLOY_ARTIFACT_INVALID",
                format!("文件太大（{} 字节）", size),
            ));
        }
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err(DeviceError::new(
                "DEPLOY_ARTIFACT_INVALID",
                "这次部署的文件总量太大",
            ));
        }
        let sha = sha256_file(&source)?;
        files.push(PlannedFile {
            source: display(&source),
            source_sha256: sha,
            source_size: size,
            dest,
            target_path: display(&target_path),
        });
    }
    Ok(Plan {
        id: format!("dep_{}_{}", now_ms(), short_id("")),
        at: iso_now(),
        session: session.to_string(),
        project: project.slug,
        message: message.trim().to_string(),
        target,
        files,
    })
}

/// What the person is asked before anything on disk changes.
pub fn approval_text(plan: &Plan) -> String {
    let mut text = format!(
        "把 {} 个文件部署到《{}》（{}）？\n\n来源：项目 {}（会话 {}）\n{}",
        plan.files.len(),
        plan.target.name,
        plan.target.path,
        plan.project,
        if plan.session.is_empty() {
            "本机"
        } else {
            &plan.session
        },
        if plan.message.is_empty() {
            String::new()
        } else {
            format!("说明：{}\n", plan.message)
        }
    );
    for file in &plan.files {
        text.push_str(&format!(
            "\n  {} → {}（{} 字节）",
            file.source, file.dest, file.source_size
        ));
    }
    text.push_str("\n\n会先备份被覆盖的文件；失败会自动回滚。");
    text
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetRef {
    name: String,
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupRef {
    file: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    source: String,
    source_sha256: String,
    source_size: u64,
    dest: String,
    target_path: String,
    /// `None` when the target file did not exist before this deploy (rollback deletes it).
    backup: Option<BackupRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub at: String,
    pub session: String,
    pub project: String,
    #[serde(default)]
    pub message: String,
    target: TargetRef,
    files: Vec<ManifestFile>,
    pub status: String,
    #[serde(default)]
    pub rolled_back_at: Option<String>,
    #[serde(default)]
    pub failure: Option<String>,
}

impl Manifest {
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id, "at": self.at, "target": self.target.name,
            "targetPath": self.target.path, "project": self.project, "session": self.session,
            "files": self.files.len(), "status": self.status, "message": self.message,
            "rolledBackAt": self.rolled_back_at,
        })
    }
}

fn deployments_dir(data: &Path) -> PathBuf {
    data.join(DEPLOYMENTS_DIR)
}

pub fn manifest_path(data: &Path, id: &str) -> PathBuf {
    deployments_dir(data)
        .join(checked_id(id).unwrap_or_default())
        .join("manifest.json")
}

fn checked_id(id: &str) -> Result<String, DeviceError> {
    let id = id.trim();
    if id.len() < 8
        || id.len() > 80
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(DeviceError::new(
            "DEPLOY_NOT_FOUND",
            "部署 id 不像我们的记录",
        ));
    }
    Ok(id.to_string())
}

fn load_manifest(data: &Path, id: &str) -> Result<Manifest, DeviceError> {
    let id = checked_id(id)?;
    let path = deployments_dir(data).join(&id).join("manifest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| DeviceError::new("DEPLOY_NOT_FOUND", format!("没有这次部署记录：{id}")))?;
    serde_json::from_str(&text).map_err(|e| {
        DeviceError::new(
            "DEPLOY_NOT_FOUND",
            format!("部署记录读不出来（{}）：{e}", display(&path)),
        )
    })
}

fn write_manifest(data: &Path, manifest: &Manifest) -> Result<(), DeviceError> {
    let dir = deployments_dir(data).join(&manifest.id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| DeviceError::new("DEPLOY_FAILED", format!("建不了部署记录目录：{e}")))?;
    let path = dir.join("manifest.json");
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|e| DeviceError::new("DEPLOY_FAILED", format!("记录写不出来：{e}")))?;
    std::fs::write(&tmp, bytes)
        .map_err(|e| DeviceError::new("DEPLOY_FAILED", format!("记录写入失败：{e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| DeviceError::new("DEPLOY_FAILED", format!("记录换名失败：{e}")))
}

fn copy_file(from: &Path, to: &Path) -> Result<(), DeviceError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            DeviceError::new(
                "DEPLOY_FAILED",
                format!("建不了目录 {}：{e}", display(parent)),
            )
        })?;
    }
    std::fs::copy(from, to).map_err(|e| {
        DeviceError::new(
            "DEPLOY_FAILED",
            format!("复制 {} → {} 失败：{e}", display(from), display(to)),
        )
    })?;
    Ok(())
}

fn deployment_dir(data: &Path, id: &str) -> PathBuf {
    deployments_dir(data).join(id)
}

fn rollback_manifest(data: &Path, manifest: &Manifest) -> Result<(), DeviceError> {
    let dir = deployment_dir(data, &manifest.id);
    let mut first_error: Option<DeviceError> = None;
    for file in &manifest.files {
        let target = Path::new(&file.target_path);
        let result = match &file.backup {
            Some(backup) => {
                let from = dir.join(&backup.file);
                copy_file(&from, target).and_then(|_| {
                    let sha = sha256_file(target)?;
                    if sha != backup.sha256 {
                        return Err(DeviceError::new(
                            "DEPLOY_ROLLBACK_FAILED",
                            format!("回滚后的文件 hash 不对：{}", file.dest),
                        ));
                    }
                    Ok(())
                })
            }
            None => {
                if target.exists() {
                    std::fs::remove_file(target).map_err(|e| {
                        DeviceError::new(
                            "DEPLOY_ROLLBACK_FAILED",
                            format!("删不掉部署的文件 {}：{e}", display(target)),
                        )
                    })
                } else {
                    Ok(())
                }
            }
        };
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Apply a planned deploy: back up what will be overwritten, write the files, verify, record. On a
/// failure it restores the backups and marks the deployment failed before returning the error.
pub fn apply(data: &Path, plan: &Plan) -> Result<Value, DeviceError> {
    apply_inner(data, plan, None)
}

fn apply_inner(data: &Path, plan: &Plan, fail_after: Option<usize>) -> Result<Value, DeviceError> {
    let dir = deployment_dir(data, &plan.id);
    std::fs::create_dir_all(dir.join("backup"))
        .map_err(|e| DeviceError::new("DEPLOY_FAILED", format!("建不了部署记录目录：{e}")))?;
    let mut files: Vec<ManifestFile> = Vec::new();
    // Back up first: nothing in the target changes before every backup succeeded.
    for file in &plan.files {
        let target = Path::new(&file.target_path);
        let backup = if target.exists() {
            if !target.is_file() {
                return Err(DeviceError::new(
                    "DEPLOY_DEST_INVALID",
                    format!("目标位置不是文件：{}", file.dest),
                ));
            }
            let rel = format!("backup/{}", file.dest);
            let backup_path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            let before = sha256_file(target)?;
            copy_file(target, &backup_path)?;
            let size = file_size(target)?;
            let after = sha256_file(&backup_path)?;
            if before != after {
                return Err(DeviceError::new(
                    "DEPLOY_BACKUP_FAILED",
                    format!("备份校验失败：{}", file.dest),
                ));
            }
            Some(BackupRef {
                file: rel,
                sha256: before,
                size,
            })
        } else {
            None
        };
        files.push(ManifestFile {
            source: file.source.clone(),
            source_sha256: file.source_sha256.clone(),
            source_size: file.source_size,
            dest: file.dest.clone(),
            target_path: file.target_path.clone(),
            backup,
        });
    }
    let mut manifest = Manifest {
        schema_version: SCHEMA,
        id: plan.id.clone(),
        at: plan.at.clone(),
        session: plan.session.clone(),
        project: plan.project.clone(),
        message: plan.message.clone(),
        target: TargetRef {
            name: plan.target.name.clone(),
            path: plan.target.path.clone(),
        },
        files: files.clone(),
        status: "applied".into(),
        rolled_back_at: None,
        failure: None,
    };
    // Write the manifest before touching the target: a crash still leaves a record to roll back.
    write_manifest(data, &manifest)?;
    for (written, file) in files.iter().enumerate() {
        if fail_after.is_some_and(|limit| written >= limit) {
            manifest.status = "failed".into();
            manifest.failure = Some("注入的失败（测试）".into());
            let _ = rollback_manifest(data, &manifest);
            let _ = write_manifest(data, &manifest);
            return Err(DeviceError::new(
                "DEPLOY_FAILED",
                "部署失败，已回滚（测试注入）",
            ));
        }
        let target = Path::new(&file.target_path);
        let source = Path::new(&file.source);
        if let Err(e) = copy_file(source, target) {
            manifest.status = "failed".into();
            manifest.failure = Some(e.message.clone());
            let _ = rollback_manifest(data, &manifest);
            let _ = write_manifest(data, &manifest);
            return Err(DeviceError::new(
                "DEPLOY_FAILED",
                format!("部署失败，已回滚：{}", e.message),
            ));
        }
        match sha256_file(target) {
            Ok(sha) if sha == file.source_sha256 => {}
            _ => {
                manifest.status = "failed".into();
                manifest.failure = Some("写入后 hash 不符".into());
                let _ = rollback_manifest(data, &manifest);
                let _ = write_manifest(data, &manifest);
                return Err(DeviceError::new(
                    "DEPLOY_FAILED",
                    "部署失败（写入校验不符），已回滚",
                ));
            }
        }
    }
    write_manifest(data, &manifest)?;
    prune(data, &manifest.id);
    Ok(manifest.summary())
}

fn prune(data: &Path, keep_id: &str) {
    let Ok(entries) = std::fs::read_dir(deployments_dir(data)) else {
        return;
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|id| id.starts_with("dep_"))
        .collect();
    ids.sort();
    while ids.len() > KEEP {
        let Some(old) = ids.first().cloned() else {
            break;
        };
        if old == keep_id {
            break;
        }
        let _ = std::fs::remove_dir_all(deployment_dir(data, &old));
        ids.remove(0);
    }
}

/// What the person is asked before a rollback.
pub fn rollback_text(data: &Path, id: &str) -> Result<String, DeviceError> {
    let manifest = load_manifest(data, id)?;
    if manifest.status == "rolled_back" {
        return Err(DeviceError::new(
            "DEPLOY_ALREADY_ROLLED_BACK",
            format!("{} 已经回滚过了", manifest.id),
        ));
    }
    let mut text = format!(
        "把部署 {} 回滚？\n\n目标：《{}》（{}）\n项目：{}\n",
        manifest.id, manifest.target.name, manifest.target.path, manifest.project
    );
    for file in &manifest.files {
        text.push_str(&format!(
            "\n  {} → {}{}",
            file.dest,
            if file.backup.is_some() {
                "恢复备份"
            } else {
                "删除部署的文件"
            },
            if file.backup.is_some() {
                ""
            } else {
                "（原本不存在）"
            }
        ));
    }
    text.push_str("\n\n回滚会覆盖目标里这些文件当前的内容。");
    Ok(text)
}

/// Restore the deployment (the caller asked the person first).
pub fn rollback_apply(data: &Path, id: &str) -> Result<Value, DeviceError> {
    let mut manifest = load_manifest(data, id)?;
    if manifest.status == "rolled_back" {
        return Err(DeviceError::new(
            "DEPLOY_ALREADY_ROLLED_BACK",
            format!("{} 已经回滚过了", manifest.id),
        ));
    }
    rollback_manifest(data, &manifest)?;
    manifest.status = "rolled_back".into();
    manifest.rolled_back_at = Some(iso_now());
    write_manifest(data, &manifest)?;
    Ok(manifest.summary())
}

/// Recent deployments (newest first).
pub fn list(data: &Path, limit: usize) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(deployments_dir(data)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|id| id.starts_with("dep_"))
        .collect();
    ids.sort();
    ids.reverse();
    ids.iter()
        .take(limit.clamp(1, 100))
        .filter_map(|id| load_manifest(data, id).ok())
        .map(|manifest| manifest.summary())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("target");
        let dir = target
            .join("tmp")
            .join(format!("deploy-{tag}-{}", short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        workspace::real(&dir).unwrap()
    }

    fn setup() -> (PathBuf, PathBuf, PathBuf, Target) {
        let data = tmp("data");
        let home = tmp("home");
        let target = tmp("target");
        let enforced = workspace::ensure_project(&data, &home, &[], "mod", None).unwrap();
        let project = PathBuf::from(enforced.project_dir);
        std::fs::create_dir_all(project.join("out")).unwrap();
        std::fs::write(project.join("out/mod.dll"), b"new-dll").unwrap();
        std::fs::create_dir_all(target.join("mods")).unwrap();
        std::fs::write(target.join("mods/mod.dll"), b"old-dll").unwrap();
        let deployed = add_target(&data, "game", target.to_str().unwrap()).unwrap();
        (data, project, target, deployed)
    }

    fn plan_one(data: &Path, project: &Path) -> Plan {
        let artifacts = json!([{"source": project.join("out/mod.dll").display().to_string(), "dest": "mods/mod.dll"}]);
        plan(data, "ags_test", "mod", &artifacts, "game", "first mod").unwrap()
    }

    #[test]
    fn deploy_backs_up_overwrites_and_rolls_back() {
        let (data, project, target, _deployed) = setup();
        let planned = plan_one(&data, &project);
        let summary = apply(&data, &planned).unwrap();
        assert_eq!(summary["status"], "applied");
        assert_eq!(
            std::fs::read_to_string(target.join("mods/mod.dll")).unwrap(),
            "new-dll"
        );
        let list = list(&data, 10);
        assert_eq!(list.len(), 1);
        let id = list[0]["id"].as_str().unwrap().to_string();
        assert_eq!(list[0]["files"], 1);
        // The backup kept the old bytes.
        let manifest = load_manifest(&data, &id).unwrap();
        let backup = manifest.files[0].backup.as_ref().unwrap();
        assert_eq!(
            std::fs::read_to_string(deployment_dir(&data, &id).join(&backup.file)).unwrap(),
            "old-dll"
        );
        rollback_apply(&data, &id).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("mods/mod.dll")).unwrap(),
            "old-dll"
        );
        assert_eq!(load_manifest(&data, &id).unwrap().status, "rolled_back");
    }

    #[test]
    fn a_new_file_is_removed_on_rollback() {
        let (data, project, target, _deployed) = setup();
        std::fs::create_dir_all(project.join("out")).unwrap();
        std::fs::write(project.join("out/new.txt"), b"new").unwrap();
        let artifacts = json!([{"source": project.join("out/new.txt").display().to_string(), "dest": "new.txt"}]);
        let planned = plan(&data, "s", "mod", &artifacts, "game", "").unwrap();
        let summary = apply(&data, &planned).unwrap();
        assert!(target.join("new.txt").is_file());
        rollback_apply(&data, summary["id"].as_str().unwrap()).unwrap();
        assert!(!target.join("new.txt").exists());
    }

    #[test]
    fn unregistered_targets_outside_artifacts_and_bad_dests_are_refused() {
        let (data, project, _target, _deployed) = setup();
        let outside = tmp("outside-file");
        std::fs::write(outside.join("x.dll"), b"x").unwrap();
        let artifacts_outside =
            json!([{"source": outside.join("x.dll").display().to_string(), "dest": "x.dll"}]);
        assert_eq!(
            plan(&data, "s", "mod", &artifacts_outside, "game", "")
                .unwrap_err()
                .code,
            "DENIED"
        );
        let artifacts = json!([{"source": project.join("out/mod.dll").display().to_string(), "dest": "../escape.dll"}]);
        assert_eq!(
            plan(&data, "s", "mod", &artifacts, "game", "")
                .unwrap_err()
                .code,
            "DEPLOY_DEST_INVALID"
        );
        let artifacts = json!([{"source": project.join("out/mod.dll").display().to_string(), "dest": "ok.dll"}]);
        assert_eq!(
            plan(&data, "s", "mod", &artifacts, "nope", "")
                .unwrap_err()
                .code,
            "DEPLOY_TARGET_UNKNOWN"
        );
    }

    #[test]
    fn a_failure_mid_apply_rolls_back_everything() {
        let (data, project, target, _deployed) = setup();
        std::fs::write(project.join("out/b.dll"), b"second").unwrap();
        std::fs::create_dir_all(target.join("mods")).unwrap();
        std::fs::write(target.join("mods/b.dll"), b"second-old").unwrap();
        let artifacts = json!([
            {"source": project.join("out/mod.dll").display().to_string(), "dest": "mods/mod.dll"},
            {"source": project.join("out/b.dll").display().to_string(), "dest": "mods/b.dll"},
        ]);
        let planned = plan(&data, "s", "mod", &artifacts, "game", "").unwrap();
        let error = apply_inner(&data, &planned, Some(1)).unwrap_err();
        assert_eq!(error.code, "DEPLOY_FAILED");
        assert_eq!(
            std::fs::read_to_string(target.join("mods/mod.dll")).unwrap(),
            "old-dll"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("mods/b.dll")).unwrap(),
            "second-old"
        );
        let manifest = load_manifest(&data, &planned.id).unwrap();
        assert_eq!(manifest.status, "failed");
    }

    #[test]
    fn targets_cannot_overlap_the_workspace_or_the_device_data() {
        let data = tmp("guards-data");
        let home = tmp("guards-home");
        let enforced = workspace::ensure_project(&data, &home, &[], "mod", None).unwrap();
        let project = PathBuf::from(enforced.project_dir);
        assert_eq!(
            add_target(&data, "bad", project.to_str().unwrap())
                .unwrap_err()
                .code,
            "DENIED"
        );
        assert_eq!(
            add_target(&data, "bad2", data.to_str().unwrap())
                .unwrap_err()
                .code,
            "DENIED"
        );
    }
}
