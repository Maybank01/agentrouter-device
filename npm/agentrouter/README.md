# @agentrouter-top/cli

AgentRouter 小助手：把你的电脑链接到 AgentRouter，云端对话里的 AI 通过电脑上默认装好的本机工具（MCP 服务：文件、Git、终端、代码理解，打开屏幕功能后加 Windows 操作）读代码、改文件、跑测试。

```sh
cd 你的项目文件夹
npx @agentrouter-top/cli@next link
```

也可以全局安装，之后直接用 `agentrouter` 命令：

```sh
npm i -g @agentrouter-top/cli@next
agentrouter link
```

第一次运行会显示一个配对码和网址，在网页上确认后这台电脑就链接好了；之后在别的文件夹里运行会把那个文件夹也加进来。小助手在前台运行，按 Ctrl+C 断开（本机工具都会停下）。

- 小助手只做链接：本机工具是现成的开源 MCP 服务，由云端对话装上和启动，干活不用每次问你；文件服务只看链接的文件夹，终端服务（DesktopCommander）到得了这台电脑的各个位置。AI 操作时屏幕上方有提示条，锁屏和 UAC 上不操作。
- `agentrouter screen on`：屏幕功能（Windows），默认装上 Windows-MCP，AI 可以截图、操作鼠标键盘、打开和关闭程序。
- 会改文件的调用之前自动打检查点：`agentrouter undo`（没全局安装就用 `npx @agentrouter-top/cli@next undo`） 撤销最近一轮，`undo --list` 看全部。
- 其他命令：`status`、`mcp on|off`、`unlink [文件夹]`、`audit verify`、`help`。

`next` 是测试版，连的是 AgentRouter 的 Dev 环境。目前只有 Windows x64，需要 Node.js 18 或更新。

源代码（Apache-2.0）：https://github.com/Maybank01/agentrouter-device
