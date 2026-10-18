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

## 工作区（默认关闭，M0/M1）

`agentrouter workspace init` 建工作区（默认 `%USERPROFILE%\Workspaces`，兼容 `C:\Users\<user>\Workspaces`）并打开；`agentrouter workspace project add <名字>` 在工作区里建一个项目（目录在根下）。云端通过 `workspace_ensure`/`workspace_list` 用同一份登记。

**git 基线与 worktree（M1）**：`workspace project baseline <项目>` 让项目有 git（新项目非破坏性 init + 一个基线提交，默认 `.gitignore`；已有仓库只读用它的 HEAD，不改配置/remote）。`workspace worktree create <项目> <会话 id>` 从基线给会话开 `<项目>/.worktrees/<会话 id>`、分支 `ar/<会话 id>`（幂等；`.worktrees/` 走仓库本地 `info/exclude`）；`worktree list/remove/prune` 列出、移除（默认保留分支）、清理过期目录。`mcp_start` 可带 `cwd`/`project`/`worktree: true` 或 `{{worktree}}`/`{{project}}` 占位符：本机工具（含 `start_process`）就在那个目录里跑，写调用先给会话 worktree 打检查点。云端绑定在 M2。

硬规则：项目只能在工作区根下面；链接的游戏目录/程序目录只能作只读引用或部署目标，不能当工作区或项目（canonicalize 后双向检查，junction/symlink 逃逸也拒绝）；`git init` 只发生在新项目目录。工作区默认关闭；`agentrouter status` 显示开关、根、项目数。代码 `crates/device-core/src/{workspace,worktree}.rs`，规范见 AgentTeam 的 `workspace-worktree-spec.md`。

**工作区 MCP 服务（S1，云端零改动）**：连接器内置 `agentrouter mcp-serve workspace`（stdio MCP，隐藏子命令），工具 = `exec(command, cwd?, timeout_ms?)` + `workspace_create/list` + `project_add/list` + `worktree_create/list/remove/prune`。云端用现有「装任意本机 MCP 服务」装一次：`runner: "command"`、`command: "agentrouter"`、`args: ["mcp-serve", "workspace"]`（定义可在会话里用 `device_mcp_install` 给出），之后会话里得到 `device__workspace__*` 工具。设备在 `<data>/bin` 放 `agentrouter` shim 并注入 MCP 子进程 PATH，同时把 `AGENTROUTER_SESSION`/`AGENTROUTER_DEVICE_HOME` 传给内置服务：exec 默认 cwd = 本会话 worktree。安全：cwd 只允许登记的项目/worktree（游戏目录、外部链接目录、设备数据目录拒绝）；高危命令 deny 列表 + 本机确认；每条 exec 写 `mcp-audit.log`，连接器审计里也带 command/cwd。后续：把它加进云端 catalog 默认 bundle（`workspace` preset）就不用在会话里手动装；低完整性 token、持久/流式 shell、deploy 审批+备份+回滚留到后续切片。
**低完整性边界（S2b）**：访问级别 `folders` 时，`exec` 在 Windows 上用低完整性受限 token 运行（`CreateRestrictedToken` + Low 标签 `S-1-16-4096`，Job Object 超时杀树），并把本次 cwd（会话 worktree/项目）标 Low、TEMP/TMP 与常见构建缓存指向 `%LOCALAPPDATA%Low\AgentRouter\Device`；工作区外（未标 Low 的 NTFS 对象）的写入/删除/改名/新建由内核拒绝，解释器、相对路径、junction/8.3 路径和子进程都绕不过去，读取仍允许。设备 data 目录若在工作区内会被保护回 Medium。标签记录在 `confined.json`，设备解链/文件夹移除时撤回。限制：无 NTFS 安全性的卷（FAT/exFAT）、网络共享、把文件移动进保留原标签的目录、以及向已运行的中完整性程序（COM/任务计划）代理请求不在边界内；`full` 级别是所有者明确要求不受限命令，此时不启用。Unix 暂无可移植的 OS 写边界（只有 gate/deny/consent/审计 + 进程组，Linux 额外设 `NO_NEW_PRIVS`），后续可用 bwrap/Landlock。**Shell 选择（Windows，2026-10-18 owner 要求）**：`exec` 默认优先 **PowerShell 7**（先找 PATH 里的 `pwsh.exe`，再找标准路径 `C:\Program Files\PowerShell\7\pwsh.exe`）；没装 PS7 时回退 Windows PowerShell 5.1。两者用同一套参数（`-NoLogo -NoProfile -NonInteractive -Command`），低完整性（S2b）spawn 也用选中的同一个 shell。#32 的 `PSModulePath` sanitize 只作用于 5.1 回退（避免 PS7 模块路径遮蔽）；选中 PS7 时保留继承的环境以免影响其模块发现。可选本机覆盖（后续再做配置文件/UI）：`AGENTROUTER_SHELL=pwsh|powershell|<绝对路径>`。
**本地 Agent 入口（2026-10-18）**：`agentrouter mcp-serve local` 是给本机 Agent 的 stdio MCP 服务（由本地 Agent 作为子进程拉起，不开端口），工具与 `workspace` 服务相同（deploy_target_* 随 full 模式门控的 PR 一起出现）。身份从启动环境 `AGENTROUTER_LOCAL_AGENT`（名字/type，如 dsh）与 `AGENTROUTER_LOCAL_SESSION`（会话 id）读取；连接器首次见到自动登记为 `lag_<id>`/`lses_<id>`（`<data>/local-agents.json`），用 `agentrouter agent list` 查看。exec 默认 cwd/worktree、deploy 归属按 `lses_` 隔离；同一项目的本地会话 exec 由跨进程项目锁串行（等待超时返回 `LOCAL_SESSION_BUSY`，不会打断已开始命令）。`mcp-audit.log` 与连接器 audit.log 的本地调用条目都带 caller，审批弹窗/拒绝文案也带 caller。限制：目前没有 token，按「同一 OS 用户 + 自报身份」处理（token/revoke 后续）；本地入口不会获得 Cloud 身份；files/git/屏幕聚合后续。**部署（S2a）**：工作区 MCP 服务的 `deploy(project, artifacts[], target, message?)` 把构建产物放到登记过的外部目标（如游戏目录），先备份被覆盖文件、本机批准、逐文件写 hash，失败自动回滚；`deploy_list` 列出记录、`deploy_rollback(id)` 一键恢复。目标只能由所有者在本机登记：`agentrouter workspace deploy target add <名字> <目录>`（本机确认；AI 没有登记目标的工具），`target list/remove`、`deploy list/rollback <id>` 也在同一命令下。记录在 `<data>/deployments/<id>/`（manifest：文件、hash、大小、来源 session/commit、目标、说明；backup/ 是旧文件），保留最近 20 次；每次调用的审批与结果写入 `mcp-audit.log`，连接器审计记录 deploy 的 target。无人值守机器（config.unattended）视为所有者已预授权；无头测试可用本机环境变量 `AGENTROUTER_CONSENT=allow|deny` 覆盖弹窗（只能由本机进程设置）。
**部署（S2a）**：工作区 MCP 服务的 `deploy(project, artifacts[], target, message?)` 把构建产物放到登记过的外部目标（如游戏目录），先备份被覆盖文件、本机批准、逐文件写 hash，失败自动回滚；`deploy_list` 列出记录、`deploy_rollback(id)` 一键恢复。目标登记：**folders 模式**只能由所有者在本机用 `agentrouter workspace deploy target add <名字> <目录>`（本机确认）；**full（完全访问）模式**下云端 Agent 也可用 workspace MCP 的 `deploy_target_add(name, path)` / `deploy_target_list()` / `deploy_target_remove(name)` 自己登记/移除（同一套校验：必须是已存在的外部目录、不能与工作区根/设备 data 重叠；folders/readonly/confirm 下调用返回 `DEPLOY_TARGET_OWNER_ONLY`）。**登记不等于允许部署**：每次 `deploy`/`deploy_rollback` 仍要本机 L2 审批、先备份、失败回滚。`target list/remove`、`deploy list/rollback <id>` 也在 CLI 下。记录在 `<data>/deployments/<id>/`（manifest：文件、hash、大小、来源 session/commit、目标、说明；backup/ 是旧文件），保留最近 20 次；每次调用的审批与结果写入 `mcp-audit.log`，连接器审计记录 deploy 的 target。无人值守机器（config.unattended）视为所有者已预授权；无头测试可用本机环境变量 `AGENTROUTER_CONSENT=allow|deny` 覆盖弹窗（只能由本机进程设置）。
## 命令行

```text
cd D:\work\my-project
agentrouter link            # 第一次：显示配对码，在网页上确认；之后：把当前文件夹加进来。然后前台运行，Ctrl+C 断开
agentrouter workspace init  # 建工作区（默认 %USERPROFILE%\Workspaces）并打开
agentrouter workspace project add <名字> | list | baseline <名字>`nagentrouter mcp-serve workspace   # 隐藏：云端把它当本机 MCP 服务启动（S1）`nagentrouter workspace deploy target add <名字> <目录>   # 所有者登记外部部署目标（S2a）
agentrouter workspace worktree create <项目> <会话 id> | list [项目] | remove <项目> <会话 id> | prune <项目>
agentrouter undo            # 撤销 AI 在当前文件夹最近一轮的改动（--list 看全部检查点）
agentrouter status | unlink [文件夹] | access <级别> | audit verify
agentrouter run --background   # 不占终端
```

## 开发

本机只做 `cargo build` / `cargo check` / `cargo clippy`；测试只在 GitHub 托管的 runner 上跑（见 `.github/workflows/ci.yml`）。CI 产物没有代码签名，见 [docs/SIGNING.md](docs/SIGNING.md)；降低杀软误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)。安全说明见 [SECURITY.md](SECURITY.md)。

---

## English summary

AgentRouter's desktop client is a thin shell: its window shows the cloud web app (the same pages as in the browser) and adds one thing, a linked-device connector. Since 2026-10-14 the device is only the link (owner: use the off-the-shelf MCP servers installed by default): it relays the cloud conversations' calls to local MCP servers (files, git, DesktopCommander, serena, Windows-MCP with the screen on, or any other) that it starts and supervises here, and has no file, command or screen actions of its own. `app/` is the Tauri 2 shell (tray), `crates/device-core` holds the protocol checks (Ed25519-signed, expiring, replay-protected requests), the local access levels, the MCP host (package digests checked before a start), the per-turn checkpoint before a writing call (`agentrouter undo`), the on-screen bar and a hash-chained audit log, and `crates/connector-cli` is the `agentrouter` command line (`agentrouter link` in a project folder; `agentrouter-device` remains as an alias). There are no inbound ports: the device connects out to the gateway. Builds are unsigned (see `docs/SIGNING.md`); tests run only in GitHub-hosted CI. M0 adds an opt-in workspace root and project registry (`agentrouter workspace init|project`; the signed `workspace_ensure`/`workspace_list` actions with canonical path guards); M1 adds the non-destructive git baseline, one git worktree per session under `<project>/.worktrees/<session>` (branch `ar/<session>`, `workspace_worktree_*` actions, `list`/`remove`/`prune`) and the per-session cwd for `mcp_start` (`cwd`/`project`/`worktree: true`, `{{worktree}}`/`{{project}}`), with writing calls checkpointed inside the session worktree. Licensed under Apache-2.0.





