# AgentRouter 桌面版与设备连接器

AgentRouter 的客户端是一层很小的皮：窗口里就是云端网页（和浏览器里同一套页面），本机只多做一件事——在你同意的范围内，让云端对话使用这台电脑（列文件、读写文件、运行命令）。

## 组成

| 目录 | 是什么 |
| --- | --- |
| `app/` | 桌面版（Tauri 2）：左边是云端网页（和浏览器同一套），右边是本机的「电脑」栏；链接窗口、确认窗口、“正在使用”控制条、托盘。界面在 `app/ui/`（03 樱粉） |
| `crates/device-core` | 设备核心：协议校验（Ed25519 签名、过期、防重放）、本地访问级别、命令与文件、审计日志、密钥保存 |
| `crates/connector-cli` | 命令行连接器 `agentrouter-device`：服务器、无界面机器和 CI 用 |
| `docs/` | 签名计划等文档 |

## 访问级别（只在本机选，网页只显示不能改）

- **只读**（默认）：只能读你选的文件夹里的文件；不能写文件，不能运行命令。
- **只限这些文件夹**：文件只能在你选的文件夹里读写；每条命令都先问你。
- **每条都确认**：文件和命令都先问你。
- **完全访问**：每个对话第一次运行命令时问一次，之后不再逐条问。

不管哪一级，应用自己的数据目录（密钥、审计日志）都碰不到；每个请求都写进带哈希链的审计日志。

## 桌面版的几个窗口

- **主窗口**：云端网页 + 右侧「电脑」栏：这台电脑（在线、AI 能不能用、访问级别）、允许别人连这台电脑（设备码和口令，开之前先问一句防骗）、正在连着、连接别人的电脑、让本地 AI 用（复制提示词、各客户端的配置）。分享和连接别人的电脑等云端开通（`CONTROL_PLANE_DEVICE_SHARES`），现在点了会说“分享还在开通中”。
- **链接窗口**：设备名、四档访问级别（竖排单选，默认“只限这些文件夹”）、正在连上、连上了、没连上。
- **确认窗口**：默认焦点在“拒绝”；Esc、关窗、2 分钟不点都算拒绝；删除类命令标红，没有“同类以后直接允许”。
- **“正在使用这台电脑”控制条**：有人连着就钉在屏幕顶端（只能沿顶端拖，拖到边上变小），写着谁、哪个 AI、在做什么、多久了，带暂停和断开；运行命令时屏幕四周有粉色光。**控制条显示不出来，任何请求都不执行。**
- **托盘**：品牌标 + 状态点（绿空闲、粉干活、黄重连、灰断开），点开是菜单。

CI 的 “Windows GUI smoke” 会把这些窗口逐个截图（产物 `gui-smoke-screenshots`）。

## 命令行

```text
agentrouter-device link --access folders --folder D:\work   # 链接（显示配对码，在网页上确认）
agentrouter-device run                                       # 连上网关，等云端对话的请求
agentrouter-device status | access <级别> | unlink | audit verify
agentrouter-device mcp                                       # 本机 MCP（stdio）：让 Claude Code、Codex、Cursor 用这台电脑
agentrouter-device mcp setup                                 # 打印给本机 AI 的提示词和各客户端的配置
```

`mcp` 只是一层转发：它通过只有当前用户能连的本机通道（Windows 命名管道 / Unix 套接字，不开端口）把调用交给正在运行的 AgentRouter，后者照样过本机访问级别、确认框、“正在被使用”控制条和审计。AgentRouter 没开时，工具回答“先打开 AgentRouter”。

```text
claude mcp add --scope user agentrouter -- "<安装目录>gentrouter-device.exe" mcp
codex mcp add agentrouter -- "<安装目录>gentrouter-device.exe" mcp
# Cursor：~/.cursor/mcp.json → {"mcpServers": {"agentrouter": {"command": "<exe>", "args": ["mcp"]}}}
```

`link --unattended` 用于服务器和 CI：本机管理员在链接时预先同意命令，运行时不再逐条确认（只读级别不生效）。

## 开发

本机只做 `cargo build` / `cargo check` / `cargo clippy`；测试只在 GitHub 托管的 runner 上跑（见 `.github/workflows/ci.yml`）。CI 产物没有代码签名，见 [docs/SIGNING.md](docs/SIGNING.md)；降低杀软误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)。安全说明见 [SECURITY.md](SECURITY.md)。

---

## English summary

AgentRouter's desktop client is a thin shell: its window shows the cloud web app (the same pages as in the browser) and adds one thing, a linked-device connector that lets cloud conversations use this computer within limits the person sets here. `app/` is the Tauri 2 shell (tray, native consent dialogs), `crates/device-core` holds the protocol checks (Ed25519-signed, expiring, replay-protected requests), the local access levels (read-only by default, folders, confirm each, full), command and file actions and a hash-chained audit log, and `crates/connector-cli` is the headless connector for servers and CI. There are no inbound ports: the device connects out to the gateway. Builds are unsigned (see `docs/SIGNING.md`); tests run only in GitHub-hosted CI. Licensed under Apache-2.0.
