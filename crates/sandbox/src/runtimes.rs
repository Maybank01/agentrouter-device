//! The runtimes local tools run on (device #116, Cloud #2383): folder mode never asks the person to install anything
//! (the product is for ordinary people). srt is a Node program and so are `npx` servers; `uvx` servers (serena, in the
//! default bundle) need uv. The device provisions both itself, once, in `sandbox setup` (the double-click start and
//! the first `link` run it):
//!
//! - **Pinned:** one version per runtime and the project's official portable archive per OS and architecture, its
//!   SHA-256 written here. Node: from the release's `SHASUMS256.txt`, whose signature was checked against the Node.js
//!   release keys (`nodejs/release-keys`) when the pin was set. uv: the release's `.sha256` files (the same on GitHub
//!   and on Astral's mirror). An archive that does not match is deleted, never unpacked.
//! - **Sources:** the project's own download host first, then a mirror (npmmirror for Node, GitHub for uv); a source
//!   that stays too slow is left for the next one. The download is in-process (no curl, PowerShell or installer
//!   program) and lands in `<data>-shared/runtimes/` beside the data folder, never in the temp folder
//!   (docs/AV-HYGIENE.md).
//! - **One path:** srt, the Windows sandbox entry, `npm ci`, and `npx`/`uvx` servers use these runtimes, never the
//!   person's own Node or uv (whatever version that is, if any). They go first on a server's PATH.
//! - **Bumping a pin:** the new version and all six digests, re-vetted (licence, release notes). The new folder is put
//!   in place first; the old version's folder is removed after.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

/// One runtime the device provisions.
#[derive(Debug)]
pub struct Pinned {
    /// Shown to the person (`Node.js`, `uv`).
    pub title: &'static str,
    /// The folder prefix (`node`, `uv`).
    pub name: &'static str,
    pub version: &'static str,
    /// Download URLs in order; `{version}` and `{file}` are filled in.
    pub sources: &'static [&'static str],
    pub archives: &'static [Archive],
    /// The folder with the programs, inside the unpacked runtime, per OS family (`""` is the runtime folder itself).
    pub bin_windows: &'static str,
    pub bin_unix: &'static str,
    /// Its programs (without `.exe`/`.cmd`); the first one must exist for the runtime to count as installed.
    pub programs: &'static [&'static str],
}

/// The official archive for one OS and architecture (`std::env::consts` names).
#[derive(Debug)]
pub struct Archive {
    pub os: &'static str,
    pub arch: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

/// Node.js 22 LTS ("Jod", maintained until 2027-04; srt needs 22.12+). 22.23.4 is the 2026-10-20 security release;
/// SHASUMS256.txt signed by release key 5BE8A3F6C8A5C01D106C0AD820B1A390B168D356 (Antoine du Hamel), verified
/// 2026-10-21. Not 24 yet: its libuv refuses a Unix socket path longer than macOS's 104 bytes (`listen EINVAL`), and
/// srt puts its proxy socket in the server's scratch TMPDIR, which is longer than that under the macOS data folder.
pub const NODE: Pinned = Pinned {
    title: "Node.js",
    name: "node",
    version: "22.23.4",
    sources: &[
        "https://nodejs.org/dist/v{version}/{file}",
        "https://cdn.npmmirror.com/binaries/node/v{version}/{file}",
    ],
    archives: &[
        Archive {
            os: "windows",
            arch: "x86_64",
            file: "node-v22.23.4-win-x64.zip",
            sha256: "cbc74b5e0982c9dbb41525fd8fb5dcd32fbee237863f7829072f4639a1ce5f20",
        },
        Archive {
            os: "windows",
            arch: "aarch64",
            file: "node-v22.23.4-win-arm64.zip",
            sha256: "a8b8cd79cbbb3062435f449591d98dd63730bbbf30ce2d7cc3a6376c236cb111",
        },
        Archive {
            os: "macos",
            arch: "x86_64",
            file: "node-v22.23.4-darwin-x64.tar.gz",
            sha256: "efcc2b0db6f94bf53eddb038fe2b370277a042bd29e4f19b8f8e23598795b819",
        },
        Archive {
            os: "macos",
            arch: "aarch64",
            file: "node-v22.23.4-darwin-arm64.tar.gz",
            sha256: "c9e1dbc748b33ed2d14f42445191ecaf14c56259c7a9a26d0042c49ec2b66428",
        },
        Archive {
            os: "linux",
            arch: "x86_64",
            file: "node-v22.23.4-linux-x64.tar.gz",
            sha256: "b8492284c76a54baf58d61668dd0e41ee0fc98adabf9503d30f05d08f55b4406",
        },
        Archive {
            os: "linux",
            arch: "aarch64",
            file: "node-v22.23.4-linux-arm64.tar.gz",
            sha256: "f7a1e486dbc8b286847815b477d4fb5e9fb963ed4489c3e1588853d685f589f9",
        },
    ],
    bin_windows: "",
    bin_unix: "bin",
    programs: &["node", "npm", "npx"],
};

/// uv (Astral, MIT/Apache-2.0), for `uvx` servers. 0.13.4, released 2026-10-20; Linux uses the static musl build.
pub const UV: Pinned = Pinned {
    title: "uv",
    name: "uv",
    version: "0.13.4",
    sources: &[
        "https://releases.astral.sh/github/uv/releases/download/{version}/{file}",
        "https://github.com/astral-sh/uv/releases/download/{version}/{file}",
    ],
    archives: &[
        Archive {
            os: "windows",
            arch: "x86_64",
            file: "uv-x86_64-pc-windows-msvc.zip",
            sha256: "ccec7508a07d9933d7cb1261880633efcf1f30d6e6eddab21823f1dae6f23225",
        },
        Archive {
            os: "windows",
            arch: "aarch64",
            file: "uv-aarch64-pc-windows-msvc.zip",
            sha256: "c4bc5a9534b2bb6a12fe29ea748445e1a6f9cb7bb275211c4fb74c52ecf317a5",
        },
        Archive {
            os: "macos",
            arch: "x86_64",
            file: "uv-x86_64-apple-darwin.tar.gz",
            sha256: "7be2dcd316122f4b3e5a0a7889f6248266c14425374e105ff7dc9779184603a6",
        },
        Archive {
            os: "macos",
            arch: "aarch64",
            file: "uv-aarch64-apple-darwin.tar.gz",
            sha256: "e0b1d4743ce64c55d9f0f4a444afa6b81125680070ee04e853fcd0f6c1ebf8f1",
        },
        Archive {
            os: "linux",
            arch: "x86_64",
            file: "uv-x86_64-unknown-linux-musl.tar.gz",
            sha256: "28d4385f9f9d1d1c7654d85b3855e0e2615b2f6cc13136a2edff858bbdc16ff1",
        },
        Archive {
            os: "linux",
            arch: "aarch64",
            file: "uv-aarch64-unknown-linux-musl.tar.gz",
            sha256: "253a17cadcb659ae77a0fe3b9a2f1c14297528d480c421899e61e64d64f24e8f",
        },
    ],
    bin_windows: "",
    bin_unix: "",
    programs: &["uv", "uvx"],
};

/// Every runtime the device provisions, in the order `sandbox setup` does them.
pub const ALL: [&Pinned; 2] = [&NODE, &UV];

/// The largest archive accepted (Node's macOS archive is ~50 MB).
const ARCHIVE_MAX: u64 = 256 << 20;
/// A source that has not delivered [`SLOW_RATE`] bytes per second on average after [`SLOW_AFTER`] is given up for the
/// next one (the last source is never given up for being slow).
const SLOW_AFTER: Duration = Duration::from_secs(30);
const SLOW_RATE: u64 = 128 << 10;
/// Written into an unpacked runtime: the archive digest it came from (a changed pin reinstalls).
const MARKER: &str = ".agentrouter-runtime";

impl Pinned {
    /// This computer's archive.
    pub fn archive(&self) -> Option<&'static Archive> {
        self.archives
            .iter()
            .find(|a| a.os == std::env::consts::OS && a.arch == std::env::consts::ARCH)
    }

    /// `node-22.23.4`: the runtime's folder under the runtimes root.
    pub fn folder(&self) -> String {
        format!("{}-{}", self.name, self.version)
    }

    pub fn dir(&self, root: &Path) -> PathBuf {
        root.join(self.folder())
    }

    pub fn bin_dir(&self, root: &Path) -> PathBuf {
        let sub = if cfg!(windows) {
            self.bin_windows
        } else {
            self.bin_unix
        };
        if sub.is_empty() {
            self.dir(root)
        } else {
            self.dir(root).join(sub)
        }
    }

    /// One of its programs, when installed (`npx` is `npx.cmd` on Windows).
    pub fn program(&self, root: &Path, name: &str) -> Option<PathBuf> {
        if !self.programs.contains(&name) || !self.installed(root) {
            return None;
        }
        program_in(&self.bin_dir(root), name)
    }

    /// Unpacked from the pinned archive (its marker names this archive's digest) and its first program is there.
    pub fn installed(&self, root: &Path) -> bool {
        let Some(archive) = self.archive() else {
            return false;
        };
        let marker = std::fs::read_to_string(self.dir(root).join(MARKER)).unwrap_or_default();
        marker.trim() == archive.sha256
            && program_in(&self.bin_dir(root), self.programs[0]).is_some()
    }

    /// Install the pinned version under `root` when it is not there: download (in order of [`Pinned::sources`]),
    /// check the SHA-256, unpack, put in place, then remove older versions. Returns whether anything was done.
    pub fn ensure(&self, root: &Path) -> Result<bool, String> {
        if self.installed(root) {
            return Ok(false);
        }
        let archive = self.archive().ok_or_else(|| {
            format!(
                "{} 没有这台电脑能用的版本（{} {}）",
                self.title,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        std::fs::create_dir_all(root).map_err(|e| format!("建不了 {}：{e}", root.display()))?;
        let tag = random_tag();
        let file = root.join(format!(".{}.{tag}.download", archive.file));
        let mut failures = Vec::new();
        let mut fetched = false;
        for (i, source) in self.sources.iter().enumerate() {
            let url = source
                .replace("{version}", self.version)
                .replace("{file}", archive.file);
            let last = i + 1 == self.sources.len();
            match download(&url, &file, archive.sha256, !last) {
                Ok(()) => {
                    fetched = true;
                    break;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&file);
                    failures.push(format!("{url}：{e}"));
                }
            }
        }
        if !fetched {
            return Err(format!(
                "{} {} 没下载下来（{}）",
                self.title,
                self.version,
                failures.join("；")
            ));
        }
        let unpacked = root.join(format!(".{}.{tag}.unpack", self.folder()));
        let placed = unpack(&file, archive.file, &unpacked).and_then(|()| {
            let content = single_folder(&unpacked).unwrap_or_else(|| unpacked.clone());
            std::fs::write(content.join(MARKER), archive.sha256)
                .map_err(|e| format!("写不了 {}：{e}", content.display()))?;
            self.put_in_place(root, &content)
        });
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(&unpacked);
        placed?;
        // Older versions of this runtime (never another runtime's folder, never anything we did not create).
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let old_version = name
                    .strip_prefix(&format!("{}-", self.name))
                    .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()));
                if old_version && name != self.folder() {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        Ok(true)
    }

    fn put_in_place(&self, root: &Path, content: &Path) -> Result<(), String> {
        let target = self.dir(root);
        if target.exists() {
            // Another start provisioned it meanwhile, or an earlier attempt left a broken folder of ours.
            if self.installed(root) {
                return Ok(());
            }
            std::fs::remove_dir_all(&target)
                .map_err(|e| format!("清不掉旧的 {}：{e}", target.display()))?;
        }
        match std::fs::rename(content, &target) {
            Ok(()) => Ok(()),
            Err(_) if self.installed(root) => Ok(()),
            Err(e) => Err(format!("放不到 {}：{e}", target.display())),
        }
    }
}

/// `name` in `dir`, with the Windows extensions (`node.exe`, `npx.cmd`).
fn program_in(dir: &Path, name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd"]
    } else {
        &[""]
    };
    exts.iter()
        .map(|e| dir.join(format!("{name}{e}")))
        .find(|p| p.is_file())
}

fn random_tag() -> String {
    let mut b = [0u8; 6];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(30 * 60)));
    // The system's TLS with the system's roots (Windows' store, as every program on the PC): ureq's default with
    // native-tls is Mozilla's list only, and Schannel then rejects nodejs.org's chain ("unable to find any
    // user-specified roots in the final cert chain", hosted Windows runner).
    #[cfg(feature = "native-tls")]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build(),
    );
    #[cfg(all(feature = "rustls", not(feature = "native-tls")))]
    let config = config.tls_config(
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .build(),
    );
    config.build().new_agent()
}

/// Stream `url` into `to` while hashing it; the file stays only when its SHA-256 is `sha256`.
fn download(url: &str, to: &Path, sha256: &str, give_up_when_slow: bool) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("只用 https 下载".into());
    }
    let mut response = agent().get(url).call().map_err(|e| e.to_string())?;
    let mut body = response
        .body_mut()
        .with_config()
        .limit(ARCHIVE_MAX)
        .reader();
    let mut out = std::fs::File::create(to).map_err(|e| format!("写不了 {}：{e}", to.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    let started = Instant::now();
    let mut total: u64 = 0;
    loop {
        let n = body.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("写不了 {}：{e}", to.display()))?;
        total += n as u64;
        let elapsed = started.elapsed();
        if give_up_when_slow && elapsed > SLOW_AFTER && total < SLOW_RATE * elapsed.as_secs() {
            return Err(format!(
                "太慢了（{} 秒只下了 {} KB），换下一个地址",
                elapsed.as_secs(),
                total >> 10
            ));
        }
    }
    out.sync_all().map_err(|e| e.to_string())?;
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != sha256 {
        return Err(format!(
            "校验值对不上（应为 {sha256}，下到的是 {got}），已丢弃"
        ));
    }
    Ok(())
}

/// Unpack a `.zip` or `.tar.gz` into `into` (created). Entries that would land outside it are refused.
fn unpack(archive: &Path, name: &str, into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("建不了 {}：{e}", into.display()))?;
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let failed = |e: &dyn std::fmt::Display| format!("解不开 {name}：{e}");
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(file).map_err(|e| failed(&e))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| failed(&e))?;
            let rel = entry.enclosed_name().ok_or_else(|| {
                failed(&format!(
                    "条目 {} 不在包里",
                    entry.name_raw().escape_ascii()
                ))
            })?;
            let path = into.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&path).map_err(|e| failed(&e))?;
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| failed(&e))?;
            }
            let mut out = std::fs::File::create(&path).map_err(|e| failed(&e))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| failed(&e))?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o755));
            }
        }
        Ok(())
    } else if name.ends_with(".tar.gz") {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        tar.set_preserve_permissions(true);
        tar.set_overwrite(true);
        // `unpack` refuses absolute paths and `..` (each entry goes through `unpack_in`).
        tar.unpack(into).map_err(|e| failed(&e))
    } else {
        Err(failed(&"不认识的格式"))
    }
}

/// The one folder an archive unpacked into (`node-v22.23.4-win-x64/`), when that is all there is.
fn single_folder(dir: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(dir).ok()?.flatten();
    let first = entries.next()?;
    if entries.next().is_some() || !first.file_type().ok()?.is_dir() {
        return None;
    }
    Some(first.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_covers_six_platforms_with_a_well_formed_digest() {
        for rt in ALL {
            assert_eq!(rt.archives.len(), 6, "{}", rt.name);
            for os in ["windows", "macos", "linux"] {
                for arch in ["x86_64", "aarch64"] {
                    let a = rt
                        .archives
                        .iter()
                        .find(|a| a.os == os && a.arch == arch)
                        .unwrap_or_else(|| panic!("{} has no {os} {arch}", rt.name));
                    assert_eq!(a.sha256.len(), 64, "{}", a.file);
                    assert!(
                        a.sha256
                            .bytes()
                            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                    );
                    let ext = if os == "windows" { ".zip" } else { ".tar.gz" };
                    assert!(a.file.ends_with(ext), "{}", a.file);
                }
            }
            for s in rt.sources {
                assert!(s.starts_with("https://") && s.contains("{file}"), "{s}");
            }
            assert!(rt.archive().is_some(), "{} on this runner", rt.name);
        }
        for a in NODE.archives {
            assert!(a.file.starts_with(&format!("node-v{}-", NODE.version)));
        }
    }

    /// Only the projects' own hosts and their well-known mirrors (docs/AV-HYGIENE.md).
    #[test]
    fn downloads_come_from_the_official_hosts_only() {
        let hosts: Vec<&str> = ALL
            .iter()
            .flat_map(|rt| rt.sources.iter())
            .map(|s| s.trim_start_matches("https://").split('/').next().unwrap())
            .collect();
        assert_eq!(
            hosts,
            [
                "nodejs.org",
                "cdn.npmmirror.com",
                "releases.astral.sh",
                "github.com"
            ]
        );
    }

    fn temp(name: &str) -> PathBuf {
        // Under the build's own target folder (never the system temp one, docs/AV-HYGIENE.md).
        let exe = std::env::current_exe().unwrap();
        let target = exe
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap();
        let dir = target
            .join("test-runtimes")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_tar_gz_unpacks_and_its_single_folder_is_found() {
        let dir = temp("tgz");
        let archive = dir.join("a.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut b = tar::Builder::new(gz);
            let data = b"#!/bin/sh\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, "rt-1.0/bin/tool", &data[..]).unwrap();
            b.into_inner().unwrap().finish().unwrap();
        }
        let into = dir.join("out");
        unpack(&archive, "a.tar.gz", &into).unwrap();
        let top = single_folder(&into).unwrap();
        assert!(top.ends_with("rt-1.0"));
        assert!(top.join("bin").join("tool").is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(top.join("bin").join("tool"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_unpacks_and_an_escaping_entry_is_refused() {
        let dir = temp("zip");
        let write = |path: &Path, entries: &[&str]| {
            let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
            for e in entries {
                w.start_file(*e, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        };
        let good = dir.join("good.zip");
        write(&good, &["uv.exe", "uvx.exe"]);
        let into = dir.join("good");
        unpack(&good, "good.zip", &into).unwrap();
        assert!(into.join("uvx.exe").is_file());
        assert!(single_folder(&into).is_none());

        let bad = dir.join("bad.zip");
        write(&bad, &["../escaped.txt"]);
        assert!(unpack(&bad, "bad.zip", &dir.join("bad")).is_err());
        assert!(!dir.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_counts_as_installed_without_the_marker_of_this_pin() {
        let root = temp("marker");
        let bin = NODE.bin_dir(&root);
        std::fs::create_dir_all(&bin).unwrap();
        let exe = if cfg!(windows) { "node.exe" } else { "node" };
        std::fs::write(bin.join(exe), "").unwrap();
        assert!(!NODE.installed(&root));
        std::fs::write(NODE.dir(&root).join(MARKER), "0".repeat(64)).unwrap();
        assert!(!NODE.installed(&root), "another pin's digest");
        std::fs::write(NODE.dir(&root).join(MARKER), NODE.archive().unwrap().sha256).unwrap();
        assert!(NODE.installed(&root));
        assert_eq!(NODE.program(&root, "node"), Some(bin.join(exe)));
        assert_eq!(NODE.program(&root, "uv"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
