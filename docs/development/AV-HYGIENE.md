# 降低杀软误报（构建要求）

所有者决定（2026-10-08）：程序不签名发布，先不做误报提交；**尽力降低误报风险是构建的硬要求**。窗口的做法是所有者 2026-11-04
定的：**一个常驻、可见的窗口，不是每个命令一个窗口，也不隐藏**。下面每一条都是要求，能自动检查的由 `scripts/av-hygiene.sh`
在 CI（`Repository checks`）里检查 `client/`、`.github/` 和 `release/`（测试数据和 `go.sum` 除外），违反就失败。

| 要求 | 客户端（`client/`）怎么做 | 自动检查 |
| --- | --- | --- |
| 不隐藏窗口 | `agentdevice run` 一启动就保持**一个**常驻、可见、可最小化的控制台窗口，标题「AgentRouter 设备」（托盘版是 GUI 程序，用 `AllocConsole` 开这个窗口；从终端启动就用那个终端）。窗口开头写「关闭此窗口即断开连接」，之后每个 Agent 做的事一行：时间、哪个对话（凭证里有的话）、命令的第一行、退出码（终端、传文件、连接本机服务也各一行）。非终端命令启动时**不带任何控制台创建标志**（没有 `CREATE_NO_WINDOW`、`CREATE_NEW_CONSOLE`、`DETACHED_PROCESS`），附在这个窗口上，输出走管道交给云端；交互终端走伪控制台（ConPTY），本身没有窗口。关掉窗口就是干净地断开连接（Windows 发 `CTRL_CLOSE_EVENT`，程序关掉 SSH 连接、离开网络再退出） | 是（不出现这三个标志和 `SW_HIDE`；`main.go` 保留常驻窗口）；单元测试（`internal/proc`：不加标志；`internal/app`：活动行） |
| 命令行普通、可见 | 固定的明文运行器（`client/internal/sshd/runner.go`）：命令文字放进只有本人能打开的文件，shell 读完即删再运行（工作区规则「命令行里不传数据」）；PowerShell 命令用 `-NoLogo -NoProfile -Command`（非交互时再加 `-NonInteractive`），不用 `-File`、不改执行策略、不用 `-EncodedCommand`、不解 Base64 | 是 |
| 不挂起、不注入 | 不用 `CREATE_SUSPENDED`、`ResumeThread`、跨进程写内存。Job Object：Go 的 `os/exec` 不支持创建时的 `PROC_THREAD_ATTRIBUTE_JOB_LIST`，所以 shell 启动后立刻放进 Job Object（它读到命令之前）；云端在命令没跑完时断开，整棵进程树结束 | 是 |
| 开机自启只由本人打开 | 默认不自启；只有本人打开：安装程序里勾着的「登录电脑后自动启动」（本人可以取消，它运行 `agentdevice autostart on --no-start`），或本人运行 `agentdevice autostart on`。用系统自己的机制：systemd 用户服务、LaunchAgent、任务计划程序（本人登录时、最低权限、交互令牌、不存密码）；没有 Run 键、启动文件夹、自启插件 | 是（没有 Run 键等）；单元测试（`internal/autostart`） |
| 不要管理员权限 | 没有提权请求；没有清单 = 按调用者权限运行；网络是用户态的（不装驱动） | 是（没有 `requireAdministrator` / `highestAvailable`） |
| 没有自己的工具 | 只执行：不截屏、不读屏幕、不模拟键鼠，没有 MCP | 代码里没有这些功能 |
| 网络 | 只连配置的地址：身份（AgentIdentity / ZITADEL）、算力层、NetBird 的 management 和它下发的 signal / relay，都走 TLS；SSH 只在 NetBird 的虚拟地址上监听，电脑自己的网络上不开端口 | 否 |
| 不加壳、不混淆 | `go build -trimpath -ldflags "-s -w"`，普通剥离，不用 UPX | 是（`ci.yml` 的构建参数；没有 UPX） |
| 二进制对得上源码 | 只在公开的 GitHub 托管 runner 上构建发布产物，日志里打印 SHA-256，发布带 `SHA256SUMS` | 工作流本身 |
| 不从临时目录运行 | 程序本身不从 %TEMP% 运行，也不在那里自更新；`%TEMP%\agentdevice-run\` 只放命令文字文件（只有本人能读、读完即删） | 否 |
| 文件信息齐全 | 安装程序有版本信息（产品名、公司、版本）；程序本身**还没有**版本资源和图标（加的时候用开源的 `.syso` 生成工具，比如 goversioninfo） | 加的时候补检查 |
| 自动更新 | 只提示，不自己替换（#243，[release/README.md](../../release/README.md)）：正式版每天问一次最新版本，有新的就在窗口里写一行和下载地址，由本人装新的安装程序（原地替换）。以后要自己替换时：下载内容用钉住的发布公钥验签，下载并替换在安装目录里完成，不经过 %TEMP% | 代码里没有下载或替换程序的路径（`internal/update` 只读版本号） |
| 安装和卸载 | 安装程序（Inno Setup）按用户安装、不要管理员（`PrivilegesRequired=lowest`）、不加壳；卸载时先关自动启动、按程序路径结束客户端、从账号里移除这台电脑、删掉它的文件夹 | `scripts/av-hygiene.sh` 也查 `release/` |

## 暂缓

- 向 Microsoft、360、火绒等提交误报：所有者 2026-10-08 决定先不做（见 [ADR 0001](../adr/0001-code-signing.md)）。
