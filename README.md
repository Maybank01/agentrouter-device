# AgentRouter 桌面版与设备连接器

AgentRouter 的客户端是一层很小的皮：窗口里就是云端网页（和浏览器里同一套页面），本机只多做一件事——把这台电脑链接上云：云端对话通过电脑上的本机工具（MCP 服务）干活。设备只做链接、转发和装 MCP 服务（所有者 2026-10-14：「电脑上的mcp就是接口，我们的链接就是把这些mcp能够链接上云」「就用现成的mcp就好了，就是默认安装的mcp」）。写代码的人用命令行 `agentrouter link` 就够了。

## 组成

| 目录 | 是什么 |
| --- | --- |
| `app/` | 桌面版（Tauri 2）：加载云端网页，托盘显示设备状态，本机确认对话框 |
| `crates/device-core` | 设备核心：协议校验（Ed25519 签名、过期、防重放）、本地访问级别、MCP 主机（启动和看护本机 MCP 服务、校验包摘要）、每轮检查点、「正在操作」提示条、审计日志、密钥保存 |
| `crates/connector-cli` | 命令行 `agentrouter`（旧名 `agentrouter-device` 也能用）：在项目文件夹里 `agentrouter link` |
| `npm/` | npm 分发：`npx @agentrouter-top/cli link`（或 `npm i -g @agentrouter-top/cli` 后 `agentrouter link`）；主包 `@agentrouter-top/cli` + 各平台二进制包 `@agentrouter-top/cli-<os>-<arch>`。测试版在 `next`（连 Dev），正式版 M2 |
| `docs/` | 签名计划等文档 |

## 访问级别（只在本机选，网页只显示不能改）

- **只限这些文件夹**（`agentrouter link` 默认）和 **完全访问**：本机工具（MCP 服务）为 AI 干活，不用问你；文件服务只看链接的文件夹，终端服务（DesktopCommander）到得了这台电脑的各个位置。会改文件的调用之前自动打检查点，`agentrouter undo` 撤销。
- **只读**（桌面版默认）和 **每条都确认**：本机工具不运行（回 `DENIED`），AI 只能看到这台电脑在线和链接的文件夹。

不管哪一级，每个请求都写进带哈希链的审计日志。

## 屏幕（目前只有 Windows）

`agentrouter screen on`（或 `agentrouter link --screen`）之后，默认一组本机工具里加上 Windows 操作（Windows-MCP）：截图、界面元素、鼠标键盘、打开和关闭程序；录屏由终端服务运行 ffmpeg。打开时就是同意，之后不再逐条问；AI 操作时屏幕上方有一条「本机工具 <名字> · <工具>」的提示条（不进截图），锁屏 / UAC / 安全桌面上不调用。

## 本机工具（MCP 服务）

我们只做远程联通，能力交给电脑上的 MCP 服务（所有者 2026-10-13）。`agentrouter mcp on`（或 `agentrouter link --mcp`）之后，云端对话可以把 stdio MCP 服务（例如 `uvx windows-mcp@0.8.8 serve`）装到这台电脑上：小助手启动它、崩了重启（2 分钟内最多 3 次）、留 stderr 日志（密钥涂掉）、把它的工具调用原样转过去（截图照样是图）。只在“只限这些文件夹”和“完全访问”级别；锁屏和 UAC 上不调用；调用时屏幕上方提示条写“本机工具 <名字> · <工具>”；`mcp off`、托盘断开、Ctrl+C 全部停掉。定义和密钥在云端（工具作品 + 密钥库），这里不落盘。协议见 Cloud 仓库 `DEVICE-PROTOCOL.md` §18，代码 `crates/device-core/src/mcp.rs`。

设备自己不再有文件、命令和屏幕动作（2026-10-14，DEVICE-PROTOCOL §18.4）：只服务 `info`、工作区和 MCP 动作，原来的每个工具由默认一组里的哪个 MCP 工具代替见 Cloud 仓库 `LINKED-DEVICES.md` §23.8。

## 工作区（默认关闭，M0）

`agentrouter workspace init` 建工作区（默认 `%USERPROFILE%\Workspaces`，兼容 `C:\Users\<user>\Workspaces`）并打开；`agentrouter workspace project add <名字>` 在工作区里建一个项目（目录在根下），`agentrouter workspace project list` 列出项目和位置。云端通过 `workspace_ensure`/`workspace_list` 用同一份登记（M0 只有根、项目登记和路径校验；git 基线、worktree 在 M1，云端会话绑定在 M2）。

硬规则：项目只能在工作区根下面；链接的游戏目录/程序目录只能作只读引用或部署目标，不能当工作区或项目（canonicalize 后双向检查，junction/symlink 逃逸也拒绝）。工作区默认关闭；`agentrouter status` 显示开关、根和项目数。代码 `crates/device-core/src/workspace.rs`，规范见 AgentTeam 的 `workspace-worktree-spec.md`。

## 命令行

```text
cd D:\work\my-project
agentrouter link            # 第一次：显示配对码，在网页上确认；之后：把当前文件夹加进来。然后前台运行，Ctrl+C 断开
agentrouter workspace init  # 建工作区（默认 %USERPROFILE%\Workspaces）并打开
agentrouter workspace project add <名字> | list
agentrouter undo            # 撤销 AI 在当前文件夹最近一轮的改动（--list 看全部检查点）
agentrouter status | unlink [文件夹] | access <级别> | audit verify
agentrouter run --background   # 不占终端
```

## 开发

本机只做 `cargo build` / `cargo check` / `cargo clippy`；测试只在 GitHub 托管的 runner 上跑（见 `.github/workflows/ci.yml`）。CI 产物没有代码签名，见 [docs/SIGNING.md](docs/SIGNING.md)；降低杀软误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)。安全说明见 [SECURITY.md](SECURITY.md)。

---

## English summary

AgentRouter's desktop client is a thin shell: its window shows the cloud web app (the same pages as in the browser) and adds one thing, a linked-device connector. Since 2026-10-14 the device is only the link (owner: use the off-the-shelf MCP servers installed by default): it relays the cloud conversations' calls to local MCP servers (files, git, DesktopCommander, serena, Windows-MCP with the screen on, or any other) that it starts and supervises here, and has no file, command or screen actions of its own. `app/` is the Tauri 2 shell (tray), `crates/device-core` holds the protocol checks (Ed25519-signed, expiring, replay-protected requests), the local access levels, the MCP host (package digests checked before a start), the per-turn checkpoint before a writing call (`agentrouter undo`), the on-screen bar and a hash-chained audit log, and `crates/connector-cli` is the `agentrouter` command line (`agentrouter link` in a project folder; `agentrouter-device` remains as an alias). There are no inbound ports: the device connects out to the gateway. Builds are unsigned (see `docs/SIGNING.md`); tests run only in GitHub-hosted CI. M0 adds an opt-in workspace root and project registry (`agentrouter workspace init|project`; the signed `workspace_ensure`/`workspace_list` actions with canonical path guards); git baselines and worktrees come in M1. Licensed under Apache-2.0.
