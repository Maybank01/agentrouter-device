// AgentRouter sandbox entry (Windows). srt runs this INSIDE the sandbox as the `srt-sandbox` account; it starts the
// real MCP server with the arguments and environment the device chose. srt-win hands its child a fresh profile
// environment (PATH, PATHEXT and the proxy variables only) and runs it through `cmd /c`, so neither the server's
// environment nor arguments with spaces would survive the wrap; this file carries them instead.
//
// argv[2] is a one-shot launch file in the server's scratch folder ({program, args, env, cwd}); it may hold secret
// values, so it is deleted as soon as it is read. Batch files (npx.cmd, uvx shims, agentrouter.cmd) go through
// cmd.exe with every argument escaped the way cross-spawn does it (MIT, github.com/moxystudio/node-cross-spawn).
'use strict';
const fs = require('fs');
const path = require('path');
const { spawn } = require('child_process');

const file = process.argv[2];
let spec;
try {
  spec = JSON.parse(fs.readFileSync(file, 'utf8'));
} catch (e) {
  process.stderr.write(`agentrouter sandbox: launch file unreadable: ${e.message}\n`);
  process.exit(126);
}
try {
  fs.unlinkSync(file);
} catch {
  // Already gone: nothing to hide.
}

const meta = /([()\][%!^"`<>&|;, *?])/g;
function escapeCommand(arg) {
  return arg.replace(meta, '^$1');
}
function escapeArgument(arg, doubleEscape) {
  arg = `${arg}`;
  arg = arg.replace(/(?=(\\+?)?)\1"/g, '$1$1\\"');
  arg = arg.replace(/(?=(\\+?)?)\1$/, '$1$1');
  arg = `"${arg}"`;
  arg = arg.replace(meta, '^$1');
  if (doubleEscape) arg = arg.replace(meta, '^$1');
  return arg;
}

const env = Object.assign({}, process.env, spec.env || {});
const args = spec.args || [];
const options = { stdio: 'inherit', env, cwd: spec.cwd || process.cwd() };
let child;
if (/\.(cmd|bat)$/i.test(spec.program)) {
  const double = /node_modules[\\/]\.bin[\\/][^\\/]+\.cmd$/i.test(spec.program);
  const line = [escapeCommand(path.normalize(spec.program)), ...args.map(a => escapeArgument(a, double))].join(' ');
  child = spawn(env.ComSpec || 'cmd.exe', ['/d', '/s', '/c', `"${line}"`], {
    ...options,
    windowsVerbatimArguments: true,
  });
} else {
  child = spawn(spec.program, args, options);
}
child.on('error', e => {
  process.stderr.write(`agentrouter sandbox: ${spec.program}: ${e.message}\n`);
  process.exit(127);
});
child.on('exit', (code, signal) => process.exit(code === null ? (signal ? 1 : 0) : code));
for (const s of ['SIGINT', 'SIGTERM', 'SIGBREAK']) {
  process.on(s, () => child.kill());
}
