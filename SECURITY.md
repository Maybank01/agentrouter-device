# 安全说明 / Security

## 报告漏洞

请不要在公开 issue 里报告漏洞。用 GitHub 的私密漏洞报告（本仓库 Security → Report a vulnerability）。我们会在 3 个工作日内回复。

Please report vulnerabilities privately through GitHub's "Report a vulnerability" (Security tab), not in public issues.

## 设计要点

- **只有出站连接**：设备主动连网关（WebSocket over TLS），本机不开任何监听端口。
- **每个请求都要控制面签名**：Ed25519；校验顺序是签名、设备号、参数摘要、过期时间（最多 65 秒）、随机数防重放，全部通过后才看本地访问级别。控制面公钥在链接时固定下来。
- **访问级别只在本机选**：默认只读。网页能看到当前级别，但不能修改；每次放宽级别、添加文件夹都弹本机确认框。
- **范围而不是逐条审批**：默认的「只限这些文件夹」级别在文件夹里不逐条问，安全靠三样：路径检查（文件工具出不了文件夹）、越界命令拦截（写文件夹以外、系统目录、`.ssh` / 浏览器配置 / Windows 凭据、注册表、格式化、`runas` / `sudo` 提权、全局安装，一律 `OUT_OF_SCOPE`，不弹窗）、每轮改动前的检查点（git 仓库用隐藏引用 `refs/agentrouter/cp/*` 快照，不碰分支和暂存区；其他文件夹备份要改的文件，`agentrouter undo` 撤销）。命令拦截是启发式的，只看命令文字，用来尽早给模型一个清楚的回答。
- **Windows 上的写入边界由系统保证**（`confine.rs`）：在「只限这些文件夹」和「每条都确认」级别，命令以**低完整性级别**运行（本人令牌的受限副本，去掉特权），链接的文件夹打上可继承的 Low 标签。系统按实际打开的对象检查，所以不管命令怎么写（PowerShell 的 .NET 调用、python、node、相对路径、8.3 短名、目录联接、符号链接、往正在运行的任务里输入），都只能写、删、改名文件夹里的东西和它自己的临时文件夹（`AppData\LocalLow\AgentRouter\Device`，TEMP 和常见工具缓存指向这里）；HKCU 注册表（`AppDataLow` 以外）也写不了。用户同意的越界命令（`beyondScope`）和「完全访问」不受限。标签记录在数据目录的 `confined.json` 里，取消链接文件夹、解除链接或被撤销时撤回。**不覆盖**：没有 NTFS 权限的盘（FAT32/exFAT U 盘）、网络共享（由服务器决定）、从别处移进文件夹而保留原标签的文件（命令写不了它）、通过已在中完整性运行的程序代办（COM 服务、计划任务、其他已打开的程序）、读取（照旧允许）。基于 MSYS2/Cygwin 的程序（Git Bash 的 `sh`、`bash` 等，以及用 sh 写的 git 钩子）在低完整性下起不来；`git` 本身、PowerShell、cmd、python、node 正常。macOS / Linux 目前只有命令文字检查。
- **本机确认在前**：在更严格的级别和越界请求上，本机同意之前不启动任何 shell 进程。确认在终端（前台）或 Windows 弹窗（后台）里问，默认是拒绝；云端最多等 10 分钟，到时当拒绝。macOS / Linux 后台没有界面，一律拒绝。
- **路径检查**：解析真实路径（符号链接、目录联接）后再比对文件夹；拒绝 UNC、`\\?\`、备用数据流、设备名等写法；文件打开以后再按句柄核对一次真实位置。应用自己的数据目录永远不可访问。
- **进程管理**：命令放进 Job Object（Windows）或进程组（Linux/macOS），断开、暂停或退出时整棵进程树一起结束。命令行是普通的 `powershell -NoProfile -NonInteractive -Command`，不用 `-EncodedCommand`，不改执行策略。
- **密钥**：设备私钥用 Windows DPAPI 加密保存（其他系统是仅本人可读的文件）；解除链接时覆盖删除。
- **审计**：每个请求（包括被拒绝的）写入带哈希链的 JSONL 日志，`agentrouter audit verify` 可以校验。
- **桌面壳和网页之间**：网页只能调用两个命令：读设备状态（不含任何密钥或令牌）、发起链接（仍要本机确认）。这两个命令只开放给配置的 AgentRouter 网址。
- **不签名**：现阶段发布的 exe 没有代码签名，也不写开机启动项，见 [docs/SIGNING.md](docs/SIGNING.md)；降低误报的构建要求见 [docs/AV-HYGIENE.md](docs/AV-HYGIENE.md)（CI 检查）。
- **只连网关域名**：只接受 https 域名（不接受裸 IP）；127.0.0.1/localhost 只用于本地测试。

## Design summary

Outbound-only connection; every request is Ed25519-signed by the control plane and checked (signature, device, argument digest, expiry, replay) before the local gate; the access level is chosen only on the device (read-only by default) and every widening is confirmed locally; at the default folders level commands run without prompts inside the linked folders, clear overreach is refused (`OUT_OF_SCOPE`, a heuristic text guard) and on Windows the write boundary is the OS's own: commands run at low integrity and only the linked folders carry a Low label, so writes elsewhere fail however the command is written (not covered: non-NTFS volumes, network shares, brokered requests to medium-integrity programs; MSYS2/Cygwin tools do not start; macOS/Linux have the text guard only) and each turn is checkpointed for `agentrouter undo`; at stricter levels no shell starts before local consent; paths are resolved and re-checked on the opened handle; process trees live in a Job Object or process group; the device key is DPAPI-protected; a hash-chained audit log records every request. The web page can only read the device status and start linking. Builds are currently unsigned.
