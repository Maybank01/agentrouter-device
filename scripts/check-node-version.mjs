#!/usr/bin/env node
// One Node version (AgentHost#278: the runtime image ran Node 24.15, whose zstd decoder reads only the first frame,
// while the workstation and CI ran 24.22 - what CI tests must be what ships). The source is `.node-version`
// (x.y.z). This gate fails when anything else names another Node:
//   - a workflow `node-version:` literal (use `node-version-file: .node-version`) or a node-version-file that is not it;
//   - a Dockerfile `node:<tag>` image or `ARG NODE_VERSION=` default whose version differs, or a floating tag (`node:24`);
//   - a package.json `engines.node` floor above it.
// Images built FROM another image (runtime-image) assert the version at build time instead: see runtime-image/Dockerfile.
// Seconds, no install, no network. Copy byte-identical across repositories with a Dockerfile or a CI Node.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const root = process.argv[2] ?? ".";
const SKIP = new Set(["node_modules", ".git", "vendor", "dist"]);
const walk = (dir, out = []) => {
  for (const e of readdirSync(join(root, dir), { withFileTypes: true })) {
    if (SKIP.has(e.name)) continue;
    const rel = dir ? `${dir}/${e.name}` : e.name;
    if (e.isDirectory()) walk(rel, out);
    else out.push(rel);
  }
  return out;
};
const lines = (f) => readFileSync(join(root, f), "utf8").split("\n").map((text, i) => ({ text, n: i + 1 }));
const cmp = (a, b) => { const x = a.split(".").map(Number), y = b.split(".").map(Number); for (let i = 0; i < 3; i++) if ((x[i] ?? 0) !== (y[i] ?? 0)) return x[i] - y[i]; return 0; };

const vf = join(root, ".node-version");
if (!existsSync(vf)) { console.error("check-node-version: .node-version is missing (one line, x.y.z)"); process.exit(1); }
const pin = readFileSync(vf, "utf8").trim();
if (!/^\d+\.\d+\.\d+$/u.test(pin)) { console.error(`check-node-version: .node-version must be x.y.z, got "${pin}"`); process.exit(1); }

const bad = [];
for (const f of walk("")) {
  if (/^\.github\/workflows\/.+\.ya?ml$/u.test(f)) {
    for (const { text, n } of lines(f)) {
      if (/^\s*#/u.test(text)) continue;
      if (/\bnode-version\s*:/u.test(text)) bad.push(`${f}:${n} node-version literal; use "node-version-file: .node-version": ${text.trim()}`);
      const m = /\bnode-version-file\s*:\s*["']?([^\s"']+)/u.exec(text);
      if (m && m[1] !== ".node-version") bad.push(`${f}:${n} node-version-file is ${m[1]}, must be .node-version`);
    }
  } else if (/(^|\/)(Dockerfile[^/]*|[^/]*\.Dockerfile)$/u.test(f)) {
    for (const { text, n } of lines(f)) {
      if (/^\s*#/u.test(text)) continue;
      for (const m of text.matchAll(/\bnode:(\d[\w.]*)/gu)) {
        const v = m[1].split("-")[0];
        if (v !== pin) bad.push(`${f}:${n} Node image ${m[1]} != .node-version ${pin}${/^\d+(\.\d+)?$/u.test(v) ? " (floating tag)" : ""}`);
      }
      const a = /^\s*ARG\s+NODE_VERSION=(\S+)/u.exec(text);
      if (a && a[1] !== pin) bad.push(`${f}:${n} ARG NODE_VERSION=${a[1]} != .node-version ${pin}`);
    }
  } else if (/(^|\/)package\.json$/u.test(f)) {
    let eng;
    try { eng = JSON.parse(readFileSync(join(root, f), "utf8")).engines?.node; } catch { continue; }
    const m = eng && /^>=\s*(\d+\.\d+\.\d+)$/u.exec(eng);
    if (eng && !m && eng !== pin) bad.push(`${f} engines.node "${eng}" is not ">=x.y.z" or ${pin}`);
    if (m && cmp(m[1], pin) > 0) bad.push(`${f} engines.node floor ${m[1]} is above .node-version ${pin}`);
  }
}
if (existsSync(join(root, "runtime-image/Dockerfile"))) {
  const d = readFileSync(join(root, "runtime-image/Dockerfile"), "utf8");
  const wf = existsSync(join(root, ".github/workflows/images.yml")) ? readFileSync(join(root, ".github/workflows/images.yml"), "utf8") : "";
  if (!/ARG NODE_VERSION\b/u.test(d) || !/process\.versions\.node/u.test(d)) bad.push("runtime-image/Dockerfile must assert process.versions.node equals ARG NODE_VERSION (the base is another repository's image)");
  if (!/NODE_VERSION=/u.test(wf)) bad.push(".github/workflows/images.yml must pass --build-arg NODE_VERSION=<.node-version> to the runtime image build");
}
if (bad.length) {
  console.error(`check-node-version: ${bad.length} place(s) differ from .node-version ${pin}:\n  ${bad.join("\n  ")}\nChange .node-version and every place together; what CI tests must be what ships.`);
  process.exit(1);
}
console.log(`check-node-version: ok (${pin})`);
