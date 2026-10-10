# agentrouter

AgentRouter 小助手：让你在 AgentRouter 云端对话里用的 AI，在你电脑上链接的文件夹里读代码、改文件、跑测试。

```sh
cd 你的项目文件夹
npx agentrouter@next link
```

第一次运行会显示一个配对码和网址，在网页上确认后这台电脑就链接好了；之后在别的文件夹里运行会把那个文件夹也加进来。小助手在前台运行，按 Ctrl+C 断开（它起的命令都会结束）。

- AI 只能碰你链接的文件夹，在里面干活不用每次问你；明显越界的命令（改系统、碰密码和密钥、提权、全局安装）会被直接拦下。
- 每轮改动之前自动打检查点：`npx agentrouter@next undo` 撤销最近一轮，`undo --list` 看全部。
- 其他命令：`status`、`unlink [文件夹]`、`audit verify`、`help`。

`next` 是测试版，连的是 AgentRouter 的 Dev 环境。目前只有 Windows x64。

源代码（Apache-2.0）：https://github.com/Maybank01/agentrouter-device
