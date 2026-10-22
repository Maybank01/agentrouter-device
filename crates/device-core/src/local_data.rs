//! Named local data (Cloud `docs/development/DEVICES.md` §2.5): a folder on this computer the person registers once
//! under a name (`agentrouter data add 素材 D:\素材`). A space requires it by name (`requires.dependencies:
//! [{ local: 素材 }]`), never by path; placement's "can" finds the device that has it, and an instance reaches it as
//! `${deps.<as>.path}`.
//!
//! Rules:
//! - **Registered here only**: `config.json` `local_data`, written by `agentrouter data add|rename|remove` (or the web
//!   device page asking this computer's folder picker later). The cloud never adds one.
//! - **A grant**: in folder mode a registered folder joins the link's folders (writable) or its readable places
//!   (`--read-only`); removing it takes it out of a running link and the cloud stops the instances that used it
//!   (`LOCAL_DATA_REVOKED`).
//! - **Reported as `link.localData`**: `[{name, path, readOnly, present}]`; `present:false` when the folder is gone
//!   (unplugged drive), so placement refuses it with `LOCAL_DATA_MISSING` instead of failing later.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The most named folders one computer reports.
pub const MAX_LOCAL_DATA: usize = 64;

/// One named folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalData {
    /// What spaces call it (`素材`, `steam-saves`): 1–40 characters, no control characters, no path separators.
    pub name: String,
    /// The folder's real absolute path.
    pub path: String,
    /// Agents may read but never change it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
}

/// Why a name cannot be used, in the person's words; `None` when it can.
pub fn name_problem(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty() {
        return Some("名字不能是空的".into());
    }
    if n.chars().count() > 40 {
        return Some(format!("名字最多 40 个字：{n}"));
    }
    if n.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
        return Some(format!("名字里不能有斜杠或控制字符：{n}"));
    }
    None
}

/// The entry named `name` (names compare trimmed and case-insensitively, as a space writes them).
pub fn find<'a>(list: &'a [LocalData], name: &str) -> Option<&'a LocalData> {
    list.iter().find(|d| same_name(&d.name, name))
}

pub fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Add or replace `entry` (by name). Errors: a bad name, too many entries.
pub fn upsert(list: &mut Vec<LocalData>, entry: LocalData) -> Result<(), String> {
    if let Some(p) = name_problem(&entry.name) {
        return Err(p);
    }
    let entry = LocalData {
        name: entry.name.trim().to_string(),
        ..entry
    };
    if let Some(slot) = list.iter_mut().find(|d| same_name(&d.name, &entry.name)) {
        *slot = entry;
        return Ok(());
    }
    if list.len() >= MAX_LOCAL_DATA {
        return Err(format!("一台电脑最多 {MAX_LOCAL_DATA} 个命名文件夹"));
    }
    list.push(entry);
    Ok(())
}

/// Rename `from` to `to`. Errors: no such name, a bad new name, the new name taken.
pub fn rename(list: &mut [LocalData], from: &str, to: &str) -> Result<(), String> {
    if let Some(p) = name_problem(to) {
        return Err(p);
    }
    if !same_name(from, to) && find(list, to).is_some() {
        return Err(format!("已经有叫 {} 的文件夹了", to.trim()));
    }
    let entry = list
        .iter_mut()
        .find(|d| same_name(&d.name, from))
        .ok_or_else(|| format!("没有叫 {} 的文件夹", from.trim()))?;
    entry.name = to.trim().to_string();
    Ok(())
}

/// Remove `name`; whether it was there.
pub fn remove(list: &mut Vec<LocalData>, name: &str) -> bool {
    let before = list.len();
    list.retain(|d| !same_name(&d.name, name));
    list.len() != before
}

/// What a link reports (`link.localData`): every entry with whether its folder is there now.
pub fn view(list: &[LocalData]) -> Value {
    Value::Array(
        list.iter()
            .map(|d| {
                let mut v =
                    json!({"name": d.name, "path": d.path, "present": Path::new(&d.path).is_dir()});
                if d.read_only {
                    v["readOnly"] = json!(true);
                }
                v
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, path: &str) -> LocalData {
        LocalData {
            name: name.into(),
            path: path.into(),
            read_only: false,
        }
    }

    #[test]
    fn names_are_checked_and_unique_case_insensitively() {
        assert!(name_problem("素材").is_none());
        assert!(name_problem("  ").is_some());
        assert!(name_problem("a/b").is_some());
        assert!(name_problem(&"x".repeat(41)).is_some());
        let mut list = vec![];
        upsert(&mut list, entry(" Saves ", "/a")).unwrap();
        upsert(&mut list, entry("saves", "/b")).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, "/b");
        assert_eq!(find(&list, "SAVES").unwrap().name, "saves");
    }

    #[test]
    fn rename_and_remove() {
        let mut list = vec![entry("a", "/a"), entry("b", "/b")];
        assert!(rename(&mut list, "a", "B").is_err());
        assert!(rename(&mut list, "missing", "c").is_err());
        rename(&mut list, "a", "素材").unwrap();
        assert_eq!(list[0].name, "素材");
        assert!(remove(&mut list, "素材"));
        assert!(!remove(&mut list, "素材"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn the_view_says_whether_the_folder_is_there() {
        let here = std::env::temp_dir();
        let list = vec![
            LocalData {
                name: "tmp".into(),
                path: here.to_string_lossy().into_owned(),
                read_only: true,
            },
            entry("gone", "/no/such/folder/agentrouter-test"),
        ];
        let v = view(&list);
        assert_eq!(v[0]["present"], true);
        assert_eq!(v[0]["readOnly"], true);
        assert_eq!(v[1]["present"], false);
        assert!(v[1].get("readOnly").is_none());
    }
}
