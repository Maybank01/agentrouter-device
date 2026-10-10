#!/usr/bin/env node
// Runs the native `agentrouter` binary from the platform package npm installed next to this one
// (the esbuild pattern: one optional dependency per OS/CPU, npm keeps only the matching one).
// No install scripts, no downloads: the binary is whatever the registry served for that package.
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");

const pkg = `@agentrouter-top/cli-${process.platform}-${process.arch}`;
const exe = process.platform === "win32" ? "agentrouter.exe" : "agentrouter";

let binary;
try {
  binary = require.resolve(`${pkg}/bin/${exe}`);
} catch {
  console.error(
    `AgentRouter 小助手还不支持这个系统（${process.platform}-${process.arch}），或者安装时跳过了可选依赖 ${pkg}。\n` +
      "请不要用 --no-optional / --omit=optional 安装；仍然不行请到 https://github.com/Maybank01/agentrouter-device/issues 反馈。",
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit", windowsHide: false });
// Ctrl+C / Ctrl+Break reach the helper directly (same console / process group); it stops its jobs
// and disconnects. The wrapper only waits for it, so the prompt comes back after it has finished.
for (const signal of ["SIGINT", "SIGBREAK", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (signal === "SIGTERM" || signal === "SIGHUP") child.kill(signal);
  });
}
child.on("error", (err) => {
  console.error(`没能启动 ${path.basename(binary)}：${err.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  process.exit(code ?? (signal ? 1 : 0));
});
