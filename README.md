# AgentRouter 桌面版与设备连接器

AgentRouter 的客户端是一层很小的皮：窗口里就是云端网页（和浏览器里同一套页面），本机只多做一件事——在你同意的范围内，让云端对话使用这台电脑（列文件、读写文件、运行命令）。

## 组成

| 目录 | 是什么 |
| --- | --- |
| `app/` | 桌面版（Tauri 2）：加载云端网页，托盘显示设备状态，本机确认对话框 |
| `crates/device-core` | 设备核心：协议校验（Ed25519 签名、过期、防重放）、本地访问级别、命令与文件、审计日志、密钥保存 |
| `crates/connector-cli` | 命令行连接器 `agentrouter-device`：服务器、无界面机器和 CI 用 |
| `docs/` | 签名计划等文档 |

## 访问级别（只在本机选，网页只显示不能改）

- **只读**（默认）：只能读你选的文件夹里的文件；不能写文件，不能运行命令。
- **只限这些文件夹**：文件只能在你选的文件夹里读写；每条命令都先问你。
- **每条都确认**：文件和命令都先问你。
- **完全访问**：每个对话第一次运行命令时问一次，之后不再逐条问。

不管哪一级，应用自己的数据目录（密钥、审计日志）都碰不到；每个请求都写进带哈希链的审计日志。

## 命令行

```text
agentrouter-device link --access folders --folder D:\work   # 链接（显示配对码，在网页上确认）
agentrouter-device run                                       # 连上网关，等云端对话的请求
agentrouter-device status | access <级别> | unlink | audit verify
```

`link --unattended` 用于服务器和 CI：本机管理员在链接时预先同意命令，运行时不再逐条确认（只读级别不生效）。

## 开发

本机只做 `cargo build` / `cargo check` / `cargo clippy`；测试只在 GitHub 托管的 runner 上跑（见 `.github/workflows/ci.yml`）。CI 产物没有代码签名，见 [docs/SIGNING.md](docs/SIGNING.md)；降低杀软误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)。安全说明见 [SECURITY.md](SECURITY.md)。

---

## English summary

AgentRouter's desktop client is a thin shell: its window shows the cloud web app (the same pages as in the browser) and adds one thing, a linked-device connector that lets cloud conversations use this computer within limits the person sets here. `app/` is the Tauri 2 shell (tray, native consent dialogs), `crates/device-core` holds the protocol checks (Ed25519-signed, expiring, replay-protected requests), the local access levels (read-only by default, folders, confirm each, full), command and file actions and a hash-chained audit log, and `crates/connector-cli` is the headless connector for servers and CI. There are no inbound ports: the device connects out to the gateway. Builds are unsigned (see `docs/SIGNING.md`); tests run only in GitHub-hosted CI. Licensed under Apache-2.0.
