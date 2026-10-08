# 安全说明 / Security

## 报告漏洞

请不要在公开 issue 里报告漏洞。用 GitHub 的私密漏洞报告（本仓库 Security → Report a vulnerability）。我们会在 3 个工作日内回复。

Please report vulnerabilities privately through GitHub's "Report a vulnerability" (Security tab), not in public issues.

## 设计要点

- **只有出站连接**：设备主动连网关（WebSocket over TLS），本机不开任何监听端口。
- **每个请求都要控制面签名**：Ed25519；校验顺序是签名、设备号、参数摘要、过期时间（最多 65 秒）、随机数防重放，全部通过后才看本地访问级别。控制面公钥在链接时固定下来。
- **访问级别只在本机选**：默认只读。网页能看到当前级别，但不能修改；每次放宽级别、添加文件夹都弹本机确认框。
- **本机确认在前**：在本机同意之前，不启动任何 shell 进程。确认框默认按钮是“取消”，2 分钟没回应就当拒绝。
- **路径检查**：解析真实路径（符号链接、目录联接）后再比对文件夹；拒绝 UNC、`\\?\`、备用数据流、设备名等写法；文件打开以后再按句柄核对一次真实位置。应用自己的数据目录永远不可访问。
- **进程管理**：命令放进 Job Object（Windows）或进程组（Linux/macOS），断开、暂停或退出时整棵进程树一起结束。命令行是普通的 `powershell -NoProfile -NonInteractive -Command`，不用 `-EncodedCommand`，不改执行策略。
- **密钥**：设备私钥用 Windows DPAPI 加密保存（其他系统是仅本人可读的文件）；解除链接时覆盖删除。
- **审计**：每个请求（包括被拒绝的）写入带哈希链的 JSONL 日志，`agentrouter-device audit verify` 可以校验。
- **桌面壳和网页之间**：网页只能调用两个命令：读设备状态（不含任何密钥或令牌）、发起链接（仍要本机确认）。这两个命令只开放给配置的 AgentRouter 网址。
- **不签名**：现阶段发布的 exe 没有代码签名，也不写开机启动项，见 [docs/SIGNING.md](docs/SIGNING.md)；降低误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)（CI 检查）。
- **只连网关域名**：只接受 https 域名（不接受裸 IP）；127.0.0.1/localhost 只用于本地测试。

## Design summary

Outbound-only connection; every request is Ed25519-signed by the control plane and checked (signature, device, argument digest, expiry, replay) before the local gate; the access level is chosen only on the device (read-only by default) and every widening is confirmed in a native dialog; no shell starts before local consent; paths are resolved and re-checked on the opened handle; process trees live in a Job Object or process group; the device key is DPAPI-protected; a hash-chained audit log records every request. The web page can only read the device status and start linking. Builds are currently unsigned.
