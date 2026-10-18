//! The workspace root, the project registry and the path guards (workspace spec §1–3; M0 of the
//! device side). Every agent wakes up in one workspace root: the person picks a project name, never
//! a path; the root is created (idempotently) by this device, and a project is a directory under it.
//!
//! This module owns M0 only: root + project registry + canonical path guards. Git baseline and
//! worktrees are M1; the cloud binding fields are M2.
//!
//! Path rules (hard):
//! - root and project paths are real paths (`canonicalize`, links followed), so a junction or a
//!   symbolic link cannot smuggle a project out of the workspace or an external folder into it;
//! - a workspace must never be nested in, or contain, a linked folder that is not workspace-side
//!   (a game install, a program folder): those are read-only references / deployment targets;
//! - the device's own data folder (`workspaces.json`, keys, audit) is never a workspace path;
//! - Windows long paths go through `\\?\` internally; the registry stores the plain form people see.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gate::{self, display, inside, resolve_folders};
use crate::protocol::DeviceError;
use crate::util::iso_now;

/// The registry file in the device's data folder.
pub const FILE: &str = "workspaces.json";
pub const SCHEMA: u32 = 1;
const SLUG_MAX: usize = 40;
const NAME_MAX: usize = 60;

/// What a person sees when a path is a linked folder that is not the workspace (spec §5).
pub const GAME_DIR_MESSAGE: &str =
    "该目录是游戏安装目录，只能作为只读引用/部署目标；请选择或新建项目";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// What the person typed (trimmed).
    pub name: String,
    /// Where it lives under the root: `[a-z0-9-]{1,40}`.
    pub slug: String,
    /// The project directory as people write it (`C:\Users\me\Workspaces\mod`).
    pub path: String,
    /// Reserved for M1: `<project>/.worktrees`.
    pub worktrees_dir: String,
    /// A project created from a git repository (M1 makes new ones).
    #[serde(default)]
    pub has_git: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Registry {
    pub schema_version: u32,
    /// The workspace root as people write it.
    pub root: String,
    pub projects: Vec<Project>,
}

impl Registry {
    pub fn new(root: &Path) -> Registry {
        Registry {
            schema_version: SCHEMA,
            root: display(root),
            projects: Vec::new(),
        }
    }

    /// Not created yet: the caller fills in the default root (`ensure_root`/`listing`).
    fn empty() -> Registry {
        Registry {
            schema_version: SCHEMA,
            root: String::new(),
            projects: Vec::new(),
        }
    }
}

/// The default workspace root (spec §1: `%USERPROFILE%\Workspaces`, compatible with the existing
/// `C:\Users\<user>\Workspaces`).
pub fn default_root(home: &Path) -> PathBuf {
    home.join("Workspaces")
}

pub fn registry_path(data: &Path) -> PathBuf {
    data.join(FILE)
}

/// The registry on disk; a missing file is the default (root not created yet), a broken file is an
/// error rather than a silent reset (the projects in it are the person's directories).
pub fn load(data: &Path) -> Result<Registry, DeviceError> {
    let path = registry_path(data);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Registry::empty());
    };
    let registry: Registry = serde_json::from_str(&text).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_REGISTRY_INVALID",
            format!("工作区登记文件读不出来（{}）：{e}", display(&path)),
        )
    })?;
    if registry.schema_version != SCHEMA {
        return Err(DeviceError::new(
            "WORKSPACE_REGISTRY_INVALID",
            format!(
                "工作区登记文件版本不认识（{}），这台设备需要更新",
                registry.schema_version
            ),
        ));
    }
    Ok(registry)
}

/// Write the registry atomically: a crash never leaves a half-written file.
pub fn save(data: &Path, registry: &Registry) -> Result<(), DeviceError> {
    let dir = data.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("建不了设备数据目录 {}：{e}", display(&dir)),
        )
    })?;
    let path = registry_path(data);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|e| DeviceError::new("WORKSPACE_FAILED", format!("登记文件写不出来：{e}")))?;
    std::fs::write(&tmp, bytes).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("工作区登记文件写不进去（{}）：{e}", display(&tmp)),
        )
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("工作区登记文件换不上去（{}）：{e}", display(&path)),
        )
    })
}

/// A project name as the directory it becomes: lowercase ASCII, spaces and punctuation to `-`,
/// `[a-z0-9-]{1,40}`. Non-ASCII names are refused instead of guessed (a project name is an id here,
/// not a path).
pub fn slug(name: &str) -> Result<String, DeviceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DeviceError::new("WORKSPACE_INVALID", "项目名不能是空的"));
    }
    if name.chars().count() > NAME_MAX {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            format!("项目名太长（最多 {NAME_MAX} 个字）"),
        ));
    }
    if !name.is_ascii() {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            "项目名只能用英文字母、数字和 -（中文名请写成拼音或英文；目录名就是项目 id）",
        ));
    }
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if keep {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() || slug.len() > SLUG_MAX {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            format!("项目名要能变成 {SLUG_MAX} 个字符以内的目录名（字母、数字、-）"),
        ));
    }
    Ok(slug)
}

/// Windows: the `\\?\` form for filesystem calls, so a long path works (people never see this form).
fn verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\?\") {
            return path.to_path_buf();
        }
        // A drive path (`C:\…`); UNC paths are refused by gate::check_syntax before this point.
        let bytes = text.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\'
        {
            return PathBuf::from(format!(r"\\?\{text}"));
        }
    }
    path.to_path_buf()
}

fn create_dir(path: &Path) -> Result<(), DeviceError> {
    std::fs::create_dir_all(verbatim(path)).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("建不了目录 {}：{e}", display(path)),
        )
        .next("检查这个位置能不能写（权限、只读盘、路径太长）。")
    })
}

/// The real (canonical) form of a path that exists; links and junctions are resolved here.
fn real(path: &Path) -> Result<PathBuf, DeviceError> {
    std::fs::canonicalize(verbatim(path)).map_err(|e| {
        DeviceError::new(
            "WORKSPACE_FAILED",
            format!("看不到目录 {}：{e}", display(path)),
        )
    })
}

/// The plain form people and the registry use (`C:\…`), so a verbatim `\\?\C:\…` path compares equal.
fn plain(path: &Path) -> PathBuf {
    PathBuf::from(display(path))
}

fn strictly_inside(path: &Path, base: &Path) -> bool {
    inside(path, base) && !inside(base, path)
}

fn same(a: &Path, b: &Path) -> bool {
    inside(a, b) && inside(b, a)
}

/// Whether two stored paths are the same location (both forms are plain display paths).
fn same_path(a: &str, b: &str) -> bool {
    if a.trim().is_empty() || b.trim().is_empty() {
        return a.trim() == b.trim();
    }
    same(Path::new(a), Path::new(b))
}

fn root_conflict(registry: &Registry) -> DeviceError {
    DeviceError::new(
        "WORKSPACE_ROOT_CONFLICT",
        format!(
            "这台设备的工作区根已经是 {}，还有 {} 个项目；换根要先迁移这些项目",
            registry.root,
            registry.projects.len()
        ),
    )
}

/// A linked folder that is the workspace root (or above it) is workspace-side; the others are
/// external references: game installs, program folders, deployment targets.
fn external_linked(linked: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    linked
        .iter()
        .filter(|l| !inside(root, l.as_path()))
        .cloned()
        .collect()
}

/// A workspace root may not be nested in, or contain, an external linked folder, and it is never in
/// the device's own data folder.
pub fn guard_root(root: &Path, linked: &[PathBuf], data: &Path) -> Result<(), DeviceError> {
    let root = plain(root);
    let data = plain(data);
    let linked: Vec<PathBuf> = linked.iter().map(|p| plain(p)).collect();
    if inside(&root, &data) || inside(&data, &root) {
        return Err(DeviceError::denied(
            "工作区不能放在 AgentRouter 设备程序自己的数据目录里",
        ));
    }
    for folder in &linked {
        if same(&root, folder) {
            continue;
        }
        if strictly_inside(&root, folder) || strictly_inside(folder, &root) {
            return Err(DeviceError::denied(GAME_DIR_MESSAGE)
                .next("请换一个工作区位置（默认 %USERPROFILE%\\Workspaces），游戏目录只能作只读引用/部署目标。"));
        }
    }
    Ok(())
}

/// A project is strictly below the root, outside every external linked folder, and not in the app's data.
pub fn guard_project(
    project: &Path,
    root: &Path,
    linked: &[PathBuf],
    data: &Path,
) -> Result<(), DeviceError> {
    let project = plain(project);
    let root = plain(root);
    let data = plain(data);
    let linked: Vec<PathBuf> = linked.iter().map(|p| plain(p)).collect();
    if inside(&project, &data) || inside(&data, &project) {
        return Err(DeviceError::denied(
            "项目目录不能和 AgentRouter 设备程序自己的数据目录重叠",
        ));
    }
    // Any overlap with an external linked folder (a game install, a program folder) is out, including
    // a junction inside the root that resolves to it: it is not a project, it is a read-only reference.
    for folder in external_linked(&linked, &root) {
        if inside(&project, &folder) || inside(&folder, &project) {
            return Err(DeviceError::denied(GAME_DIR_MESSAGE)
                .next("项目只能在工作区里；游戏目录只能作只读引用/部署目标。"));
        }
    }
    if !strictly_inside(&project, &root) {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            format!("项目目录必须在工作区根下面：{}", display(&project)),
        ));
    }
    Ok(())
}

/// Root for a request: the registry's root (created if missing), or `root` when the cloud/person
/// names one (first call). Idempotent: the same root always comes back.
pub fn ensure_root(
    data: &Path,
    home: &Path,
    root: Option<&str>,
    linked: &[PathBuf],
) -> Result<PathBuf, DeviceError> {
    let mut registry = load(data)?;
    let wanted = match root.map(str::trim).filter(|r| !r.is_empty()) {
        Some(raw) => {
            gate::check_syntax(raw)?;
            PathBuf::from(raw)
        }
        None if !registry.root.trim().is_empty() => PathBuf::from(registry.root.trim()),
        None => default_root(home),
    };
    create_dir(&wanted)?;
    let real_root = real(&wanted)?;
    if !real_root.is_dir() {
        return Err(DeviceError::new(
            "WORKSPACE_INVALID",
            format!("工作区位置不是目录：{}", display(&real_root)),
        ));
    }
    guard_root(&real_root, linked, data)?;
    let shown = display(&real_root);
    // A root change with projects in place would orphan them: refuse instead of moving data.
    if !registry.root.trim().is_empty()
        && !same_path(&registry.root, &shown)
        && !registry.projects.is_empty()
    {
        return Err(root_conflict(&registry));
    }
    if registry.root != shown {
        registry.root = shown;
        save(data, &registry)?;
    }
    Ok(real_root)
}

/// The result of `workspace_ensure`: what the cloud stores in the session binding (M2 uses it).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Ensured {
    pub root: String,
    pub project: String,
    pub slug: String,
    pub project_dir: String,
    pub worktrees_dir: String,
    pub created: bool,
}

/// Create (or return) the project directory under the workspace root and register it. Idempotent:
/// the same project name always returns the same canonical directory.
pub fn ensure_project(
    data: &Path,
    home: &Path,
    linked: &[PathBuf],
    project: &str,
    root: Option<&str>,
) -> Result<Ensured, DeviceError> {
    let mut registry = load(data)?;
    let previous_root = registry.root.clone();
    let root_real = ensure_root(data, home, root, linked)?;
    let slug = slug(project)?;
    let shown_root = display(&root_real);
    // `ensure_root` already refuses a root change with projects; keep the belt here too.
    if !registry.projects.is_empty()
        && !previous_root.trim().is_empty()
        && !same_path(&previous_root, &shown_root)
    {
        return Err(root_conflict(&registry));
    }
    registry.root = shown_root;
    if let Some(existing) = registry.projects.iter().find(|p| p.slug == slug).cloned() {
        let project_real = real(Path::new(&existing.path))?;
        guard_project(&project_real, &root_real, linked, data)?;
        return Ok(Ensured {
            root: display(&root_real),
            project: existing.name,
            slug: existing.slug,
            project_dir: display(&project_real),
            worktrees_dir: existing.worktrees_dir,
            created: false,
        });
    }
    registry.root = display(&root_real);
    if let Some(existing) = registry.projects.iter().find(|p| p.slug == slug).cloned() {
        let project_real = real(Path::new(&existing.path))?;
        guard_project(&project_real, &root_real, linked, data)?;
        return Ok(Ensured {
            root: display(&root_real),
            project: existing.name,
            slug: existing.slug,
            project_dir: display(&project_real),
            worktrees_dir: existing.worktrees_dir,
            created: false,
        });
    }
    let project_path = root_real.join(&slug);
    create_dir(&project_path)?;
    let project_real = real(&project_path)?;
    guard_project(&project_real, &root_real, linked, data)?;
    let entry = Project {
        name: project.trim().to_string(),
        slug,
        path: display(&project_real),
        worktrees_dir: display(&project_real.join(".worktrees")),
        has_git: project_real.join(".git").exists(),
        created_at: iso_now(),
    };
    registry.projects.push(entry.clone());
    save(data, &registry)?;
    Ok(Ensured {
        root: display(&root_real),
        project: entry.name,
        slug: entry.slug,
        project_dir: display(&project_real),
        worktrees_dir: entry.worktrees_dir,
        created: true,
    })
}

/// `workspace_list`: what the registry knows; the root is reported even before it is created.
pub fn listing(data: &Path, home: &Path) -> Result<Registry, DeviceError> {
    let registry = load(data)?;
    let root = if registry.root.trim().is_empty() {
        display(&default_root(home))
    } else {
        registry.root.clone()
    };
    Ok(Registry { root, ..registry })
}

/// Turn the current linked folders into the guard's form (already canonical).
pub fn linked_folders(folders: &[String]) -> Vec<PathBuf> {
    resolve_folders(folders)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        // Unit tests are the lib target, where Cargo does not set CARGO_TARGET_TMPDIR; keep the
        // scratch folders under the build's own target folder (never the system temp one). The test
        // binary lives at `<target>/debug/deps/`, so `<target>/tmp` needs no `..` in the path.
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("target folder");
        let dir = target
            .join("tmp")
            .join("ar-workspace-unit")
            .join(format!("{tag}-{}", crate::util::short_id("")));
        std::fs::create_dir_all(&dir).unwrap();
        real(&dir).unwrap()
    }

    #[test]
    fn slugs_are_directory_ids() {
        assert_eq!(slug("Mod Arena").unwrap(), "mod-arena");
        assert_eq!(slug("  BvB.Mod  ").unwrap(), "bvb-mod");
        assert_eq!(slug("mod_2").unwrap(), "mod-2");
        assert_eq!(slug(&"a".repeat(39)).unwrap().len(), 39);
        assert_eq!(slug(&"a".repeat(41)).unwrap_err().code, "WORKSPACE_INVALID");
        assert_eq!(slug("悟空").unwrap_err().code, "WORKSPACE_INVALID");
        assert_eq!(slug(" ").unwrap_err().code, "WORKSPACE_INVALID");
    }

    #[test]
    fn a_root_never_swallows_a_linked_game_folder() {
        let data = tmp("data");
        let container = tmp("container");
        let game = container.join("game");
        std::fs::create_dir_all(&game).unwrap();
        let game = real(&game).unwrap();
        let linked = vec![game.clone()];
        // The workspace would sit inside the game install.
        assert_eq!(
            guard_root(&game.join("Workspace"), &linked, &data)
                .unwrap_err()
                .code,
            "DENIED"
        );
        // …or would contain it.
        assert_eq!(
            guard_root(&container, &linked, &data).unwrap_err().code,
            "DENIED"
        );
        // A root of its own is fine, and so is a linked root itself.
        let root = tmp("root");
        assert!(guard_root(&root, &linked, &data).is_ok());
        assert!(guard_root(&root, std::slice::from_ref(&root), &data).is_ok());
        assert!(
            guard_root(&root, &linked, &root).is_err(),
            "never in the data folder"
        );
    }

    #[test]
    fn a_project_stays_below_the_root_and_outside_linked_folders() {
        let data = tmp("data2");
        let root = tmp("root2");
        let game = tmp("game2");
        let linked = vec![root.clone(), game.clone()];
        let project = root.join("mod");
        std::fs::create_dir_all(&project).unwrap();
        let project = real(&project).unwrap();
        assert!(guard_project(&project, &root, &linked, &data).is_ok());
        // The root itself is not a project.
        assert_eq!(
            guard_project(&root, &root, &linked, &data)
                .unwrap_err()
                .code,
            "WORKSPACE_INVALID"
        );
        let elsewhere = tmp("elsewhere2");
        assert_eq!(
            guard_project(&elsewhere, &root, &linked, &data)
                .unwrap_err()
                .code,
            "WORKSPACE_INVALID"
        );
        assert!(guard_project(&game, &root, &linked, &data).is_err());
    }

    #[test]
    fn registry_round_trips_through_the_file() {
        let data = tmp("reg");
        let root = tmp("reg-root");
        let mut registry = Registry::new(&root);
        registry.projects.push(Project {
            name: "Mod".into(),
            slug: "mod".into(),
            path: display(&root.join("mod")),
            worktrees_dir: display(&root.join("mod").join(".worktrees")),
            has_git: false,
            created_at: "2026-10-18T00:00:00.000Z".into(),
        });
        save(&data, &registry).unwrap();
        assert_eq!(load(&data).unwrap(), registry);
        std::fs::write(registry_path(&data), "{not json").unwrap();
        assert_eq!(load(&data).unwrap_err().code, "WORKSPACE_REGISTRY_INVALID");
    }
}
