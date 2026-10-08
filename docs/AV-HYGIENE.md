# 降低杀软误报（构建要求）

所有者决定（2026-10-08）：程序不签名发布，先不做误报提交；**尽力降低误报风险是构建的硬要求**。下面每一条都是要求，能自动检查的由 `scripts/av-hygiene.sh` 在 CI 里检查，违反就失败。

| 要求 | 怎么做到 | 自动检查 |
| --- | --- | --- |
| 命令行普通、可见 | `powershell -NoLogo -NoProfile -NonInteractive -InputFormat None -Command "<命令>"`（Linux/macOS 是 `/bin/sh -c`）；不用 `-EncodedCommand`，不解 Base64，不改执行策略 | 是 |
| 不隐藏 shell 窗口 | 不用 `CREATE_NO_WINDOW` / `WindowStyle Hidden`：桌面版里每个命令有自己可见的控制台窗口，命令行版共用当前终端 | 是 |
| 不挂起、不注入 | 普通 CreateProcess 后放进 Job Object；不用 `CREATE_SUSPENDED`、`ResumeThread`、跨进程写内存 | 是 |
| 先同意再执行 | 本机确认框在任何 shell 启动之前；只读级别从不启动 shell | 单元测试（`crates/device-core/tests`） |
| 安装位置稳定 | 安装包按用户装到固定目录（Tauri NSIS `currentUser`），或按机器装到 Program Files；绝不从 %TEMP% 运行，不自解压到临时目录，不在临时目录里自更新 | 源码里不出现 `temp_dir` / `%TEMP%`（测试除外） |
| 文件信息齐全 | 版本资源（公司、产品名、版本、版权、说明）、图标、清单（`asInvoker`，不要求管理员权限） | 清单是 `asInvoker`；CI 读取 exe 的版本信息，缺了就失败 |
| 不加壳、不混淆 | 不用 UPX 等加壳；release 只做普通的符号剥离（`strip = true`） | 是 |
| 二进制对得上源码 | 只在公开的 GitHub 托管 runner 上构建发布产物（`--locked`），日志里打印 SHA-256 | 工作流本身 |
| 网络只连网关域名 | 只连配置的网关域名，走 TLS；不连裸 IP（本机 127.0.0.1/localhost 只用于本地测试） | 单元测试（`net::gateway_allowed`） |
| 不开机自启 | 没有 Run 键、启动文件夹或自启插件；以后要加，也只能由用户在界面里打开 | 是 |
| 自动更新 | 还没有。以后加时：下载内容用 Ed25519 签名校验，下载并替换在安装目录里完成，不经过 %TEMP% | 加的时候补检查 |

## 暂缓

- 向 Microsoft、360、火绒等提交误报：所有者 2026-10-08 决定先不做（见 [SIGNING.md](SIGNING.md)）。
