#!/usr/bin/env node
// Runs the native `agentrouter` binary from the platform package npm installed next to this one
// (the esbuild pattern: one optional dependency per OS/CPU, npm keeps only the matching one).
// npm skips an optional dependency it could not fetch without saying so (for example a few minutes
// after a release, before the registry serves it everywhere), and npx then keeps that install. So if
// the binary is missing on a supported system, install exactly that package version next to this one
// once, from the same registry. No install scripts.
"use strict";

const { spawn, spawnSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const pkg = `@agentrouter-top/cli-${process.platform}-${process.arch}`;
const exe = process.platform === "win32" ? "agentrouter.exe" : "agentrouter";
const own = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"));
const version = own.optionalDependencies?.[pkg];
const fallback = path.join(__dirname, "..", ".platform");

function find() {
  for (const paths of [undefined, [fallback]]) {
    try {
      return require.resolve(`${pkg}/bin/${exe}`, paths ? { paths } : undefined);
    } catch {
      // not here
    }
  }
  return null;
}

let binary = find();
if (!binary && version) {
  console.error(`第一次运行：补装 ${pkg}@${version} ……`);
  fs.mkdirSync(fallback, { recursive: true });
  const npm = process.platform === "win32" ? "npm.cmd" : "npm";
  const r = spawnSync(
    `${npm} install --no-save --no-package-lock --no-audit --no-fund --no-update-notifier --loglevel=error --prefix "${fallback}" ${pkg}@${version}`,
    { stdio: ["ignore", "ignore", "inherit"], shell: true },
  );
  if (r.status === 0) binary = find();
}
if (!binary) {
  console.error(
    version
      ? `没能装上 ${pkg}@${version}。请检查网络和 npm 源后重试；仍然不行请到 https://github.com/Maybank01/agentrouter-device/issues 反馈。`
      : `AgentRouter 小助手还不支持这个系统（${process.platform}-${process.arch}）。`,
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
