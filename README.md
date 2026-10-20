# AgentRouter 设备连接器（控制台模式）

在电脑上任意一个文件夹里运行一条命令，云端对话里的 Agent 就能在**这个文件夹里**干活；命令开着就一直连着，Ctrl+C 断开。

```text
cd D:\work\my-mod
npx @agentrouter-top/cli link        # 或者装好后：agentrouter link
```

第一次运行会给出一个配对码，在网页上确认后这台电脑就链接到你自己的账号（只链到自己的账号，没有分享码）。

## 边界

- 每个本机工具（文件、Git、终端、代码理解……都是 MCP 服务）都在系统沙箱里运行：**只能写这个文件夹**，文件夹外的写、删、改名由系统直接拒绝，读不到 `~/.ssh` 等密钥文件夹，网络只放行包镜像和目录里登记的域名。
- 沙箱不可用时，本机工具一个也不运行（不会在沙箱外运行）：按提示运行 `agentrouter sandbox setup`（Windows 会弹一次管理员确认）。
- 同意只在本机：运行 `link` 就是同意；唯一会问你的是装一个不在目录里的本机工具（终端里 y/N，120 秒不答算不同意）。
- 每次调用连参数一起记进本机只追加的审计日志；Agent 改文件前自动打检查点，`agentrouter undo` 撤销。

## 命令

| 命令 | 作用 |
| --- | --- |
| `agentrouter link [--read-only] [--yes-servers]` | 链接当前文件夹并保持连接（`--read-only`：只读；`--yes-servers`：无人值守时不逐个问） |
| `agentrouter undo [--list] [检查点]` | 撤销 Agent 最近一轮改动（再运行一次再往前一轮）；`--list` 列出检查点 |
| `agentrouter audit [verify] [条数]` | 这个文件夹的审计记录；`verify` 检查有没有被改过 |
| `agentrouter sandbox setup\|status\|remove` | 本机工具的隔离环境 |
| `agentrouter status` | 账号、网关、沙箱、当前文件夹 |
| `agentrouter logout` | 这台电脑退出链接 |

## Development

Design: [`docs/LINKER-MCP.md`](docs/LINKER-MCP.md). One chain, MCP end to end:

| Crate | Role |
| --- | --- |
| `crates/linker` | The link: one outbound WebSocket per running `link` (`/device/v2/link`, hello v2, `{s, m}` frames, reconnect, heartbeat with the audit head). No capabilities. |
| `crates/aggregator` | One MCP endpoint (official Rust SDK `rmcp`): checks the control plane's signature v3 in `params._meta`, argument-level audit, checkpoints, `link__*` server management; an MCP client per local server. |
| `crates/sandbox` | Every server process OS-confined to the folder (Anthropic `sandbox-runtime`, pinned). |
| `crates/checkpoint` | Shadow-git checkpoints outside the folder; `undo`. |
| `crates/device-core` | Pairing/token, keys, protocol primitives, audit log, pinned MCP packages, config. |
| `crates/connector-cli` | The `agentrouter` binary (console mode; `console.rs`), npm `@agentrouter-top/cli`. |

Tests run on GitHub-hosted runners only (`.github/workflows/ci.yml`; the `Sandbox (…)` jobs exercise the OS boundary on Linux, macOS and Windows). Locally: `cargo build` / `cargo check` / `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, `bash scripts/av-hygiene.sh`. Build rules against antivirus false positives: [`docs/AV-HYGIENE.md`](docs/AV-HYGIENE.md).

Removed with the old device chain (#49): the signed device action set and its dispatcher, the home-grown MCP host, the old WebSocket connection, the built-in `mcp-serve workspace` service, the screen tools, in-repo checkpoint refs, the `run`/`access`/`unlink`/`screen`/`mcp`/`workspace`/`deploy`/`agent` commands and the old Dev E2E driver (#50 brings the new E2E). The desktop shell (`app/`) is frozen: source kept, outside the workspace, CI and releases.

Still here only for `agentrouter mcp-serve local` (local Agents, pending their own design, LINKER-MCP §4): `exec`, `shell`, `confine`/`confined`, `consent`, `deploy`, `workspace`, `worktree`, `gate`, `lockfile`, `local_agent`.
