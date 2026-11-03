# 安全说明 / Security

## 报告漏洞

请不要在公开 issue 里报告漏洞。用 GitHub 的私密漏洞报告（本仓库 Security → Report a vulnerability）。我们会在 3 个工作日内回复。

Please report vulnerabilities privately through GitHub's "Report a vulnerability" (Security tab), not in public issues.

## 设计要点

设计全文见 [docs/HOSTD-MCP.md](docs/HOSTD-MCP.md)（§3.3 签名、§6 沙箱、§7 审计和撤销）。

- **只有出站连接**：`agentrouter link` 主动连网关（WebSocket over TLS），本机不开任何监听端口。只接受 https 网关域名；127.0.0.1/localhost 只用于本地测试。
- **每个调用都要控制面签名**：Ed25519，放在 MCP 的 `params._meta` 里（签名 v3）；校验链接、会话、参数摘要、过期时间（60 秒 + 5 秒时钟偏差）和随机数防重放，全部通过才执行。控制面公钥在配对时固定下来；只执行设备主人账号发起的会话。
- **文件夹边界由操作系统保证**：每个本机工具（MCP 服务，包括 `host`）都在系统沙箱里运行（Anthropic `sandbox-runtime`，固定版本；Linux bubblewrap、macOS Seatbelt、Windows 独立的沙箱账户）：只能写链接的文件夹，读不到 `~/.ssh` 等密钥文件夹，网络只放行包镜像和目录里登记的域名。沙箱不可用时一个工具也不启动，不会退回到沙箱外运行。「完全访问」（`--full`）是本人在本机的选择，工具用本人账户的权限。
- **同意只在本机**：运行 `link` 就是对这些文件夹的同意；唯一会问的是装一个不在目录里的本机工具（终端 y/N，默认不同意）。云端改不了访问级别。
- **审计**：每个请求和回答（连参数，密钥类的值替换掉）写入带哈希链的只追加日志，链头随心跳发给云端；`agentrouter audit verify` 校验。
- **撤销**：Agent 改文件前把文件夹快照进文件夹外的影子 git 仓库（不碰本人的 `.git`），`agentrouter undo` 撤销最近一轮。
- **进程管理**：命令放进 Job Object（Windows）或进程组（Linux/macOS），断开或超时时整棵进程树一起结束。
- **密钥**：设备私钥在 Windows 上用 DPAPI 加密保存（其他系统是仅本人可读的文件）；解除链接时覆盖删除。
- **不签名**：现阶段发布的 exe 没有代码签名，见 [docs/adr/0001-code-signing.md](docs/adr/0001-code-signing.md)；降低误报的构建要求见 [docs/development/AV-HYGIENE.md](docs/development/AV-HYGIENE.md)（CI 检查）。

## Design summary

Outbound-only connection to an https gateway; every call carries the control plane's Ed25519 signature (v3, in MCP
`params._meta`: link, session, argument digest, expiry, replay nonce) and runs only for sessions of the device owner's
account; every local tool, `host` included, runs in an OS sandbox (Anthropic `sandbox-runtime`, pinned) that can write
only the linked folders, cannot read key folders and reaches only allow-listed domains, and nothing runs when the
sandbox is unavailable; 完全访问 is a local choice; consent is local only (starting `link`; installing an off-catalog
server asks y/N); a hash-chained, argument-level audit log whose head the cloud keeps; shadow-git checkpoints outside
the folder for `agentrouter undo`; process trees in a Job Object or process group; the device key is DPAPI-protected on
Windows. Builds are currently unsigned (docs/HOSTD-MCP.md §3.3, §6, §7).
