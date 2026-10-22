# AgentRouter 设备连接器（控制台模式）

在电脑上任意一个文件夹里运行一条命令，云端对话里的 Agent 就能在**这个文件夹里**干活；命令开着就一直连着，Ctrl+C 断开。

```text
cd D:\work\my-mod
npx @agentrouter-top/cli link        # 或者装好后：agentrouter link
```

Windows 上也可以**直接双击 `agentrouter.exe`**（测试版）：第一次弹出文件夹选择窗口（默认是「文档」里新建的 `AgentRouter` 文件夹），浏览器打开「我的设备」并填好配对码，点「链接」确认，窗口里显示「已连上 · 在线」；以后再双击直接连上。和 `agentrouter link` 一样，云端 Agent 只能在这个文件夹里干活（第一次会准备隔离环境，Windows 弹一次管理员确认）；要整台电脑用 `agentrouter start --full`（完全访问）。关掉窗口就断开。`agentrouter start` 是同一件事。

`link` 启动的是 `hostd`（Host Daemon，宿主守护进程）：它在这台电脑上替云端 Agent 干活，`agentrouter hostd` 是同一条命令。

第一次运行会给出一个配对码，在网页上确认后这台电脑就链接到你自己的账号（只链到自己的账号，没有分享码）。

## 边界

- 每个本机工具（文件、Git、终端、代码理解……都是 MCP 服务）都在系统沙箱里运行：**只能写这个文件夹**，文件夹外的写、删、改名由系统直接拒绝，读不到 `~/.ssh` 等密钥文件夹，网络只放行包镜像和目录里登记的域名。
- 沙箱不可用时，本机工具一个也不运行（不会在沙箱外运行）：按提示运行 `agentrouter sandbox setup`（Windows 会弹一次管理员确认）。
- 不用自己装 Node.js 或 uv：第一次准备隔离环境时，`agentrouter` 从官方地址下载它自己用的固定版本（约 70 MB，校验 SHA-256），放在数据文件夹旁边，不改这台电脑上已有的。
- 同意只在本机：运行 `link` 就是同意；唯一会问你的是装一个不在目录里的本机工具（终端里 y/N，120 秒不答算不同意）。
- 每次调用连参数一起记进本机只追加的审计日志；Agent 改文件前自动打检查点，`agentrouter undo` 撤销。

## 命令

| 命令 | 作用 |
| --- | --- |
| `agentrouter link [文件夹…] [--read-only \| --full] [--yes-servers]` | 链接这些文件夹（默认当前文件夹）并保持连接；连着时输入 `+ 路径` 再加一个（`--read-only`：只读；`--full`：完全访问，本机工具用你的账户权限；`--yes-servers`：无人值守时不逐个问） |
| `agentrouter start [--full]` | 双击 `agentrouter.exe` 做的事：选工作文件夹（只选一次）、第一次在浏览器里确认、保持连接；云端 Agent 只能在这个文件夹里干活（`--full`：完全访问） |
| `agentrouter undo [--list] [--journal] [检查点]` | 撤销 Agent 在这个文件夹最近一轮的改动（再运行一次再往前一轮）；`--list` 列出检查点；`--journal` 放回完全访问时改过的文件 |
| `agentrouter audit [verify] [条数]` | 这个文件夹的审计记录；`verify` 检查有没有被改过 |
| `agentrouter sandbox setup\|status\|remove` | 本机工具的隔离环境 |
| `agentrouter status` | 账号、网关、沙箱、当前文件夹 |
| `agentrouter logout` | 这台电脑退出链接 |

## Development

Design: [`docs/HOSTD-MCP.md`](docs/HOSTD-MCP.md). One chain, MCP end to end:

| Crate | Role |
| --- | --- |
| `crates/hostd-link` | `hostd`'s link layer (formerly the Linker): one outbound WebSocket per running `link` (`/device/v2/link`, hello v2, `{s, m}` frames, reconnect, heartbeat with the audit head). No capabilities. |
| `crates/aggregator` | One MCP endpoint (official Rust SDK `rmcp`): checks the control plane's signature v3 in `params._meta`, argument-level audit, checkpoints, `link__*` server management; an MCP client per local server. |
| `crates/sandbox` | Every server process OS-confined to the folder (Anthropic `sandbox-runtime`, pinned). |
| `crates/checkpoint` | Shadow-git checkpoints outside the folder; `undo`. |
| `crates/device-core` | Pairing/token, keys, protocol primitives, audit log, pinned MCP packages, config. |
| `crates/connector-cli` | The `agentrouter` binary = `hostd` (`agentrouter link`, alias `agentrouter hostd`; console mode `console.rs`, cloud sandbox `cloud.rs`), npm `@agentrouter-top/cli`. |

Tests run on GitHub-hosted runners only (`.github/workflows/ci.yml`; the `Sandbox (…)` jobs exercise the OS boundary on Linux, macOS and Windows). Required checks: `Linux (core + command line)`, `Windows (command line + unsigned build)`, `Sandbox (Linux)`, `Sandbox (macOS)`. `Sandbox (Windows)` (4–7 min) gates after the merge: a red run of `main` is re-run once, then the merge is reverted through a revert PR with auto-merge when its parent was green, or reported in a `main-red` issue (`.github/workflows/main-red.yml`, `scripts/main-red.mjs`). Locally: `cargo build` / `cargo check` / `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `bash scripts/av-hygiene.sh`. Build rules against antivirus false positives: [`docs/AV-HYGIENE.md`](docs/AV-HYGIENE.md).

Removed with the old device chain (#49): the signed device action set and its dispatcher, the home-grown MCP host, the old WebSocket connection, the built-in `mcp-serve workspace` service, the screen tools, in-repo checkpoint refs, the `run`/`access`/`unlink`/`screen`/`mcp`/`workspace`/`deploy`/`agent` commands and the old Dev E2E driver (#50 brings the new E2E). The desktop shell (`app/`) is frozen: source kept, outside the workspace, CI and releases.

Still here only for `agentrouter mcp-serve local` (local Agents, pending their own design, HOSTD-MCP §4): `exec`, `shell`, `confine`/`confined`, `consent`, `deploy`, `workspace`, `worktree`, `gate`, `lockfile`, `local_agent`.
