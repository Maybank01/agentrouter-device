//! Windows: embed the version resource (publisher metadata) and the application manifest
//! (asInvoker, UTF-8 code page, per-monitor DPI, common controls v6).

fn main() {
    println!("cargo:rerun-if-changed=res/app.manifest");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version
        .split(['.', '-', '+'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    parts.resize(3, 0);
    let (a, b, c) = (parts[0], parts[1], parts[2]);
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("res")
        .join("app.manifest");
    let manifest = manifest.display().to_string().replace(char::from(92), "/");
    let rc = format!(
        r#"#pragma code_page(65001)
#include <winver.h>
1 24 "{manifest}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080404b0"
    BEGIN
      VALUE "CompanyName", "AgentRouter"
      VALUE "FileDescription", "AgentRouter 设备（已链接的设备）"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "agentrouter-device"
      VALUE "LegalCopyright", "Copyright 2026 AgentRouter contributors. Apache-2.0."
      VALUE "OriginalFilename", "agentrouter-device.exe"
      VALUE "ProductName", "AgentRouter 设备"
      VALUE "ProductVersion", "{version}"
      VALUE "Comments", "Open source: https://github.com/Maybank01/agentrouter-device"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x804, 1200
  END
END
"#
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.rc");
    std::fs::write(&out, rc).unwrap();
    let result = embed_resource::compile(&out, embed_resource::NONE);
    // CI (and every release build there) must carry the resource; a local machine without a
    // resource compiler still builds, with a warning.
    if std::env::var_os("CI").is_some() {
        result.manifest_required().unwrap();
    } else if let Err(e) = result.manifest_optional() {
        println!("cargo:warning=version resource not embedded (no resource compiler): {e}");
    }
}
