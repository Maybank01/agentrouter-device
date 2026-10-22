// AgentRouter sandbox entry (Windows). srt runs this INSIDE the sandbox as the `srt-sandbox` account; it starts the
// real MCP server with the arguments and environment the device chose. srt-win hands its child a fresh profile
// environment (PATH, PATHEXT and the proxy variables only) and runs it through `cmd /c`, so neither the server's
// environment nor arguments with spaces would survive the wrap; this file carries them instead.
//
// argv[2] is a one-shot launch file in the server's scratch folder ({program, args, env, cwd, pipe?}); it may hold
// secret values, so it is deleted as soon as it is read. Batch files (npx.cmd, uvx shims, agentrouter.cmd) go through
// cmd.exe with every argument escaped the way cross-spawn does it (MIT, github.com/moxystudio/node-cross-spawn).
//
// `pipe` (a stdio server, #112): srt-win gives this process an empty stdin by design, so the server's stdin and stdout
// travel over the device's named pipe instead. This side only connects (a client end, as with srt's own pipes); the
// pipe takes one client, so a second connect fails. The server's stderr stays on srt's stderr.
//
// `job` (device #129, {name, exe}): the environment's limits. srt-win runs this process in a job of its own, out of the
// device's reach; the device made a named job with the limits, and this process joins it (`<exe> sandbox job-join
// <name> <pid>`, the device's own program: Node.js cannot) before it starts the server, so the server and everything
// it starts stay inside. When joining fails the server does not start: it must not run without its limits.
'use strict';
const fs = require('fs');
const net = require('net');
const path = require('path');
const { spawn, spawnSync } = require('child_process');

const file = process.argv[2];
let spec;
try {
  spec = JSON.parse(fs.readFileSync(file, 'utf8'));
} catch (e) {
  process.stderr.write(`agentrouter folder confinement: launch file unreadable: ${e.message}\n`);
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

if (spec.job) {
  const joined = spawnSync(spec.job.exe, ['sandbox', 'job-join', spec.job.name, String(process.pid)], {
    stdio: ['ignore', 'pipe', 'pipe'],
    encoding: 'utf8',
  });
  // Only this exact answer is a join (another program exiting 0 is not).
  if (joined.status !== 0 || (joined.stdout || '').trim() !== 'joined') {
    const why = joined.error ? joined.error.message : `${(joined.stderr || '').trim()} (exit ${joined.status})`;
    process.stderr.write(`agentrouter folder confinement: 没能进入资源上限的作业对象，所以没有启动（device #129）：${why}\n`);
    process.exit(124);
  }
}

const env = Object.assign({}, process.env, spec.env || {});
const args = spec.args || [];

function launch(stdio) {
  const options = { stdio, env, cwd: spec.cwd || process.cwd() };
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
    process.stderr.write(`agentrouter folder confinement: ${spec.program}: ${e.message}\n`);
    process.exit(127);
  });
  for (const s of ['SIGINT', 'SIGTERM', 'SIGBREAK']) {
    process.on(s, () => child.kill());
  }
  return child;
}

const status = (code, signal) => (code === null ? (signal ? 1 : 0) : code);

if (spec.pipe) {
  const sock = net.connect(spec.pipe);
  sock.once('error', e => {
    process.stderr.write(`agentrouter folder confinement: stdio pipe: ${e.message}\n`);
    process.exit(125);
  });
  sock.once('connect', () => {
    const child = launch(['pipe', 'pipe', 'inherit']);
    // The device went away (its end closed or broke): the server's stdin ends, as it would on stdio.
    sock.removeAllListeners('error');
    sock.on('error', () => child.stdin.end());
    child.stdin.on('error', () => {});
    sock.pipe(child.stdin);
    child.stdout.pipe(sock);
    // Everything the server wrote reaches the device before this process ends.
    child.on('close', (code, signal) => {
      const done = () => process.exit(status(code, signal));
      if (sock.writableFinished || sock.destroyed) return done();
      sock.once('finish', done);
      sock.once('close', done);
      sock.end();
      setTimeout(done, 5000).unref();
    });
  });
} else {
  const child = launch('inherit');
  child.on('exit', (code, signal) => process.exit(status(code, signal)));
}
