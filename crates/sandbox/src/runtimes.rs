//! The runtimes local tools run on (device #116, Cloud #2383): folder mode never asks the person to install anything
//! (the product is for ordinary people). srt is a Node program and so are `npx` servers; `uvx` servers (serena, in the
//! default bundle) need uv. On Linux srt also runs ripgrep (it finds the dangerous files to keep unwritable) and socat
//! (it relays the sandbox's network to srt's proxy), #119, and bubblewrap (it makes the namespaces), #122. The device
//! provisions all of them itself, once, in `sandbox setup` (the double-click start and the first `link` run it). Where
//! AppArmor restricts unprivileged user namespaces (Ubuntu 23.10+), the bubblewrap that runs is a root-owned copy of
//! the same pin that one AppArmor profile names (`crate::userns`, one admin line).
//!
//! - **Pinned:** one version per runtime and one archive per OS and architecture, its SHA-256 written here. Node: the
//!   official archive, from the release's `SHASUMS256.txt`, whose signature was checked against the Node.js release keys
//!   (`nodejs/release-keys`) when the pin was set. uv: the release's `.sha256` files (the same on GitHub and on
//!   Astral's mirror), and the SHA-256 PyPI lists for the wheels of the same release ([`UV_WHEELS`]). ripgrep: the VS
//!   Code build (`@vscode/ripgrep-linux-<arch>`, Microsoft, MIT, a static musl build of ripgrep 15.0.0), digest of the
//!   npm tarball (its registry `integrity` checked too when the pin was set). socat and bubblewrap: our own static
//!   builds of the official source (`.github/workflows/socat.yml`, `bwrap.yml`; upstream publishes no maintained
//!   static build), digests from their releases' SHA256SUMS. A file that does not match is deleted, never unpacked.
//! - **Sources (device #154):** every runtime has a source that is fast from mainland China and one that is official.
//!   Measured on a China network 2026-10-22: nodejs.org 17 KB/s and npmmirror 17 MB/s; releases.astral.sh 10 KB/s,
//!   GitHub 8–20 KB/s and the Tsinghua PyPI mirror 11 MB/s; registry.npmjs.org ~17 KB/s. Order: the official host
//!   first (elsewhere it is the fast and authoritative one), given up after [`SLOW_AFTER`] below [`SLOW_RATE`] or when
//!   it does not answer within seconds; then the mirrors; the last source is never given up for being slow. Once an
//!   official host failed or was given up, mirrors go first for every later download (this run and later ones,
//!   [`prefer_mirrors`]): the network is evidently one where they are faster, so each later runtime costs no stall
//!   window. Node: npmmirror's copy of the archive. uv: Astral's PyPI wheels of the same release from the Tsinghua and
//!   Huawei PyPI mirrors ([`Source::Wheel`]). ripgrep: npmmirror's copy of the npm tarball. socat and bubblewrap: our
//!   npm package [`CARRIER`] (the same archives inside; npmmirror syncs it from npm), until it is published only GitHub
//!   serves them (~350 KB). The download is in-process (no curl, PowerShell or installer program) and lands in
//!   `<data>-shared/runtimes/` beside the data folder, never in the temp folder (docs/AV-HYGIENE.md).
//! - **One path:** srt, the Windows sandbox entry, `npm ci`, and `npx`/`uvx` servers use these runtimes, never the
//!   person's own Node or uv (whatever version that is, if any). [`TOOLS`] go first on a server's PATH; srt gets the
//!   absolute paths of [`SANDBOX_LINUX`] in its settings (`ripgrep.command`, `socatPath`, `bwrapPath`), never the
//!   person's own.
//! - **Bumping a pin:** the new version and all its digests (uv: the wheels too; socat, bubblewrap: a new
//!   [`CARRIER`] version), re-vetted (licence, release notes). The new folder is put in place first; the old version's
//!   folder is removed after.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Where the archive comes from, in order (see the module docs; [`prefer_mirrors`] reorders).
    pub sources: &'static [Source],
    pub archives: &'static [Archive],
    /// The folder with the programs, inside the unpacked runtime, per OS family (`""` is the runtime folder itself).
    pub bin_windows: &'static str,
    pub bin_unix: &'static str,
    /// Its programs (without `.exe`/`.cmd`); the first one must exist for the runtime to count as installed.
    pub programs: &'static [&'static str],
    /// Without it nothing confined starts (`sandbox setup` fails); otherwise only the tools that use it are missing.
    pub required: bool,
}

/// The pinned archive for one OS and architecture (`std::env::consts` names).
#[derive(Debug)]
pub struct Archive {
    pub os: &'static str,
    pub arch: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

/// One place a runtime's archive comes from. URLs are `https://` templates: `{version}`, `{file}` and `{npm_arch}`
/// (npm's name of the architecture: `x64`, `arm64`) are filled in.
#[derive(Debug)]
pub enum Source {
    /// The pinned archive itself. `mirror`: a copy someone else serves (the same file, so the same digest).
    Archive { url: &'static str, mirror: bool },
    /// The project's own PyPI wheel of the same build (uv publishes its binaries there too); `{path}` is the wheel's
    /// [`Wheel::path`]. The wheel's own digest is checked, and its `<name>.data/scripts/` folder is the runtime.
    Wheel {
        url: &'static str,
        wheels: &'static [Wheel],
        mirror: bool,
    },
    /// The pinned archive inside our npm package [`CARRIER`] (`package/<file>`), from this registry: our own builds
    /// that have no public mirror (socat, bubblewrap). The archive's digest is checked as from any other source.
    Carrier { registry: &'static str },
}

/// A PyPI wheel of a pinned runtime, per OS and architecture.
#[derive(Debug)]
pub struct Wheel {
    pub os: &'static str,
    pub arch: &'static str,
    /// Its path under a PyPI file host's `/packages/` (the same on PyPI and its mirrors).
    pub path: &'static str,
    pub sha256: &'static str,
}

/// Our npm package that carries the socat and bubblewrap archives (both architectures) and their corresponding
/// source, so a registry mirror (npmmirror) serves them where GitHub is slow. Built from these pins by
/// `cargo run -p agentrouter-sandbox --example sandbox-tools-package` (`.github/workflows/sandbox-tools.yml`) and
/// published to npm by a maintainer; a new socat or bubblewrap pin needs a new version here.
pub const CARRIER: (&str, &str) = ("@agentrouter-top/sandbox-tools-linux", "2026.10.22");

const NPM_REGISTRY: &str = "https://registry.npmjs.org";
/// npmmirror (Alibaba Cloud), the npm registry mirror for mainland China.
pub const NPM_MIRROR: &str = "https://registry.npmmirror.com";

/// Node.js 22 LTS ("Jod", maintained until 2027-04; srt needs 22.12+). 22.23.4 is the 2026-10-20 security release;
/// SHASUMS256.txt signed by release key 5BE8A3F6C8A5C01D106C0AD820B1A390B168D356 (Antoine du Hamel), verified
/// 2026-10-21. Not 24 yet: its libuv refuses a Unix socket path longer than macOS's 104 bytes (`listen EINVAL`), and
/// srt puts its proxy socket in the server's scratch TMPDIR, which is longer than that under the macOS data folder.
pub const NODE: Pinned = Pinned {
    title: "Node.js",
    name: "node",
    version: "22.23.4",
    sources: &[
        Source::Archive {
            url: "https://nodejs.org/dist/v{version}/{file}",
            mirror: false,
        },
        Source::Archive {
            url: "https://cdn.npmmirror.com/binaries/node/v{version}/{file}",
            mirror: true,
        },
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
    required: true,
};

/// uv 0.13.4's wheels on PyPI (the same build as the archives; `pypi.org/pypi/uv/0.13.4/json`, 2026-10-22). Linux:
/// the musllinux wheels, whose programs are the same static musl builds as the archives.
pub const UV_WHEELS: &[Wheel] = &[
    Wheel {
        os: "windows",
        arch: "x86_64",
        path: "2f/b9/b526eab6c72eacc705a58378f6850fde046b28c86fed01bd42da61367c6b/uv-0.13.4-py3-none-win_amd64.whl",
        sha256: "b2ff80397273fff0e02c20eacae6882834c811827d6fc08f658db156fe754386",
    },
    Wheel {
        os: "windows",
        arch: "aarch64",
        path: "7c/f7/4d5fa89d3d10ff6a84e547ef6e02c45b2f2d50e66682da778acf60db63f1/uv-0.13.4-py3-none-win_arm64.whl",
        sha256: "deffa5a4afdb48f3e44fb26e80377d256b330ddcad0373daff4b54f6162b43a4",
    },
    Wheel {
        os: "macos",
        arch: "x86_64",
        path: "21/f2/0cb77a1ae43959772f8f7873eb1f4061907fbb1694ce55c79b231f639b1d/uv-0.13.4-py3-none-macosx_10_12_x86_64.whl",
        sha256: "b4bbacea78c9db136cd2ec3147cbfd9ed910b4db5e1877109bd74d0e73bb2eaf",
    },
    Wheel {
        os: "macos",
        arch: "aarch64",
        path: "26/44/5a8733dd1b2e8d6f9cffd1ff6428a3e11dcc7e146a7657e32cfb8d9fb4a4/uv-0.13.4-py3-none-macosx_11_0_arm64.whl",
        sha256: "95f4eb2b991b53b50ec2b18ab612fca8ba1e7a75a47af0331fb8bcd21da3eb03",
    },
    Wheel {
        os: "linux",
        arch: "x86_64",
        path: "1e/ec/f352ca2cc3b4b6191bb4ec8cdf4bc190eed0ba8bda4c33b7fd66c5d33cec/uv-0.13.4-py3-none-musllinux_1_1_x86_64.whl",
        sha256: "52b5c753a7c2d5f8323a70bb4c93118988356331d0758afa6446c81f974e7196",
    },
    Wheel {
        os: "linux",
        arch: "aarch64",
        path: "f8/39/be14f051bf34a1822d634819890a2d0cba3b69101e633fb7b9f54d7b4020/uv-0.13.4-py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.musllinux_1_1_aarch64.whl",
        sha256: "e90ab42825587821096cb0d071e387468da38cfa22818c91f9c0b3c0453048e8",
    },
];

/// uv (Astral, MIT/Apache-2.0), for `uvx` servers. 0.13.4, released 2026-10-20; Linux uses the static musl build.
pub const UV: Pinned = Pinned {
    title: "uv",
    name: "uv",
    version: "0.13.4",
    sources: &[
        Source::Archive {
            url: "https://releases.astral.sh/github/uv/releases/download/{version}/{file}",
            mirror: false,
        },
        Source::Wheel {
            url: "https://pypi.tuna.tsinghua.edu.cn/packages/{path}",
            wheels: UV_WHEELS,
            mirror: true,
        },
        Source::Wheel {
            url: "https://repo.huaweicloud.com/repository/pypi/packages/{path}",
            wheels: UV_WHEELS,
            mirror: true,
        },
        Source::Archive {
            url: "https://github.com/astral-sh/uv/releases/download/{version}/{file}",
            mirror: false,
        },
        Source::Wheel {
            url: "https://files.pythonhosted.org/packages/{path}",
            wheels: UV_WHEELS,
            mirror: false,
        },
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
    required: false,
};

/// ripgrep (MIT/Unlicense), srt's scan for dangerous files below a folder on Linux: the VS Code build,
/// `@vscode/ripgrep-linux-<arch>` 1.18.0 (Microsoft, MIT, 2026-05-07; ripgrep 15.0.0, static musl), an npm tarball
/// whose registry `integrity` matched on npmjs and npmmirror (2026-10-22). Not BurntSushi's GitHub archive any more
/// (#154): GitHub has no mirror in China, the npm registry does. The version is the npm package's.
pub const RG: Pinned = Pinned {
    title: "ripgrep",
    name: "rg",
    version: "1.18.0",
    sources: &[
        Source::Archive {
            url: "https://registry.npmjs.org/@vscode/ripgrep-linux-{npm_arch}/-/{file}",
            mirror: false,
        },
        Source::Archive {
            url: "https://registry.npmmirror.com/@vscode/ripgrep-linux-{npm_arch}/-/{file}",
            mirror: true,
        },
    ],
    archives: &[
        Archive {
            os: "linux",
            arch: "x86_64",
            file: "ripgrep-linux-x64-1.18.0.tgz",
            sha256: "8cc06e25e29f9c258ee9a09eecd7707c1ea2e99d9e76151e53dbe39f8d6b4a09",
        },
        Archive {
            os: "linux",
            arch: "aarch64",
            file: "ripgrep-linux-arm64-1.18.0.tgz",
            sha256: "2d65504a71ea421d1c457177ebefcbe0d2d3a1f60f9709b6337f1d933553064b",
        },
    ],
    bin_windows: "",
    bin_unix: "bin",
    programs: &["rg"],
    required: true,
};

/// socat (GPL-2.0), srt's network relay on Linux: on the host from a Unix socket to srt's proxy, inside bubblewrap
/// from TCP 3128/1080 to that socket. 1.8.1.3 built statically against musl by `.github/workflows/socat.yml` (release
/// `socat-static-<version>`, with its source); `-r1` is the build revision.
pub const SOCAT: Pinned = Pinned {
    title: "socat",
    name: "socat",
    version: "1.8.1.3-r1",
    sources: &[
        Source::Archive {
            url: "https://github.com/Maybank01/agentrouter-device/releases/download/socat-static-{version}/{file}",
            mirror: false,
        },
        Source::Carrier {
            registry: NPM_MIRROR,
        },
        Source::Carrier {
            registry: NPM_REGISTRY,
        },
    ],
    archives: &[
        Archive {
            os: "linux",
            arch: "x86_64",
            file: "socat-1.8.1.3-x86_64-linux-musl.tar.gz",
            sha256: "ba3cfa959b4adc7a1fe630cefa501e656edb0a89450e24dddd64422097522f44",
        },
        Archive {
            os: "linux",
            arch: "aarch64",
            file: "socat-1.8.1.3-aarch64-linux-musl.tar.gz",
            sha256: "3622ec76b7d1ec9fbeeb9aed5a6fe74cf476a1c95512f0655b6cb7f4fb90ff81",
        },
    ],
    bin_windows: "",
    bin_unix: "",
    programs: &["socat"],
    required: true,
};

/// bubblewrap (LGPL-2.0-or-later), what srt confines with on Linux. 0.13.1, released 2026-10-14, built statically
/// against musl with libcap by `.github/workflows/bwrap.yml` (release `bwrap-static-<version>`, with its source); `-r1`
/// is the build revision. The same pin is the root-owned copy `sandbox userns install` puts under
/// `/usr/local/libexec/agentrouter` (`crate::userns`).
pub const BWRAP: Pinned = Pinned {
    title: "bubblewrap",
    name: "bwrap",
    version: "0.13.1-r1",
    sources: &[
        Source::Archive {
            url: "https://github.com/Maybank01/agentrouter-device/releases/download/bwrap-static-{version}/{file}",
            mirror: false,
        },
        Source::Carrier {
            registry: NPM_MIRROR,
        },
        Source::Carrier {
            registry: NPM_REGISTRY,
        },
    ],
    archives: &[
        Archive {
            os: "linux",
            arch: "x86_64",
            file: "bwrap-0.13.1-x86_64-linux-musl.tar.gz",
            sha256: "e74b6609a092314dab5309b3e20ef7f64d6bf55e8df5a66bf44d1ac968d2fbc4",
        },
        Archive {
            os: "linux",
            arch: "aarch64",
            file: "bwrap-0.13.1-aarch64-linux-musl.tar.gz",
            sha256: "f6962376ff55c74f55ead5686f4a96d4fd3280bc007ec2dedb9a61609f19c79d",
        },
    ],
    bin_windows: "",
    bin_unix: "",
    programs: &["bwrap"],
    required: true,
};

/// The corresponding source [`CARRIER`] ships beside socat's and bubblewrap's archives (GPL-2.0 / LGPL): the official
/// source tarballs and our build scripts, as attached to their GitHub releases (`SHA256SUMS` there), each with its
/// digest. `(runtime, file, sha256)`.
pub const CARRIER_SOURCE: &[(&Pinned, &str, &str)] = &[
    (
        &SOCAT,
        "socat-1.8.1.3.tar.gz",
        "06602ffd591e98c75b3dc1d66f0f19136cc666b0b2d95caad987d6ab2cb28097",
    ),
    (
        &SOCAT,
        "build-socat-static.sh",
        "f4f5fdbe7d9c3fd9b13af4f3bbbaf98a229f9b6cbaa212aaea5ddbe1b1a68be7",
    ),
    (
        &BWRAP,
        "bubblewrap-0.13.1.tar.xz",
        "0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d",
    ),
    (
        &BWRAP,
        "build-bwrap-static.sh",
        "95e5cfb71965d8604e80bccbb89e7dabcec3d3d53383204f026c34e2ad0cc25d",
    ),
];

/// What local tools run on, on every OS (first on their PATH), in the order `sandbox setup` provisions them.
pub const TOOLS: [&Pinned; 2] = [&NODE, &UV];

/// What srt itself runs on Linux besides Node (it gets their absolute paths).
pub const SANDBOX_LINUX: [&Pinned; 3] = [&RG, &SOCAT, &BWRAP];

/// What srt runs on this OS besides Node: [`SANDBOX_LINUX`] on Linux, nothing elsewhere.
pub fn sandbox_programs() -> &'static [&'static Pinned] {
    if cfg!(target_os = "linux") {
        &SANDBOX_LINUX
    } else {
        &[]
    }
}

/// The largest download accepted (Node's macOS archive is ~50 MB).
const ARCHIVE_MAX: u64 = 256 << 20;
/// A source that has not delivered [`SLOW_RATE`] bytes per second on average after [`SLOW_AFTER`] is given up for the
/// next one (the last source is never given up for being slow). 8 s at 256 KiB/s: an official host from mainland China
/// (10–20 KB/s) is left within seconds, a working one elsewhere (MB/s) is never.
const SLOW_AFTER: Duration = Duration::from_secs(8);
const SLOW_RATE: u64 = 256 << 10;
/// Written into an unpacked runtime: the archive digest of its pin (a changed pin reinstalls), whichever source it
/// came from.
const MARKER: &str = ".agentrouter-runtime";
/// In the runtimes root once an official host proved slow on this computer ([`prefer_mirrors`]).
const PREFER_MIRRORS_FILE: &str = ".prefer-mirrors";

static PREFER_MIRRORS: AtomicBool = AtomicBool::new(false);

/// Whether mirrors go first on this computer: an official host failed or was given up for being slow, in this run or
/// an earlier one (remembered in the runtimes root). `npm ci` of srt uses the npm mirror then too (`crate::Runtime`).
pub fn prefer_mirrors(root: &Path) -> bool {
    PREFER_MIRRORS.load(Ordering::Relaxed) || root.join(PREFER_MIRRORS_FILE).is_file()
}

/// Remember that mirrors go first (best effort: without the file only this run knows).
pub fn set_prefer_mirrors(root: &Path) {
    PREFER_MIRRORS.store(true, Ordering::Relaxed);
    let _ = std::fs::write(
        root.join(PREFER_MIRRORS_FILE),
        "An official download host was slow or unreachable here; mirrors go first (agentrouter-device #154).\n",
    );
}

/// npm's name of this computer's architecture (`x64`, `arm64`).
fn npm_arch(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => "unknown",
    }
}

/// One resolved download: the URL, what it serves, and the digest it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub url: String,
    pub form: Form,
    pub mirror: bool,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// The pinned archive.
    Archive,
    /// A PyPI wheel (a zip with `<name>.data/scripts/`).
    Wheel,
    /// The pinned archive as `package/<file>` inside an npm tarball.
    Member(String),
}

impl Attempt {
    /// The host it downloads from (shown to the person).
    pub fn host(&self) -> &str {
        self.url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or("")
    }
}

impl Pinned {
    /// This computer's archive.
    pub fn archive(&self) -> Option<&'static Archive> {
        self.archive_for(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn archive_for(&self, os: &str, arch: &str) -> Option<&'static Archive> {
        self.archives.iter().find(|a| a.os == os && a.arch == arch)
    }

    /// The downloads for one archive, in the order of [`Pinned::sources`]; mirrors first when `mirrors_first`.
    pub fn attempts(&self, archive: &Archive, mirrors_first: bool) -> Vec<Attempt> {
        let fill = |url: &str| {
            url.replace("{version}", self.version)
                .replace("{file}", archive.file)
                .replace("{npm_arch}", npm_arch(archive.arch))
        };
        let mut out: Vec<Attempt> = self
            .sources
            .iter()
            .filter_map(|source| match source {
                Source::Archive { url, mirror } => Some(Attempt {
                    url: fill(url),
                    form: Form::Archive,
                    mirror: *mirror,
                    sha256: archive.sha256,
                }),
                Source::Wheel {
                    url,
                    wheels,
                    mirror,
                } => wheels
                    .iter()
                    .find(|w| w.os == archive.os && w.arch == archive.arch)
                    .map(|w| Attempt {
                        url: url.replace("{path}", w.path),
                        form: Form::Wheel,
                        mirror: *mirror,
                        sha256: w.sha256,
                    }),
                Source::Carrier { registry } => {
                    let (package, version) = CARRIER;
                    let short = package.rsplit('/').next().unwrap_or(package);
                    Some(Attempt {
                        url: format!("{registry}/{package}/-/{short}-{version}.tgz"),
                        form: Form::Member(archive.file.to_string()),
                        mirror: true,
                        sha256: archive.sha256,
                    })
                }
            })
            .collect();
        if mirrors_first {
            // Stable: mirrors in their order, then the official hosts in theirs.
            out.sort_by_key(|a| !a.mirror);
        }
        out
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

    /// Install the pinned version under `root` when it is not there: download (in the order of [`Pinned::attempts`]),
    /// check the SHA-256, unpack, put in place, then remove older versions. Returns the host it came from when anything
    /// was done.
    pub fn ensure(&self, root: &Path) -> Result<Option<String>, String> {
        if self.installed(root) {
            return Ok(None);
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
        let attempts = self.attempts(archive, prefer_mirrors(root));
        let mut failures = Vec::new();
        let mut fetched = None;
        for (i, attempt) in attempts.iter().enumerate() {
            let last = i + 1 == attempts.len();
            match download(attempt, &file, !last) {
                Ok(()) => {
                    fetched = Some(attempt);
                    break;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&file);
                    if !attempt.mirror {
                        set_prefer_mirrors(root);
                    }
                    failures.push(format!("{}：{e}", attempt.url));
                }
            }
        }
        let Some(attempt) = fetched else {
            return Err(format!(
                "{} {}（{}）从 {} 个地址都没下载下来，网络通了以后运行 agentrouter sandbox setup 会再试（{}）",
                self.title,
                self.version,
                archive.file,
                attempts.len(),
                failures.join("；")
            ));
        };
        let unpacked = root.join(format!(".{}.{tag}.unpack", self.folder()));
        let format = match attempt.form {
            Form::Wheel => "wheel.whl",
            _ => archive.file,
        };
        let placed = unpack(&file, format, &unpacked).and_then(|()| {
            let content = match attempt.form {
                Form::Wheel => wheel_scripts(&unpacked).ok_or_else(|| {
                    format!(
                        "{} 的 wheel（{}）里没有 *.data/scripts",
                        self.title, attempt.url
                    )
                })?,
                _ => single_folder(&unpacked).unwrap_or_else(|| unpacked.clone()),
            };
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
        Ok(Some(attempt.host().to_string()))
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

/// `quick`: a source that is not the last one must answer within seconds (the next one is waiting).
fn agent(quick: bool) -> ureq::Agent {
    let (connect, response) = if quick { (5, 10) } else { (15, 30) };
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(connect)))
        .timeout_recv_response(Some(Duration::from_secs(response)))
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

/// A response body that fails once it has been too slow ([`SLOW_AFTER`], [`SLOW_RATE`]) when `give_up` is set.
struct Metered<R> {
    inner: R,
    started: Instant,
    total: u64,
    give_up: bool,
}

impl<R: Read> Read for Metered<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        let elapsed = self.started.elapsed();
        if self.give_up && elapsed > SLOW_AFTER && self.total < SLOW_RATE * elapsed.as_secs() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "太慢了（{} 秒只下了 {} KB），换下一个地址",
                    elapsed.as_secs(),
                    self.total >> 10
                ),
            ));
        }
        Ok(n)
    }
}

/// Copy `from` into `out` while hashing it.
fn copy_hashing(
    from: &mut impl Read,
    out: &mut std::fs::File,
    to: &Path,
) -> Result<String, String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = from.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|e| format!("写不了 {}：{e}", to.display()))?;
    }
    out.sync_all().map_err(|e| e.to_string())?;
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Stream one attempt into `to` while hashing it; the file stays only when its SHA-256 is the attempt's.
pub fn download(attempt: &Attempt, to: &Path, give_up_when_slow: bool) -> Result<(), String> {
    if !attempt.url.starts_with("https://") {
        return Err("只用 https 下载".into());
    }
    let mut response = agent(give_up_when_slow)
        .get(&attempt.url)
        .call()
        .map_err(|e| e.to_string())?;
    let body = response
        .body_mut()
        .with_config()
        .limit(ARCHIVE_MAX)
        .reader();
    let mut body = Metered {
        inner: body,
        started: Instant::now(),
        total: 0,
        give_up: give_up_when_slow,
    };
    let mut out = std::fs::File::create(to).map_err(|e| format!("写不了 {}：{e}", to.display()))?;
    let got = match &attempt.form {
        Form::Member(file) => {
            let want = Path::new("package").join(file);
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(body));
            let mut got = None;
            for entry in tar.entries().map_err(|e| e.to_string())? {
                let mut entry = entry.map_err(|e| e.to_string())?;
                if entry.path().map_err(|e| e.to_string())? == want {
                    got = Some(copy_hashing(&mut entry, &mut out, to)?);
                    break;
                }
            }
            got.ok_or_else(|| format!("包里没有 package/{file}"))?
        }
        Form::Archive | Form::Wheel => copy_hashing(&mut body, &mut out, to)?,
    };
    if got != attempt.sha256 {
        drop(out);
        let _ = std::fs::remove_file(to);
        return Err(format!(
            "校验值对不上（应为 {}，下到的是 {got}），已丢弃",
            attempt.sha256
        ));
    }
    Ok(())
}

/// Unpack a `.zip`/`.whl` or `.tar.gz`/`.tgz` into `into` (created). Entries that would land outside it are refused.
fn unpack(archive: &Path, name: &str, into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("建不了 {}：{e}", into.display()))?;
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let failed = |e: &dyn std::fmt::Display| format!("解不开 {name}：{e}");
    if name.ends_with(".zip") || name.ends_with(".whl") {
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
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
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

/// A wheel's `<name>-<version>.data/scripts/` folder (where its programs are).
fn wheel_scripts(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with(".data"))
        .map(|e| e.path().join("scripts"))
        .filter(|p| p.is_dir())
}

/// Fetch one pinned file for packaging (the `sandbox-tools-package` example): from the non-carrier sources of `rt`, in
/// order, checked against `sha256`; `file` replaces `{file}`.
pub fn fetch_for_package(
    rt: &Pinned,
    file: &str,
    sha256: &'static str,
    to: &Path,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for source in rt.sources {
        let Source::Archive { url, mirror } = source else {
            continue;
        };
        let attempt = Attempt {
            url: url.replace("{version}", rt.version).replace("{file}", file),
            form: Form::Archive,
            mirror: *mirror,
            sha256,
        };
        match download(&attempt, to, false) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(to);
                failures.push(format!("{}：{e}", attempt.url));
            }
        }
    }
    Err(format!("{file}: {}", failures.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex64(digest: &str, what: &str) {
        assert_eq!(digest.len(), 64, "{what}");
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "{what}"
        );
    }

    fn well_formed(rt: &Pinned, a: &Archive) {
        hex64(a.sha256, a.file);
        let ok = if a.os == "windows" {
            a.file.ends_with(".zip")
        } else {
            a.file.ends_with(".tar.gz") || a.file.ends_with(".tgz")
        };
        assert!(ok, "{}", a.file);
        for s in rt.sources {
            match s {
                Source::Archive { url, .. } => {
                    assert!(
                        url.starts_with("https://") && url.contains("{file}"),
                        "{url}"
                    )
                }
                Source::Wheel { url, wheels, .. } => {
                    assert!(
                        url.starts_with("https://") && url.contains("{path}"),
                        "{url}"
                    );
                    let w = wheels
                        .iter()
                        .find(|w| w.os == a.os && w.arch == a.arch)
                        .unwrap_or_else(|| panic!("{} has no wheel for {}", rt.name, a.file));
                    hex64(w.sha256, w.path);
                    assert!(w.path.ends_with(".whl"), "{}", w.path);
                    assert!(
                        w.path.contains(&format!("/{}-{}-", rt.name, rt.version)),
                        "{}",
                        w.path
                    );
                }
                Source::Carrier { registry } => assert!(registry.starts_with("https://")),
            }
        }
    }

    #[test]
    fn every_pin_covers_its_platforms_with_a_well_formed_digest() {
        for rt in TOOLS {
            assert_eq!(rt.archives.len(), 6, "{}", rt.name);
            for os in ["windows", "macos", "linux"] {
                for arch in ["x86_64", "aarch64"] {
                    let a = rt
                        .archive_for(os, arch)
                        .unwrap_or_else(|| panic!("{} has no {os} {arch}", rt.name));
                    well_formed(rt, a);
                }
            }
            assert!(rt.archive().is_some(), "{} on this runner", rt.name);
        }
        for rt in SANDBOX_LINUX {
            assert_eq!(rt.archives.len(), 2, "{}", rt.name);
            for arch in ["x86_64", "aarch64"] {
                let a = rt
                    .archive_for("linux", arch)
                    .unwrap_or_else(|| panic!("{} has no linux {arch}", rt.name));
                well_formed(rt, a);
                assert!(
                    a.file.contains(arch) || a.file.contains(&format!("-{}-", npm_arch(arch))),
                    "{}",
                    a.file
                );
            }
            assert_eq!(
                rt.archive().is_some(),
                cfg!(target_os = "linux"),
                "{}",
                rt.name
            );
            assert!(rt.required, "{}: srt cannot confine without it", rt.name);
        }
        assert_eq!(sandbox_programs().is_empty(), !cfg!(target_os = "linux"));
        for a in NODE.archives {
            assert!(a.file.starts_with(&format!("node-v{}-", NODE.version)));
        }
        for a in RG.archives {
            assert_eq!(
                a.file,
                format!("ripgrep-linux-{}-{}.tgz", npm_arch(a.arch), RG.version)
            );
        }
        let socat_release = SOCAT.version.split("-r").next().unwrap();
        for a in SOCAT.archives {
            assert!(a.file.starts_with(&format!("socat-{socat_release}-")));
        }
        let bwrap_release = BWRAP.version.split("-r").next().unwrap();
        for a in BWRAP.archives {
            assert!(a.file.starts_with(&format!("bwrap-{bwrap_release}-")));
        }
        for (rt, file, sha256) in CARRIER_SOURCE {
            hex64(sha256, file);
            assert!(SANDBOX_LINUX.iter().any(|r| r.name == rt.name), "{file}");
        }
    }

    fn hosts(rt: &Pinned, mirrors_first: bool) -> Vec<String> {
        let a = rt.archive_for("linux", "x86_64").unwrap();
        rt.attempts(a, mirrors_first)
            .iter()
            .map(|a| a.host().to_string())
            .collect()
    }

    /// Only the projects' own hosts, their registries and the well-known mirrors of those (docs/AV-HYGIENE.md); the
    /// official host first, unless mirrors go first on this computer (#154).
    #[test]
    fn downloads_come_from_official_hosts_and_their_mirrors_only() {
        assert_eq!(hosts(&NODE, false), ["nodejs.org", "cdn.npmmirror.com"]);
        assert_eq!(hosts(&NODE, true), ["cdn.npmmirror.com", "nodejs.org"]);
        assert_eq!(
            hosts(&UV, false),
            [
                "releases.astral.sh",
                "pypi.tuna.tsinghua.edu.cn",
                "repo.huaweicloud.com",
                "github.com",
                "files.pythonhosted.org"
            ]
        );
        assert_eq!(
            hosts(&UV, true),
            [
                "pypi.tuna.tsinghua.edu.cn",
                "repo.huaweicloud.com",
                "releases.astral.sh",
                "github.com",
                "files.pythonhosted.org"
            ]
        );
        assert_eq!(
            hosts(&RG, false),
            ["registry.npmjs.org", "registry.npmmirror.com"]
        );
        for rt in [&SOCAT, &BWRAP] {
            assert_eq!(
                hosts(rt, false),
                ["github.com", "registry.npmmirror.com", "registry.npmjs.org"]
            );
            assert_eq!(
                hosts(rt, true),
                ["registry.npmmirror.com", "registry.npmjs.org", "github.com"]
            );
        }
        // The static socat and bubblewrap builds are ours, beside their source; the npm package carries the same files.
        let a = SOCAT.archive_for("linux", "aarch64").unwrap();
        let tries = SOCAT.attempts(a, false);
        assert_eq!(
            tries[0].url,
            "https://github.com/Maybank01/agentrouter-device/releases/download/socat-static-1.8.1.3-r1/socat-1.8.1.3-aarch64-linux-musl.tar.gz"
        );
        assert_eq!(
            tries[1].url,
            format!(
                "https://registry.npmmirror.com/@agentrouter-top/sandbox-tools-linux/-/sandbox-tools-linux-{}.tgz",
                CARRIER.1
            )
        );
        assert_eq!(tries[1].form, Form::Member(a.file.to_string()));
        assert!(tries.iter().all(|t| t.sha256 == a.sha256));
        let a = RG.archive_for("linux", "aarch64").unwrap();
        assert_eq!(
            RG.attempts(a, true)[0].url,
            "https://registry.npmmirror.com/@vscode/ripgrep-linux-arm64/-/ripgrep-linux-arm64-1.18.0.tgz"
        );
        // uv's wheels: their own digests, the same path on PyPI and its mirrors.
        let a = UV.archive_for("windows", "aarch64").unwrap();
        let tries = UV.attempts(a, false);
        assert_eq!(tries[0].form, Form::Archive);
        assert_eq!(tries[0].sha256, a.sha256);
        assert_eq!(tries[1].form, Form::Wheel);
        assert_eq!(
            tries[1].url,
            "https://pypi.tuna.tsinghua.edu.cn/packages/7c/f7/4d5fa89d3d10ff6a84e547ef6e02c45b2f2d50e66682da778acf60db63f1/uv-0.13.4-py3-none-win_arm64.whl"
        );
        assert_eq!(
            tries[1].sha256,
            "deffa5a4afdb48f3e44fb26e80377d256b330ddcad0373daff4b54f6162b43a4"
        );
        assert!(!tries[4].mirror && tries[4].form == Form::Wheel);
    }

    #[test]
    fn mirrors_go_first_once_remembered() {
        let root = temp("prefer");
        assert!(!root.join(PREFER_MIRRORS_FILE).exists());
        set_prefer_mirrors(&root);
        assert!(prefer_mirrors(&root));
        assert!(root.join(PREFER_MIRRORS_FILE).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_wheel_unpacks_to_its_scripts_folder() {
        let dir = temp("whl");
        let whl = dir.join("uv.whl");
        {
            let mut w = zip::ZipWriter::new(std::fs::File::create(&whl).unwrap());
            let exe = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            for e in ["uv/__init__.py", "uv-1.0.dist-info/RECORD"] {
                w.start_file(e, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(b"x").unwrap();
            }
            for e in ["uv-1.0.data/scripts/uv", "uv-1.0.data/scripts/uvx"] {
                w.start_file(e, exe).unwrap();
                w.write_all(b"#!/bin/sh\n").unwrap();
            }
            w.finish().unwrap();
        }
        let into = dir.join("out");
        unpack(&whl, "wheel.whl", &into).unwrap();
        let scripts = wheel_scripts(&into).unwrap();
        assert!(scripts.ends_with("uv-1.0.data/scripts"));
        assert!(program_in(&scripts, "uvx").is_some() || cfg!(windows));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(scripts.join("uv"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        let _ = std::fs::remove_dir_all(&dir);
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
