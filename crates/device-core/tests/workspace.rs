//! M0 of the workspace model (workspace spec §1–3): the root, the project registry and the canonical
//! path guards, through the device actions the cloud will call in M1/M2. No git and no worktrees here.

mod common;

use std::path::{Path, PathBuf};

use agentrouter_device::config::Access;
use agentrouter_device::workspace;
use common::{ControlPlane, call, device, device_workspaces, temp_dir};
use serde_json::json;

/// Create a junction (Windows, no admin) or a symbolic link (Unix); `None` when the OS refuses.
fn link_dir(target: &Path, link: &Path) -> Option<()> {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .ok()?;
        return out.status.success().then_some(());
    }
    #[cfg(unix)]
    {
        return std::os::unix::fs::symlink(target, link).ok();
    }
    #[allow(unreachable_code)]
    None
}

#[test]
fn the_switch_is_off_until_workspace_init() {
    let cp = ControlPlane::new();
    let data = temp_dir("ws-off");
    let d = device(&data, Access::Folders, &[], &cp);
    for action in ["workspace_ensure", "workspace_list"] {
        assert_eq!(
            call(&d, &cp, action, json!({"project": "mod"}))
                .unwrap_err()
                .0,
            "WORKSPACE_DISABLED",
            "{action}"
        );
    }
}

#[test]
fn workspace_ensure_is_idempotent_and_lists_projects() {
    let cp = ControlPlane::new();
    let data = temp_dir("ws-idem");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    let first = call(&d, &cp, "workspace_ensure", json!({"project": "Mod Arena"})).unwrap();
    assert_eq!(first["slug"], "mod-arena");
    assert_eq!(first["created"], true);
    let dir = PathBuf::from(first["projectDir"].as_str().unwrap());
    assert_eq!(
        PathBuf::from(first["worktreesDir"].as_str().unwrap()),
        dir.join(".worktrees")
    );
    assert!(dir.is_dir(), "{}", dir.display());
    // The same name, a different spelling: same canonical directory, nothing new.
    let again = call(&d, &cp, "workspace_ensure", json!({"project": "mod-arena"})).unwrap();
    assert_eq!(again["projectDir"], first["projectDir"]);
    assert_eq!(again["created"], false);
    let list = call(&d, &cp, "workspace_list", json!({})).unwrap();
    assert_eq!(list["enabled"], true);
    assert_eq!(list["projects"].as_array().unwrap().len(), 1);
    assert_eq!(list["projects"][0]["slug"], "mod-arena");
}

#[test]
fn a_game_install_is_never_a_workspace_or_a_project() {
    let cp = ControlPlane::new();
    let data = temp_dir("ws-game-data");
    let game = temp_dir("ws-game-install");
    // The linked folder stands in for the game install (fully linked at access=folders).
    let d = device_workspaces(&data, Access::Folders, &[&game], &cp);
    let nested = game.join("Workspace");
    let refused = call(
        &d,
        &cp,
        "workspace_ensure",
        json!({"project": "mod", "root": nested.display().to_string()}),
    )
    .unwrap_err();
    assert_eq!(refused.0, "DENIED");
    assert!(refused.1.contains("游戏安装目录"), "{}", refused.1);
    // A root of its own works, and the project does not contain the linked folder.
    let ok = call(&d, &cp, "workspace_ensure", json!({"project": "mod"})).unwrap();
    let project = PathBuf::from(ok["projectDir"].as_str().unwrap());
    let root = PathBuf::from(ok["root"].as_str().unwrap());
    assert!(workspace::guard_project(&project, &root, std::slice::from_ref(&game), &data).is_ok());
    assert!(workspace::guard_root(&game, &[], &data).is_ok());
}

#[test]
fn a_junction_inside_the_root_cannot_escape_to_a_linked_folder() {
    let cp = ControlPlane::new();
    let data = temp_dir("ws-link-data");
    let game = temp_dir("ws-link-game");
    let d = device_workspaces(&data, Access::Folders, &[&game], &cp);
    // Initialize the workspace first, then put a link in it named like the project we are about to ask for.
    let ensured = call(&d, &cp, "workspace_ensure", json!({"project": "plain"})).unwrap();
    let root = PathBuf::from(ensured["root"].as_str().unwrap());
    let escape = root.join("escape");
    if link_dir(&game, &escape).is_none() {
        eprintln!("the OS refused to create a junction/symlink; skipping the link case");
        return;
    }
    let refused = call(&d, &cp, "workspace_ensure", json!({"project": "escape"})).unwrap_err();
    assert_eq!(refused.0, "DENIED", "{}", refused.1);
    assert!(
        refused.1.contains("游戏安装目录") || refused.1.contains("工作区"),
        "{}",
        refused.1
    );
}

#[test]
fn changing_the_root_with_projects_in_place_is_refused() {
    let cp = ControlPlane::new();
    let data = temp_dir("ws-root-conflict");
    let other = temp_dir("ws-other-root");
    let d = device_workspaces(&data, Access::Folders, &[], &cp);
    call(&d, &cp, "workspace_ensure", json!({"project": "one"})).unwrap();
    let refused = call(
        &d,
        &cp,
        "workspace_ensure",
        json!({"project": "two", "root": other.display().to_string()}),
    )
    .unwrap_err();
    assert_eq!(refused.0, "WORKSPACE_ROOT_CONFLICT");
    assert!(
        refused.1.contains("迁移"),
        "the message says what to do: {}",
        refused.1
    );
}
