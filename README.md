# AgentRouter 设备客户端（agentdevice）

装上客户端，这台电脑就成了云端 Agent 可以用 SSH 连上的执行设备：它加入你自己的私有网络（NetBird，用户态，不要管理员权限、不装驱动），
只在那个网络里可达，电脑自己的网络上不开任何端口。设备只执行通过 SSH 来的命令和文件传输，没有自己的工具（所有者 2026-11-04
「设备端只做执行」，Cloud #3976）。

## 怎么用

Windows：在「我的设备」下载安装程序（现在只有 Dev 的 `agentdevice-setup-dev.exe`，[发布页](https://github.com/agentrouter-top/AgentDevice/releases/latest)），
双击安装，不需要管理员。程序没有代码签名，SmartScreen 提示时点「更多信息」→「仍要运行」。装好后浏览器打开登录页并填好设备码，用你的账号
确认，窗口里显示在线；默认登录电脑后自动启动。常驻一个「AgentRouter 设备」窗口，每件 Agent 做的事一行；关掉窗口就断开。有新版本时
窗口里会说，下载新的安装程序装上即可（登录保留）。在「我的设备」里移除这台电脑，客户端就断开并忘掉登录；在「设置 › 应用」里卸载，会
一并把这台电脑从账号里移除并删掉它的文件。Linux：下载 `agentdevice-linux-<amd64|arm64>-dev.tar.gz`，解压后运行 `agentdevice`。

| 命令 | 作用 |
| --- | --- |
| `agentdevice [run] [--name 名字] [--no-tray]` | 登录（第一次）并保持在线 |
| `agentdevice login [--name 名字]` | 只登录并加入网络（没有桌面的电脑） |
| `agentdevice status` / `facts` | 这台电脑的状态 / 上报的电脑信息 |
| `agentdevice autostart on\|off\|status` | 登录电脑后自动启动（默认关） |
| `agentdevice unbind` | 从账号里移除这台电脑 |
| `agentdevice stop` / `uninstall` | 关掉正在运行的客户端 / 卸载（安装程序用：关自动启动、关客户端、移除这台电脑、删文件夹） |
| `agentdevice sandbox [--join 文件]` | 云电脑（算力层的沙箱）按算力层给的文件加入网络 |
| `agentdevice connect <主机> <端口>` | 给 ssh 的 `ProxyCommand` 用 |

正式发布是 `client-vX.Y.Z`（安装程序、Linux 压缩包、`SHA256SUMS`）；生产环境的地址还没定，所以现在只有 Dev 的安装程序。另有给 Dev
测试用的预发布构建 `client-v0.0.0-<sha7>`（Linux 静态构建给云电脑，Windows 构建内置 Dev 地址）。

## Development

Charter (what belongs here, contracts consumed, how it is pinned): [docs/CHARTER.md](docs/CHARTER.md). Every doc:
[docs/README.md](docs/README.md).

| Module | Current state |
| --- | --- |
| [`client/`](client/README.md) | the Go program `agentdevice`: NetBird `client/embed` (userspace), our own SSH server ([ADR 0002](docs/adr/0002-own-ssh-server.md)), device-code login, enrolment and facts (AgentCompute `compute/v1`), tray, autostart |
| [`release/`](release/README.md) | releases for people: `release.yml`, the environments (`environment.json` beside the program), the per-user Windows installer (Inno Setup), SmartScreen, the update notice |
| `scripts/` | repository checks (`check-docs.mjs`, `check-repo-imports.mjs`, `check-node-version.mjs`, `av-hygiene.sh`), the main-red guard (`main-red.mjs`), the Windows end-to-end harness (`windows-e2e/`) |
| `.github/workflows/` | `ci.yml` (checks, client tests, client pre-release), `release.yml` (releases for people), `merge-on-green.yml`, `main-red.yml`, `windows-e2e.yml` |
| `skills/agentrouter-client/` | the coding-agent skill for this repository |

Tests run on GitHub-hosted runners only (R47; `.github/workflows/ci.yml`). Required checks of the branch protection, also
`REQUIRED` in `merge-on-green.yml`: `Repository checks`, `Client (Linux)`, `Client (macOS)`, `Client (Windows)`. No review
is required: PRs merge through the bot once green (workspace rule R60). After a merge every CI job of `main` gates: a red
run is re-run once, then reverted or reported in a `main-red` issue (`main-red.yml`, `scripts/main-red.mjs`). Locally, in
`client/`: `go build ./...` and `go vet ./...` only (also `GOOS=windows` / `darwin`); at the root `bash
scripts/av-hygiene.sh`, `node scripts/check-docs.mjs`, `node scripts/check-repo-imports.mjs`. Build rules against
antivirus false positives: [docs/development/AV-HYGIENE.md](docs/development/AV-HYGIENE.md).

Releases for people: `gh workflow run release.yml -f version=X.Y.Z` ([release/README.md](release/README.md)); the end-to-end
check of a release's installer is `gh workflow run windows-e2e.yml -f install=release` (Agent side `agent.py --computer
<id> --then uninstall`). Pre-releases: every push to `main` that changes `client/` publishes `client-v0.0.0-<sha7>` (static Linux amd64/arm64
for compute/v1 sandboxes, Windows amd64 with Dev's endpoints, `SHA256SUMS`); AgentCompute's environment image pins one
by tag + sha256. The Windows device end-to-end check (M2 acceptance, #231) runs on demand on a hosted Windows runner:
`gh workflow run windows-e2e.yml`, Agent side `python3 scripts/windows-e2e/agent.py --name gh-win-<run id>-<attempt>`
(how: the workflow's header).

Removed on 2026-11-05 (#236, Cloud #3976): the Rust device connector `hostd` / `agentrouter link` with executor
protocol v0, the `host__*` MCP tools, the link layer, MCP aggregator, sandbox, checkpoints and the frozen desktop shell,
their npm packaging (`@agentrouter-top/cli`), release workflows (`hostd-v*`, `socat-static-*`, `bwrap-static-*`,
`@agentrouter-top/sandbox-tools-linux`) and the PC parity bench. Released copies keep working against production until
the base cutover; the published releases and npm versions stay. Source: tag `hostd-final` ([docs/ARCHIVE.md](docs/ARCHIVE.md)).
