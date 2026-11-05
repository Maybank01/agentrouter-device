# release: the client's releases for people

**Purpose**: versioned releases of `agentdevice` with a per-user Windows installer (#243, Cloud ADR 0021 §20.2),
separate from `ci.yml`'s pre-releases `client-v0.0.0-<sha7>` (Dev testing, compute/v1 sandboxes).
**Cut one**: `gh workflow run release.yml -f version=X.Y.Z` (main's head, its required checks green; newer than the
latest). Tag `client-vX.Y.Z`, the newest release, so `releases/latest/download/<asset>` is a stable link (Cloud 我的设备).
**Assets**: `agentdevice-setup[-<env>].exe`, `agentdevice-windows-amd64.exe`, static `agentdevice-linux-{amd64,arm64}`,
`agentdevice-linux-{amd64,arm64}[-<env>].tar.gz` (program + `environment.json`), `environment-<env>.json`, `SHA256SUMS`;
the notes name the commit, the environments built and missing, the sums and the changes since the last release.

| Submodule | Current state |
| --- | --- |
| `environments/` | one file per environment, read by the client as `environment.json` beside the program (`config.json` in the client's folder wins key by key). One build serves all; the release builds an installer per **complete** file. `dev.json` complete (also the pre-release's built-in values); `production.json` **missing** until runbook P5 names the STS and compute/v1 and step 2d the `agentdevice` app ids |
| `windows/agentdevice.iss` | Inno Setup 6 (preinstalled on `windows-latest`; `ChineseSimplified.isl` vendored from issrc `is-6_7_1`) |

**Installer** (`agentdevice-setup-dev.exe`; production's will be `agentdevice-setup.exe`): no administrator rights
(`PrivilegesRequired=lowest`): `%LOCALAPPDATA%\Programs\AgentDevice`, uninstall entry under HKCU, Start menu
「AgentRouter 设备」. Ticked by default: 「登录电脑后自动启动」 = `agentdevice autostart on --no-start` (the Task Scheduler
logon task, no Run key). Then it starts the client: the resident window 「AgentRouter 设备」, the browser asks to confirm
the login. A newer installer runs `agentdevice stop`, replaces the program in place and keeps the login and the device.
**Uninstall** (Settings › Apps, or `unins000.exe`) runs `agentdevice uninstall`: autostart off, the client ended (by its
path, with what it started), the computer removed from the account, `%APPDATA%\AgentDevice` deleted; then the files.
Result: `%TEMP%\agentdevice-uninstall.txt`. Offline, the computer stays in 我的设备 to be removed there.

**Unsigned** (code signing is the owner's decision, [ADR 0001](../docs/adr/0001-code-signing.md)). SmartScreen shows
「Windows 已保护你的电脑」 for the downloaded installer: 「更多信息」 → 「仍要运行」; browsers may warn the file is rarely
downloaded. Neither appears for the installed program, which is not downloaded. Signing removes neither at once (reputation builds).

**Updates: tell, never replace.** A release build asks `updateUrl` (GitHub `releases/latest`) a minute after it is
online, then daily, and writes 「有新版本 X.Y.Z，下载安装即可替换旧版：<downloadUrl>」 into its window and log; `agentdevice
status` checks too. No self-replacement until releases carry a signature the client pins (AV-HYGIENE.md 「自动更新」):
downloading and running unverified code is not small or safe; the installer already upgrades in place.
**Checks**: `TestShippedEnvironments` (keys the client reads, complete vs `missing`), `windows-e2e.yml` `install=release`
(installed state, login, online, uninstall, unbound). Inno Setup gone from the runner image: `choco install innosetup`.
