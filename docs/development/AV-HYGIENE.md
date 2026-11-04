# 降低杀软误报（构建要求）

所有者决定（2026-10-08）：程序不签名发布，先不做误报提交；**尽力降低误报风险是构建的硬要求**。下面每一条都是要求，能自动检查的由 `scripts/av-hygiene.sh` 在 CI 里检查，违反就失败。

| 要求 | 怎么做到 | 自动检查 |
| --- | --- | --- |
| 命令行普通、可见 | `pwsh/powershell -NoLogo -NoProfile -NonInteractive -Command "<固定的运行器>"`（Linux/macOS 是 `/bin/sh -c "<固定的运行器>"`）：命令文字本身从不放进命令行（工作区规则「命令行里不传数据」，#99），一次性命令走 stdin，终端里的命令放进只有本人能打开的文件、读完即删；运行器是明文、固定不变（`crates/device-core/src/command_text.rs`）。不用 `-EncodedCommand`，不解 Base64，不改执行策略，不用 `-File` 运行脚本 | 是 |
| 不隐藏 shell 窗口 | 不用 `CREATE_NO_WINDOW` / `WindowStyle Hidden`：桌面版里每个命令有自己可见的控制台窗口，命令行版共用当前终端 | 是 |
| 不挂起、不注入 | 创建时就用 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 放进 Job Object；不用 `CREATE_SUSPENDED`、`ResumeThread`、跨进程写内存；文件夹级别的命令用本人令牌的受限、低完整性副本启动（只降权，不提权） | 是 |
| 先同意再执行 | 本机确认框在任何 shell 启动之前；只读级别从不启动 shell | 单元测试（`crates/device-core/tests`） |
| 安装位置稳定 | 安装包是 MSI（Windows Installer，按机器装到 Program Files，不是自解压程序）；不自带 WebView2 引导下载（Windows 10/11 已自带）；绝不从 %TEMP% 运行，不在临时目录里自更新 | 源码里不出现 `temp_dir` / `%TEMP%`（测试除外） |
| 文件信息齐全 | 版本资源（公司、产品名、版本、版权、说明）、图标、清单（`asInvoker`，不要求管理员权限） | 清单是 `asInvoker`；CI 读取 exe 的版本信息，缺了就失败 |
| 不加壳、不混淆 | 不用 UPX 等加壳；release 只做普通的符号剥离（`strip = true`） | 是 |
| 二进制对得上源码 | 只在公开的 GitHub 托管 runner 上构建发布产物（`--locked`），日志里打印 SHA-256 | 工作流本身 |
| 网络只连网关域名 | 只连配置的网关域名，走 TLS；不连裸 IP（本机 127.0.0.1/localhost 只用于本地测试）。这条说的是程序自己的连接；隔离环境里跑的命令另算：在 macOS 和 Windows 上它们能连本机端口，包括本人自己开的本地服务（数据库、本地管理页），和本人自己的终端一样；Linux 上命令在自己的网络里，连不到本人的本地服务。这个差异是有意保留的（2026-10-22，Cloud #2383，见 [HOSTD-MCP.md](../HOSTD-MCP.md) §6.1「Local ports」）：能在电脑上跑命令的只有本人、本人的 Agent 和授权（输入信任 `delegated` 及以上）。**唯一例外**（#116，产品要求本人什么都不用装）：第一次准备隔离环境时，下载钉死版本的 Node.js 和 uv 的官方便携包（Linux 还有隔离要用的 ripgrep 官方静态包，以及 socat 和 bubblewrap 的静态包，#119、#122；这两个没有上游维护的静态包，由本仓库的 `.github/workflows/socat.yml`、`bwrap.yml` 从官方源码构建并连同源码发布）——只从项目自己的发布地址、它的官方包仓库和这些地址的公认国内镜像（#154：nodejs.org / npmmirror；releases.astral.sh / github.com / PyPI 上 uv 自己发布的 wheel 及其清华、华为镜像；ripgrep 是 VS Code 的构建，来自 registry.npmjs.org / npmmirror；socat 和 bubblewrap 来自 github.com 或本仓库发到 npm 的同一批文件 `@agentrouter-top/sandbox-tools-linux` 及其 npmmirror 镜像），只走 https；SHA-256 写在源码里（Node 的取自其发布签名过的 SHASUMS256.txt），对不上就删掉、不解开；在程序自己的进程里下载（不调用 curl、PowerShell 或安装程序），直接放进数据文件夹旁边的 `-shared\runtimes`，不经过 %TEMP%。Node.js 的 Windows 程序带 OpenJS Foundation 的签名。**第二个例外**（Cloud #2740）：目录里的本机工具按云端发来的锁文件安装——用设备自己的 Node.js 和 uv 运行 `npm ci --ignore-scripts` / `uv pip sync --require-hashes`（不运行包里的代码），只从锁里写的官方包仓库（registry.npmjs.org、pypi.org）和它们的公认镜像（npmmirror；清华、阿里云 PyPI 镜像；uv 管理的 Python 来自 uv 自己的地址或 npmmirror 的镜像，uv 用内置的 SHA-256 校验）下载，每个文件都对锁里的校验值，对不上就不装；装到 `-shared\mcp-packages`，不经过 %TEMP% | 单元测试（`net::gateway_allowed`；`runtimes::downloads_come_from_official_hosts_and_their_mirrors_only`、校验值格式） |
| 不自己截屏、不操作界面 | hostd 自己从不截屏、不读屏幕、不模拟键鼠；只有本人在这台电脑上同意过「能看到并操作屏幕」的本机工具（`agentrouter mcp add … --display`，或云端请求后本人答 y）才会这样做，它以本人的权限运行、照样受环境的资源上限和审计约束（#97，见 [HOSTD-MCP.md](../HOSTD-MCP.md) §6.7）；没有隐藏窗口 | 单元测试（`grants::tests`；aggregator 测试：云端要屏幕时没有本人同意就拒绝） |
| 开机自启只由本人打开 | 默认不自启。只有本人运行 `agentrouter service install` 才打开（#94），用系统自己的机制：Linux 的 systemd 用户服务、macOS 的登录项 LaunchAgent、Windows 的任务计划程序（本人登录时、本人的权限、最低权限级别、不存密码、窗口可见）；系统里的定义只有 `agentrouter service run`，文件夹在数据文件夹的 `service.json` 里；只在崩溃后重启，被网关拒绝就停下；`agentrouter service uninstall` 删掉启动项。没有 Run 键、启动文件夹或自启插件；以后界面里的开关也走这条命令 | 是（没有 Run 键等）；单元测试（`service::tests`：定义的转义、只运行 `service run`、最低权限） |
| 自动更新 | 还没有。以后加时：下载内容用 Ed25519 签名校验，下载并替换在安装目录里完成，不经过 %TEMP% | 加的时候补检查 |

## Go 客户端（`client/`，base v1，2026-11-04 起）

所有者 2026-11-04 决定了 Go 客户端的窗口做法：**一个常驻、可见的窗口，不是每个命令一个窗口，也不隐藏**。上表的要求照样适用；下面是 Go 客户端怎么做到的。`scripts/av-hygiene.sh` 同样检查 `client/`（测试数据和 `go.sum` 除外）。Rust `hostd` 删除之前，上表仍描述它。

| 要求 | Go 客户端怎么做 | 自动检查 |
| --- | --- | --- |
| 不隐藏窗口 | `agentdevice run` 一启动就保持**一个**常驻、可见、可最小化的控制台窗口，标题「AgentRouter 设备」（托盘版是 GUI 程序，用 `AllocConsole` 开这个窗口；从终端启动就用那个终端）。窗口开头写「关闭此窗口即断开连接」，之后每个 Agent 做的事一行：时间、哪个对话（凭证里有的话）、命令的第一行、退出码（终端、传文件、连接本机服务也各一行）。非终端命令启动时**不带任何控制台创建标志**（没有 `CREATE_NO_WINDOW`、`CREATE_NEW_CONSOLE`、`DETACHED_PROCESS`），附在这个窗口上，输出走管道交给云端；交互终端走伪控制台（ConPTY），本身没有窗口。关掉窗口就是干净地断开连接（Windows 发 `CTRL_CLOSE_EVENT`，程序关掉 SSH 连接、离开网络再退出） | 是（`client/` 里不出现这三个标志）；单元测试（`internal/proc`：不加标志；`internal/app`：活动行） |
| 命令行普通、可见 | 和上表同一套固定的明文运行器（`client/internal/sshd/runner.go`，照搬 `command_text.rs`）：命令文字放进只有本人能打开的文件，shell 读完即删再运行；PowerShell 用 `-Command`，不用 `-File`、不改执行策略、不编码 | 是 |
| 不挂起、不注入 | 不用 `CREATE_SUSPENDED`、`ResumeThread`、跨进程写内存。Job Object：Go 的 `os/exec` 不支持创建时的 `PROC_THREAD_ATTRIBUTE_JOB_LIST`，所以 shell 启动后立刻放进 Job Object（它读到命令之前）；云端在命令没跑完时断开，整棵进程树结束 | 是（上表同样的模式） |
| 开机自启只由本人打开 | 默认不自启；只有本人运行 `agentdevice autostart on`。用系统自己的机制：systemd 用户服务、LaunchAgent、任务计划程序（本人登录时、最低权限、交互令牌、不存密码）；没有 Run 键、启动文件夹 | 是（没有 Run 键等）；单元测试（`internal/autostart`） |
| 没有自己的工具 | 只执行：不截屏、不读屏幕、不模拟键鼠，没有 MCP | 代码里没有这些功能 |
| 网络 | 只连配置的地址：NewAPI（身份）、算力层、NetBird 的 management 和它下发的 signal / relay，都走 TLS；SSH 只在 NetBird 的虚拟地址上监听，电脑自己的网络上不开端口 | 否 |
| 不加壳 | `go build -trimpath -ldflags "-s -w"`，普通剥离，不用 UPX | 是 |
| 不从临时目录运行 | 程序本身不从 %TEMP% 运行，也不在那里自更新；`%TEMP%\agentdevice-run\` 只放命令文字文件（只有本人能读、读完即删） | 否 |
| 文件信息齐全 | **还没有**：Go 程序缺版本资源和图标（加的时候用开源的 `.syso` 生成工具，比如 goversioninfo）；没有清单 = 按调用者权限运行，不要求管理员 | 加的时候补检查 |

## 暂缓

- 向 Microsoft、360、火绒等提交误报：所有者 2026-10-08 决定先不做（见 [ADR 0001](../adr/0001-code-signing.md)）。
