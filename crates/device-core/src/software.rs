//! Installed software with versions (Cloud `docs/development/DEVICES.md` §2.3): desktop programs and games, so a space can
//! require 「剪映」 or 「影之刃零」 and placement finds the device that has it.
//!
//! Rules (DEVICES §2.3):
//! - **Only the OS's and the launchers' own inventories**: registry keys, package databases through their query tools,
//!   bundle `Info.plist`, launcher manifests. Never a disk scan, never the person's files, never a discovered program run.
//! - **Applications, not libraries**: entries with a display name that are not system components, updates or frameworks;
//!   on Linux, packages that ship a desktop entry.
//! - **Stable ids** per source (`win:<key>`, `msix:<family>`, `gog:<id>`, `steam:<appid>`, `epic:<app>`,
//!   `macos:<bundle id>`, `deb:` / `rpm:` / `pacman:<package>`, `flatpak:<app id>`, `snap:<name>`, `manual:<name>`).
//! - **The person stays in charge**: `config.json` `software.sources` turns a source off, `software.hidden` keeps an id off
//!   the platform, `software.added` adds what discovery missed (`agentrouter software add|hide`).
//!
//! External programs are called with fixed flags only (no data on command lines, AGENTS.md); an unknown or unreadable
//! shape is skipped with a status line, never an error.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One installed program as its source states it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Software {
    /// The source's own stable id, prefixed with the source (`steam:1145360`).
    pub id: String,
    /// The display name.
    pub name: String,
    /// The version as the source states it (`6.2.0`, `build 18234567`); absent when the source has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Which inventory reported it (one of [`SOURCES`], or `manual`).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// The install folder or program, when the source names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Only one holder at a time (a game): the person marks it (`software add --exclusive`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclusive: bool,
}

/// Every discovery source, in the order they run. Names are the `config.json` `software.sources` keys.
pub const SOURCES: &[&str] = &[
    "windows-uninstall",
    "msix",
    "gog",
    "steam",
    "epic",
    "macos-apps",
    "dpkg",
    "rpm",
    "pacman",
    "flatpak",
    "snap",
];

/// The person's software settings (`config.json` `software`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SoftwareConfig {
    /// A source set to `false` is not read. Absent = on (owner draft default, DEVICES §7 item 4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, bool>,
    /// Ids never sent to the platform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    /// What discovery missed, added by the person.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<Software>,
}

impl SoftwareConfig {
    pub fn source_on(&self, source: &str) -> bool {
        self.sources.get(source).copied().unwrap_or(true)
    }
}

/// What a discovery run found, and one line per source that was skipped or partly read.
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub software: Vec<Software>,
    pub status: Vec<String>,
}

/// Runs every enabled source for this OS, then applies the person's hidden and added entries.
pub fn discover(config: &SoftwareConfig) -> Inventory {
    let mut inv = Inventory::default();
    for source in SOURCES {
        if !config.source_on(source) {
            continue;
        }
        match read_source(source) {
            Ok(found) => inv.software.extend(found),
            Err(Skip::NotHere) => {}
            Err(Skip::Failed(why)) => inv.status.push(format!("{source}: {why}")),
        }
    }
    finish(inv, config)
}

/// Dedupe (a launcher entry wins over the Uninstall key of the same folder), drop hidden ids, add the person's entries,
/// sort by id so two runs compare equal.
pub fn finish(mut inv: Inventory, config: &SoftwareConfig) -> Inventory {
    let launcher_paths: HashSet<String> = inv
        .software
        .iter()
        .filter(|s| matches!(s.source.as_str(), "steam" | "epic" | "gog"))
        .filter_map(|s| s.path.as_deref().map(norm_path))
        .collect();
    let hidden: HashSet<&str> = config.hidden.iter().map(String::as_str).collect();
    let mut by_id: BTreeMap<String, Software> = BTreeMap::new();
    for s in inv.software.drain(..) {
        if s.source == "windows-uninstall"
            && s.path
                .as_deref()
                .is_some_and(|p| launcher_paths.contains(&norm_path(p)))
        {
            continue;
        }
        by_id.entry(s.id.clone()).or_insert(s);
    }
    for s in &config.added {
        by_id.insert(s.id.clone(), s.clone());
    }
    by_id.retain(|id, _| !hidden.contains(id.as_str()));
    inv.software = by_id.into_values().collect();
    inv
}

/// The change between two reports: `{op:"software", added, removed, changed}` (DEVICES §2.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Delta {
    pub added: Vec<Software>,
    pub removed: Vec<String>,
    pub changed: Vec<Software>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

pub fn delta(before: &[Software], after: &[Software]) -> Delta {
    let old: BTreeMap<&str, &Software> = before.iter().map(|s| (s.id.as_str(), s)).collect();
    let new: BTreeMap<&str, &Software> = after.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut d = Delta::default();
    for (id, s) in &new {
        match old.get(id) {
            None => d.added.push((*s).clone()),
            Some(o) if o != s => d.changed.push((*s).clone()),
            Some(_) => {}
        }
    }
    d.removed = old
        .keys()
        .filter(|id| !new.contains_key(*id))
        .map(|id| id.to_string())
        .collect();
    d
}

/// A manual entry's id: `manual:<name>` lowercased, so adding the same name twice replaces it.
pub fn manual_id(name: &str) -> String {
    format!("manual:{}", name.trim().to_lowercase())
}

enum Skip {
    /// The source does not exist on this OS or machine (no Steam installed): nothing to say.
    NotHere,
    /// The source exists but could not be read; the reason goes into the status lines.
    Failed(String),
}

fn read_source(source: &str) -> Result<Vec<Software>, Skip> {
    match source {
        "steam" => steam(),
        "epic" => epic(),
        #[cfg(windows)]
        "windows-uninstall" => windows::uninstall(),
        #[cfg(windows)]
        "msix" => windows::msix(),
        #[cfg(windows)]
        "gog" => windows::gog(),
        #[cfg(target_os = "macos")]
        "macos-apps" => macos::apps(),
        #[cfg(target_os = "linux")]
        "dpkg" => linux::dpkg(),
        #[cfg(target_os = "linux")]
        "rpm" => linux::rpm(),
        #[cfg(target_os = "linux")]
        "pacman" => linux::pacman(),
        #[cfg(target_os = "linux")]
        "flatpak" => linux::flatpak(),
        #[cfg(target_os = "linux")]
        "snap" => linux::snap(),
        _ => Err(Skip::NotHere),
    }
}

fn norm_path(p: &str) -> String {
    p.trim_end_matches(['/', '\\'])
        .replace('\\', "/")
        .to_lowercase()
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------------------------------------------- Steam

fn steam() -> Result<Vec<Software>, Skip> {
    let dirs = steamlocate::locate_all().map_err(|_| Skip::NotHere)?;
    let mut out = Vec::new();
    for dir in dirs {
        let libraries = dir
            .libraries()
            .map_err(|e| Skip::Failed(format!("libraryfolders.vdf: {e}")))?;
        for library in libraries {
            let Ok(library) = library else { continue };
            for app in library.apps() {
                let Ok(app) = app else { continue };
                // StateFlags bit 4 = fully installed.
                if app.state_flags.is_some_and(|f| f.0 & 4 == 0) {
                    continue;
                }
                let Some(name) = non_empty(app.name.clone()) else {
                    continue;
                };
                out.push(Software {
                    id: format!("steam:{}", app.app_id),
                    name,
                    version: app.build_id.map(|b| format!("build {b}")),
                    source: "steam".into(),
                    publisher: None,
                    path: Some(library.resolve_app_dir(&app).to_string_lossy().into_owned()),
                    exclusive: false,
                });
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------------------------------------------- Epic

fn epic_manifest_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("ProgramData")
            .map(|p| PathBuf::from(p).join("Epic/EpicGamesLauncher/Data/Manifests"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library/Application Support/Epic/EpicGamesLauncher/Data/Manifests")
        })
    } else {
        None
    }
}

fn epic() -> Result<Vec<Software>, Skip> {
    let dir = epic_manifest_dir().ok_or(Skip::NotHere)?;
    let entries = std::fs::read_dir(&dir).map_err(|_| Skip::NotHere)?;
    let mut out = Vec::new();
    let mut unreadable = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("item") {
            continue;
        }
        match std::fs::read(&path).ok().and_then(|b| epic_item(&b)) {
            Some(Some(s)) => out.push(s),
            Some(None) => {}
            None => unreadable += 1,
        }
    }
    if unreadable > 0 && out.is_empty() {
        return Err(Skip::Failed(format!(
            "{unreadable} manifest(s) in {} had an unknown shape",
            dir.display()
        )));
    }
    Ok(out)
}

/// One Epic `.item` manifest: `None` = unknown shape, `Some(None)` = not an installed game.
fn epic_item(bytes: &[u8]) -> Option<Option<Software>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Item {
        display_name: Option<String>,
        app_name: Option<String>,
        app_version_string: Option<String>,
        install_location: Option<String>,
        #[serde(rename = "bIsIncompleteInstall", default)]
        incomplete: bool,
    }
    let item: Item = serde_json::from_slice(bytes).ok()?;
    let (Some(app), Some(name)) = (non_empty(item.app_name), non_empty(item.display_name)) else {
        return Some(None);
    };
    if item.incomplete {
        return Some(None);
    }
    Some(Some(Software {
        id: format!("epic:{app}"),
        name,
        version: non_empty(item.app_version_string),
        source: "epic".into(),
        publisher: None,
        path: non_empty(item.install_location),
        exclusive: false,
    }))
}

// ------------------------------------------------------------------------------------------------------------- Windows

#[cfg(windows)]
mod windows {
    use super::{Skip, Software, non_empty};
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};

    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

    /// "Apps & features": the Uninstall keys, per machine (64- and 32-bit views) and per user.
    pub(super) fn uninstall() -> Result<Vec<Software>, Skip> {
        let mut out = Vec::new();
        for (hive, path) in [
            (HKEY_LOCAL_MACHINE, UNINSTALL),
            (HKEY_LOCAL_MACHINE, UNINSTALL_32),
            (HKEY_CURRENT_USER, UNINSTALL),
        ] {
            let Ok(root) = RegKey::predef(hive).open_subkey_with_flags(path, KEY_READ) else {
                continue;
            };
            for name in root.enum_keys().flatten() {
                let Ok(key) = root.open_subkey_with_flags(&name, KEY_READ) else {
                    continue;
                };
                if let Some(s) = uninstall_entry(&name, &key) {
                    out.push(s);
                }
            }
        }
        Ok(out)
    }

    fn uninstall_entry(key_name: &str, key: &RegKey) -> Option<Software> {
        let get = |v: &str| non_empty(key.get_value::<String, _>(v).ok());
        let name = get("DisplayName")?;
        if key.get_value::<u32, _>("SystemComponent").unwrap_or(0) == 1
            || get("ParentKeyName").is_some()
        {
            return None;
        }
        if let Some(kind) = get("ReleaseType") {
            let kind = kind.to_lowercase();
            if kind.contains("update") || kind.contains("hotfix") || kind.contains("service pack") {
                return None;
            }
        }
        Some(Software {
            id: format!("win:{key_name}"),
            name,
            version: get("DisplayVersion"),
            source: "windows-uninstall".into(),
            publisher: get("Publisher"),
            path: get("InstallLocation"),
            exclusive: false,
        })
    }

    /// GOG Galaxy's own registry entries (also under Uninstall; the launcher entry wins by path).
    pub(super) fn gog() -> Result<Vec<Software>, Skip> {
        let root = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(r"SOFTWARE\WOW6432Node\GOG.com\Games", KEY_READ)
            .map_err(|_| Skip::NotHere)?;
        let mut out = Vec::new();
        for id in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&id, KEY_READ) else {
                continue;
            };
            let get = |v: &str| non_empty(key.get_value::<String, _>(v).ok());
            let Some(name) = get("gameName") else {
                continue;
            };
            out.push(Software {
                id: format!("gog:{id}"),
                name,
                version: get("ver"),
                source: "gog".into(),
                publisher: None,
                path: get("path"),
                exclusive: false,
            });
        }
        Ok(out)
    }

    /// Store / MSIX apps and Xbox PC games of the current user (`PackageManager.FindPackagesForUser("")`), without
    /// framework, resource and system-signed packages.
    pub(super) fn msix() -> Result<Vec<Software>, Skip> {
        use ::windows::ApplicationModel::PackageSignatureKind;
        use ::windows::Management::Deployment::PackageManager;
        use ::windows::core::HSTRING;
        let failed = |e: ::windows::core::Error| Skip::Failed(format!("PackageManager: {e}"));
        let manager = PackageManager::new().map_err(failed)?;
        let packages = manager
            .FindPackagesByUserSecurityId(&HSTRING::new())
            .map_err(failed)?;
        let mut out = Vec::new();
        for package in packages {
            if package.IsFramework().unwrap_or(true)
                || package.IsResourcePackage().unwrap_or(true)
                || package
                    .SignatureKind()
                    .map_or(true, |k| k == PackageSignatureKind::System)
            {
                continue;
            }
            let Ok(id) = package.Id() else { continue };
            let Some(family) = id
                .FamilyName()
                .ok()
                .map(|f| f.to_string())
                .filter(|f| !f.is_empty())
            else {
                continue;
            };
            let name = package
                .DisplayName()
                .ok()
                .map(|n| n.to_string())
                .filter(|n| !n.trim().is_empty() && !n.starts_with("ms-resource:"))
                .or_else(|| id.Name().ok().map(|n| n.to_string()));
            let Some(name) = name else { continue };
            let version = id
                .Version()
                .ok()
                .map(|v| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision));
            out.push(Software {
                id: format!("msix:{family}"),
                name,
                version,
                source: "msix".into(),
                publisher: package
                    .PublisherDisplayName()
                    .ok()
                    .map(|p| p.to_string())
                    .filter(|p| !p.is_empty()),
                path: package
                    .InstalledPath()
                    .ok()
                    .map(|p| p.to_string())
                    .filter(|p| !p.is_empty()),
                exclusive: false,
            });
        }
        Ok(out)
    }
}

// --------------------------------------------------------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod macos {
    use super::{Skip, Software, non_empty, run_fixed};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// App bundles in `/Applications` and `~/Applications`, plus those Spotlight knows elsewhere (not `/System`).
    pub(super) fn apps() -> Result<Vec<Software>, Skip> {
        let mut bundles: BTreeSet<PathBuf> = BTreeSet::new();
        let mut roots = vec![
            PathBuf::from("/Applications"),
            PathBuf::from("/Applications/Utilities"),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Applications"));
        }
        for root in roots {
            for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("app") {
                    bundles.insert(p);
                }
            }
        }
        if let Some(text) = run_fixed(
            "mdfind",
            &["kMDItemContentType == 'com.apple.application-bundle'"],
            Duration::from_secs(20),
        ) {
            for line in text.lines() {
                let p = Path::new(line.trim());
                let s = line.trim();
                // Not system apps, not helper apps nested in another bundle.
                if s.is_empty() || s.starts_with("/System/") || s.starts_with("/Library/Apple/") {
                    continue;
                }
                if s.trim_end_matches(".app").contains(".app/") {
                    continue;
                }
                bundles.insert(p.to_path_buf());
            }
        }
        Ok(bundles.iter().filter_map(|b| bundle(b)).collect())
    }

    fn bundle(path: &Path) -> Option<Software> {
        let info = plist::Value::from_file(path.join("Contents/Info.plist")).ok()?;
        let dict = info.as_dictionary()?;
        let get = |k: &str| non_empty(dict.get(k).and_then(|v| v.as_string()).map(str::to_string));
        let id = get("CFBundleIdentifier")?;
        let name = get("CFBundleDisplayName")
            .or_else(|| get("CFBundleName"))
            .or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
        Some(Software {
            id: format!("macos:{id}"),
            name,
            version: get("CFBundleShortVersionString").or_else(|| get("CFBundleVersion")),
            source: "macos-apps".into(),
            publisher: None,
            path: Some(path.to_string_lossy().into_owned()),
            exclusive: false,
        })
    }
}

// --------------------------------------------------------------------------------------------------------------- Linux

#[cfg(target_os = "linux")]
mod linux {
    use super::{Skip, Software, non_empty, run_fixed};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    const APPS_DIR: &str = "/usr/share/applications/";

    /// The desktop entries in `/usr/share/applications`: file name → display name (XDG Desktop Entry spec; only
    /// `Type=Application`, not `NoDisplay` / `Hidden`).
    fn desktop_names() -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for entry in std::fs::read_dir(APPS_DIR).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(name) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| super::desktop_entry_name(&t))
            {
                out.insert(p.to_string_lossy().into_owned(), name);
            }
        }
        out
    }

    /// Packages that own a shown desktop entry, with that entry's name and the package version.
    fn packaged(
        source: &str,
        owners: Vec<(String, String)>,
        versions: BTreeMap<String, String>,
    ) -> Vec<Software> {
        let names = desktop_names();
        let mut by_package: BTreeMap<String, String> = BTreeMap::new();
        for (package, file) in owners {
            if let Some(name) = names.get(&file) {
                by_package.entry(package).or_insert_with(|| name.clone());
            }
        }
        by_package
            .into_iter()
            .map(|(package, name)| Software {
                id: format!("{source}:{package}"),
                name,
                version: versions.get(&package).cloned(),
                source: source.into(),
                publisher: None,
                path: None,
                exclusive: false,
            })
            .collect()
    }

    fn tab_pairs(text: &str) -> BTreeMap<String, String> {
        text.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(a, b)| (a.trim().to_string(), b.trim().to_string()))
            .collect()
    }

    /// Debian / Ubuntu: `dpkg-query -S` for the desktop entries' owners, `dpkg-query -W` for versions.
    pub(super) fn dpkg() -> Result<Vec<Software>, Skip> {
        if crate::shell::find_on_path("dpkg-query").is_none() {
            return Err(Skip::NotHere);
        }
        let owners = run_fixed(
            "dpkg-query",
            &["-S", "/usr/share/applications/*.desktop"],
            Duration::from_secs(20),
        )
        .ok_or_else(|| Skip::Failed("dpkg-query -S did not answer".into()))?;
        let owners = owners
            .lines()
            .filter_map(|l| l.split_once(": "))
            .flat_map(|(pkgs, file)| {
                pkgs.split(", ")
                    .map(|p| {
                        (
                            p.split(':').next().unwrap_or(p).trim().to_string(),
                            file.trim().to_string(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let versions = run_fixed(
            "dpkg-query",
            &["-W", "-f", "${Package}\t${Version}\n"],
            Duration::from_secs(20),
        )
        .map(|t| tab_pairs(&t))
        .unwrap_or_default();
        Ok(packaged("deb", owners, versions))
    }

    /// Fedora / openSUSE: one `rpm -qa` listing each package's files, filtered to desktop entries.
    pub(super) fn rpm() -> Result<Vec<Software>, Skip> {
        if crate::shell::find_on_path("rpm").is_none()
            || crate::shell::find_on_path("dpkg-query").is_some()
        {
            return Err(Skip::NotHere);
        }
        let text = run_fixed(
            "rpm",
            &[
                "-qa",
                "--qf",
                "[%{NAME}\t%{VERSION}-%{RELEASE}\t%{FILENAMES}\n]",
            ],
            Duration::from_secs(30),
        )
        .ok_or_else(|| Skip::Failed("rpm -qa did not answer".into()))?;
        let mut owners = Vec::new();
        let mut versions = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(name), Some(version), Some(file)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if file.starts_with(APPS_DIR) && file.ends_with(".desktop") {
                owners.push((name.to_string(), file.to_string()));
                versions.insert(name.to_string(), version.to_string());
            }
        }
        Ok(packaged("rpm", owners, versions))
    }

    /// Arch: `pacman -Ql` for owners, `pacman -Q` for versions.
    pub(super) fn pacman() -> Result<Vec<Software>, Skip> {
        if crate::shell::find_on_path("pacman").is_none() {
            return Err(Skip::NotHere);
        }
        let files = run_fixed("pacman", &["-Ql"], Duration::from_secs(30))
            .ok_or_else(|| Skip::Failed("pacman -Ql did not answer".into()))?;
        let owners = files
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(_, f)| f.starts_with(APPS_DIR) && f.ends_with(".desktop"))
            .map(|(p, f)| (p.to_string(), f.to_string()))
            .collect();
        let versions = run_fixed("pacman", &["-Q"], Duration::from_secs(20))
            .map(|t| {
                t.lines()
                    .filter_map(|l| l.split_once(' '))
                    .map(|(a, b)| (a.to_string(), b.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(packaged("pacman", owners, versions))
    }

    /// Flatpak apps (`flatpak list --app`).
    pub(super) fn flatpak() -> Result<Vec<Software>, Skip> {
        if crate::shell::find_on_path("flatpak").is_none() {
            return Err(Skip::NotHere);
        }
        let text = run_fixed(
            "flatpak",
            &["list", "--app", "--columns=application,name,version"],
            Duration::from_secs(20),
        )
        .ok_or_else(|| Skip::Failed("flatpak list did not answer".into()))?;
        Ok(text
            .lines()
            .filter_map(|l| {
                let mut c = l.split('\t');
                let app = non_empty(c.next().map(str::to_string))?;
                let name = non_empty(c.next().map(str::to_string)).unwrap_or_else(|| app.clone());
                Some(Software {
                    id: format!("flatpak:{app}"),
                    name,
                    version: non_empty(c.next().map(str::to_string)),
                    source: "flatpak".into(),
                    publisher: None,
                    path: None,
                    exclusive: false,
                })
            })
            .collect())
    }

    /// Snaps of type `app` from snapd's REST API (`GET /v2/snaps` on `/run/snapd.socket`).
    pub(super) fn snap() -> Result<Vec<Software>, Skip> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let socket = PathBuf::from("/run/snapd.socket");
        if !socket.exists() {
            return Err(Skip::NotHere);
        }
        let failed = |e: std::io::Error| Skip::Failed(format!("snapd /v2/snaps: {e}"));
        let mut stream = UnixStream::connect(&socket).map_err(failed)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(failed)?;
        stream
            .write_all(b"GET /v2/snaps HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .map_err(failed)?;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).map_err(failed)?;
        let text = String::from_utf8_lossy(&body);
        let json = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .unwrap_or_default();
        super::snaps(json).ok_or_else(|| Skip::Failed("snapd /v2/snaps: unknown answer".into()))
    }
}

/// A desktop entry's display name when it is a shown application (`[Desktop Entry]` group only).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desktop_entry_name(text: &str) -> Option<String> {
    let mut in_group = false;
    let (mut name, mut kind, mut hidden) = (None, None, false);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "Name" => name = Some(v.trim().to_string()),
                "Type" => kind = Some(v.trim().to_string()),
                "NoDisplay" | "Hidden" if v.trim().eq_ignore_ascii_case("true") => hidden = true,
                _ => {}
            }
        }
    }
    if hidden || kind.as_deref() != Some("Application") {
        return None;
    }
    non_empty(name)
}

/// snapd's `/v2/snaps` answer → the `app` snaps.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn snaps(json: &str) -> Option<Vec<Software>> {
    #[derive(Deserialize)]
    struct Answer {
        result: Vec<Snap>,
    }
    #[derive(Deserialize)]
    struct Snap {
        name: String,
        title: Option<String>,
        version: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
        publisher: Option<Publisher>,
    }
    #[derive(Deserialize)]
    struct Publisher {
        #[serde(rename = "display-name")]
        display_name: Option<String>,
    }
    let answer: Answer = serde_json::from_str(json).ok()?;
    Some(
        answer
            .result
            .into_iter()
            .filter(|s| s.kind.as_deref() == Some("app"))
            .map(|s| Software {
                id: format!("snap:{}", s.name),
                name: non_empty(s.title).unwrap_or_else(|| s.name.clone()),
                version: non_empty(s.version),
                source: "snap".into(),
                publisher: s.publisher.and_then(|p| non_empty(p.display_name)),
                path: None,
                exclusive: false,
            })
            .collect(),
    )
}

/// A program's stdout, run with fixed flags, or None when it is missing, fails or takes longer than `limit` (killed).
/// Stdout is read while the program runs, so a large listing cannot fill the pipe and stall it.
#[cfg_attr(windows, allow(dead_code))]
fn run_fixed(program: &str, args: &[&str], limit: Duration) -> Option<String> {
    let path: PathBuf = crate::shell::find_on_path(program)?;
    let mut child = Command::new(path)
        .args(args)
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut buf)
            .ok()
            .map(|_| buf)
    });
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()??;
    // dpkg-query -S exits 1 when one pattern has no owner, with the rest still printed.
    if !status.success() && bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The id a path-like or name-like `software add` target gets, and whether `p` exists.
pub fn manual_entry(
    name: &str,
    path: Option<&Path>,
    version: Option<&str>,
    exclusive: bool,
) -> Software {
    Software {
        id: manual_id(name),
        name: name.trim().to_string(),
        version: version.map(str::to_string).filter(|v| !v.trim().is_empty()),
        source: "manual".into(),
        publisher: None,
        path: path.map(|p| p.to_string_lossy().into_owned()),
        exclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sw(id: &str, source: &str, path: Option<&str>, version: Option<&str>) -> Software {
        Software {
            id: id.into(),
            name: id.into(),
            version: version.map(str::to_string),
            source: source.into(),
            publisher: None,
            path: path.map(str::to_string),
            exclusive: false,
        }
    }

    #[test]
    fn launcher_entries_win_over_the_uninstall_key_of_the_same_folder() {
        let inv = Inventory {
            software: vec![
                sw(
                    "win:Steam App 1145360",
                    "windows-uninstall",
                    Some(r"D:\SteamLibrary\steamapps\common\Hades\"),
                    None,
                ),
                sw(
                    "steam:1145360",
                    "steam",
                    Some("D:/SteamLibrary/steamapps/common/Hades"),
                    Some("build 9"),
                ),
                sw(
                    "win:OBS Studio",
                    "windows-uninstall",
                    Some(r"C:\Program Files\obs-studio"),
                    Some("31.1.2"),
                ),
            ],
            status: vec![],
        };
        let ids: Vec<_> = finish(inv, &SoftwareConfig::default())
            .software
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["steam:1145360", "win:OBS Studio"]);
    }

    #[test]
    fn hidden_ids_are_dropped_and_added_entries_kept() {
        let config = SoftwareConfig {
            hidden: vec!["win:OBS Studio".into()],
            added: vec![manual_entry(
                "影之刃零",
                Some(Path::new("D:/Games/PBZ")),
                Some("1.2"),
                true,
            )],
            ..Default::default()
        };
        let inv = Inventory {
            software: vec![sw("win:OBS Studio", "windows-uninstall", None, None)],
            status: vec![],
        };
        let out = finish(inv, &config).software;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "manual:影之刃零");
        assert!(out[0].exclusive);
    }

    #[test]
    fn sources_are_on_unless_turned_off() {
        let config: SoftwareConfig =
            serde_json::from_str(r#"{"sources":{"steam":false}}"#).unwrap();
        assert!(!config.source_on("steam"));
        assert!(config.source_on("epic"));
    }

    #[test]
    fn delta_names_added_removed_and_changed() {
        let before = vec![sw("a", "x", None, Some("1")), sw("b", "x", None, None)];
        let after = vec![sw("a", "x", None, Some("2")), sw("c", "x", None, None)];
        let d = delta(&before, &after);
        assert_eq!(
            d.added.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
        assert_eq!(d.removed, ["b"]);
        assert_eq!(d.changed[0].version.as_deref(), Some("2"));
        assert!(delta(&after, &after).is_empty());
    }

    #[test]
    fn epic_manifests_are_read_defensively() {
        let ok = br#"{"DisplayName":"Fortnite","AppName":"Fortnite","AppVersionString":"++Fortnite+Release-31.0","InstallLocation":"C:/Epic/Fortnite"}"#;
        let s = epic_item(ok).unwrap().unwrap();
        assert_eq!(s.id, "epic:Fortnite");
        assert_eq!(s.version.as_deref(), Some("++Fortnite+Release-31.0"));
        let partial = br#"{"DisplayName":"X","AppName":"X","bIsIncompleteInstall":true}"#;
        assert!(epic_item(partial).unwrap().is_none());
        assert!(epic_item(b"not json").is_none());
    }

    #[test]
    fn desktop_entries_name_only_shown_applications() {
        let shown = "[Desktop Entry]\nType=Application\nName=OBS Studio\nName[zh_CN]=OBS\n[Desktop Action x]\nName=Other\n";
        assert_eq!(desktop_entry_name(shown).as_deref(), Some("OBS Studio"));
        assert!(
            desktop_entry_name("[Desktop Entry]\nType=Application\nName=X\nNoDisplay=true\n")
                .is_none()
        );
        assert!(desktop_entry_name("[Desktop Entry]\nType=Link\nName=X\n").is_none());
    }

    #[test]
    fn snapd_answer_keeps_app_snaps() {
        let json = r#"{"result":[{"name":"core22","type":"base","version":"1"},{"name":"vlc","title":"VLC","type":"app","version":"3.0.20","publisher":{"display-name":"VideoLAN"}}]}"#;
        let out = snaps(json).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "snap:vlc");
        assert_eq!(out[0].publisher.as_deref(), Some("VideoLAN"));
    }

    #[test]
    fn discovery_runs_on_this_machine_without_failing() {
        let inv = discover(&SoftwareConfig::default());
        let ids: HashSet<_> = inv.software.iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids.len(), inv.software.len(), "ids are unique");
    }
}
