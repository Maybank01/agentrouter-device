# AgentRouter 桌面版与设备连接器

AgentRouter 的客户端是一层很小的皮：窗口里就是云端网页（和浏览器里同一套页面），本机只多做一件事——在你同意的范围内，让云端对话使用这台电脑（列文件、读写和修改文件、搜索、运行命令）。写代码的人用命令行 `agentrouter link` 就够了。

## 组成

| 目录 | 是什么 |
| --- | --- |
| `app/` | 桌面版（Tauri 2）：加载云端网页，托盘显示设备状态，本机确认对话框 |
| `crates/device-core` | 设备核心：协议校验（Ed25519 签名、过期、防重放）、本地访问级别、命令与文件、审计日志、密钥保存 |
| `crates/connector-cli` | 命令行 `agentrouter`（旧名 `agentrouter-device` 也能用）：在项目文件夹里 `agentrouter link` |
| `npm/` | npm 分发（`npx agentrouter link`）：主包 `agentrouter` + 各平台二进制包 `@agentrouter-top/cli-<os>-<arch>`；M2 发布 |
| `docs/` | 签名计划等文档 |

## 访问级别（只在本机选，网页只显示不能改）

- **只读**（桌面版默认）：只能读你选的文件夹里的文件；不能写文件，不能运行命令。
- **只限这些文件夹**（`agentrouter link` 默认）：在链接的文件夹里读写文件、运行命令都不用问；明显越界的命令（写文件夹以外、系统目录、密钥和凭据、提权、全局安装、格式化、注册表）直接拒绝（`OUT_OF_SCOPE`），AI 说明理由后可以单独问你一次。每轮改动前自动打检查点，`agentrouter undo` 撤销。
- **每条都确认**：文件和命令都先问你（`git status`、`ls`、`rg` 这类只读命令除外）。
- **完全访问**：每个对话第一次运行命令时问一次，之后不再逐条问。

不管哪一级，应用自己的数据目录（密钥、审计日志、检查点）都碰不到；每个请求都写进带哈希链的审计日志。

设备动作（协议见 Cloud 仓库 `docs/product/workspace-v1/DEVICE-PROTOCOL.md` §5）：`list_files`、`list_dir`、`read_file`、`read_files`、`write_file`、`edit_file`（旧→新替换，带 base hash 冲突检查）、`apply_patch`（Codex 格式）、`search`（ripgrep 库）、`exec`、`job`。测试向量在 `crates/device-core/tests/fixtures/action-vectors.json`。

## 命令行

```text
cd D:\work\my-project
agentrouter link            # 第一次：显示配对码，在网页上确认；之后：把当前文件夹加进来。然后前台运行，Ctrl+C 断开
agentrouter undo            # 撤销 AI 在当前文件夹最近一轮的改动（--list 看全部检查点）
agentrouter status | unlink [文件夹] | access <级别> | audit verify
agentrouter run --background   # 不占终端：Windows 上需要确认时弹窗，macOS / Linux 直接拒绝
```

需要确认时（只在更严格的级别或越界请求时）终端里会显示完整命令、工作目录和对话：`y` 允许这一次，`a` 本对话同类的都允许，`n` 拒绝，`d` 拒绝并断开。云端最多等 10 分钟，期间可以用 `job` 查这条确认。

`link --unattended` 用于服务器和 CI：本机管理员在链接时预先同意命令，运行时不再逐条确认（只读级别不生效）。

## 开发

本机只做 `cargo build` / `cargo check` / `cargo clippy`；测试只在 GitHub 托管的 runner 上跑（见 `.github/workflows/ci.yml`）。CI 产物没有代码签名，见 [docs/SIGNING.md](docs/SIGNING.md)；降低杀软误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)。安全说明见 [SECURITY.md](SECURITY.md)。

---

## English summary

AgentRouter's desktop client is a thin shell: its window shows the cloud web app (the same pages as in the browser) and adds one thing, a linked-device connector that lets cloud conversations use this computer within limits the person sets here. `app/` is the Tauri 2 shell (tray, native consent dialogs), `crates/device-core` holds the protocol checks (Ed25519-signed, expiring, replay-protected requests), the local access levels (read-only by default, folders, confirm each, full), command and file actions and a hash-chained audit log, and `crates/connector-cli` is the `agentrouter` command line (`agentrouter link` in a project folder; `agentrouter-device` remains as an alias). At the default folders level the linked folders are fully usable without prompts, clear overreach is refused with `OUT_OF_SCOPE`, and every turn is checkpointed so `agentrouter undo` can roll it back. There are no inbound ports: the device connects out to the gateway. Builds are unsigned (see `docs/SIGNING.md`); tests run only in GitHub-hosted CI. Licensed under Apache-2.0.
