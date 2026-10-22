//! Builds the folder of our npm package `runtimes::CARRIER` (device #154): socat's and bubblewrap's pinned archives
//! for both architectures, downloaded from their GitHub releases and checked against the pins in `runtimes.rs`, with
//! the corresponding source (`runtimes::CARRIER_SOURCE`, also checked). `npm pack` that folder; a maintainer publishes
//! the tarball (`npm publish <tgz> --access public`), and npmmirror syncs it, so the device has a China-reachable copy
//! of the same files (`.github/workflows/sandbox-tools.yml`).
//!
//!   cargo run -p agentrouter-sandbox --example sandbox-tools-package -- <empty folder>

use std::path::PathBuf;

use agentrouter_sandbox::runtimes::{self, CARRIER, CARRIER_SOURCE};

fn main() -> Result<(), String> {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("usage: sandbox-tools-package <empty folder>")?,
    );
    std::fs::create_dir_all(out.join("src")).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut files = Vec::new();
    for rt in [&runtimes::SOCAT, &runtimes::BWRAP] {
        for a in rt.archives {
            runtimes::fetch_for_package(rt, a.file, a.sha256, &out.join(a.file))?;
            println!("{}  {}", a.sha256, a.file);
            files.push(a.file.to_string());
        }
    }
    for (rt, file, sha256) in CARRIER_SOURCE {
        runtimes::fetch_for_package(rt, file, sha256, &out.join("src").join(file))?;
        println!("{sha256}  src/{file}");
    }
    files.push("src/".into());
    files.push("README.md".into());
    let (name, version) = CARRIER;
    let package = serde_json::json!({
        "name": name,
        "version": version,
        "description": "Static socat and bubblewrap for Linux as the AgentRouter device pins them (the same archives as its GitHub releases), with their source. The device downloads this tarball itself; do not install it.",
        "license": "(GPL-2.0-only AND LGPL-2.0-or-later)",
        "repository": { "type": "git", "url": "git+https://github.com/Maybank01/agentrouter-device.git" },
        "os": ["linux"],
        "files": files,
    });
    let text = serde_json::to_string_pretty(&package).map_err(|e| e.to_string())?;
    std::fs::write(out.join("package.json"), format!("{text}\n")).map_err(|e| e.to_string())?;
    let socat = runtimes::SOCAT.version;
    let bwrap = runtimes::BWRAP.version;
    std::fs::write(
        out.join("README.md"),
        format!(
            "# {name}\n\nsocat {socat} (GPL-2.0) and bubblewrap {bwrap} (LGPL-2.0-or-later), built statically against musl for \
             Linux x86_64 and aarch64 by [agentrouter-device](https://github.com/Maybank01/agentrouter-device) \
             (`.github/workflows/socat.yml`, `bwrap.yml`). The archives are byte for byte the ones attached to its \
             `socat-static-{socat}` and `bwrap-static-{bwrap}` releases; the device checks each against the SHA-256 \
             pinned in `crates/sandbox/src/runtimes.rs`. `src/` holds the official source tarballs and the build \
             scripts (the corresponding source).\n\nThis package exists so a registry mirror can serve these files \
             where GitHub is slow. Do not install it.\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    println!("{name}@{version} in {}", out.display());
    Ok(())
}
