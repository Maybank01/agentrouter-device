#!/usr/bin/env node
// Cross-repository import check (agentteam-deployments docs/development/CONTRACTS.md, rule C6): no repository uses
// another repository's source by path. Another repository is reached only through a pinned package, release asset,
// image or vendored contract copy (Cloud deploy/versions.json). Seconds, no install, no network.
//
//   node scripts/check-repo-imports.mjs            # every tracked file of this repository
//   node scripts/check-repo-imports.mjs --files a,b  # only these (repository-relative) files
//
// It resolves every path a file names for building or importing and fails when the path leaves this repository:
//   - JS/TS import, export … from, require(), import(), vi.mock() specifiers that start with "." or "/";
//   - package.json dependency specs file:/link:/portal: and workspaces; tsconfig extends / references / paths;
//   - Cargo.toml `path = "…"`; go.mod / go.work `replace … => <dir>` and `use <dir>`;
//   - pyproject.toml / requirements*.txt `file://…`, `-e <dir>` and `{ path = "…" }`;
//   - Docker Compose `context:` / `dockerfile:` and Dockerfile `COPY|ADD <src>` paths.
// A line that must name an outside path for a reason other than building (a tool that manages sibling checkouts)
// carries `repo-import-ok: <why>` on the same line or the line above; JSON cannot, so a manifest has no exception.
// Canonical copy: agentteam-deployments scripts/check-repo-imports.mjs; every repository runs a byte-identical copy.
// Justification: (Open source first) dependency-cruiser (MIT, used by Cloud) covers JS/TS module graphs inside one
// repository only and cannot see Cargo, Go, Python, Compose or Dockerfile paths; no maintained tool checks
// cross-repository path references across these ecosystems, so this is a small lexical check.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";

const MARKER = /repo-import-ok:\s*\S/u;
const CODE = /\.(?:[cm]?[jt]sx?)$/u;

/** Pure: does `target` (relative to the file's directory, or absolute) leave the repository? */
export function leaves(root, file, target) {
  if (!target || /^[a-z][a-z0-9+.-]*:\/\//iu.test(target) && !target.startsWith("file://")) return false;
  let path = target.replace(/^file:\/\//u, "").replace(/^(?:file|link|portal):/u, "");
  if (path.startsWith("~")) return true;
  if (!isAbsolute(path) && !path.startsWith(".")) return false;
  // An absolute path counts only when it names a workstation checkout (container paths such as /opt/… or URL paths
  // such as /api/… are not repositories).
  if (isAbsolute(path) && !/(?:^\/home\/|^\/Users\/|^[A-Za-z]:[\\/]|AgentTeam)/u.test(path)) return false;
  const full = isAbsolute(path) ? path : resolve(root, dirname(file), path);
  const rel = relative(root, full);
  return rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel);
}

/** Pure: the outside paths a file names, as [{ line, target, what }]. `text` is the file's content. */
export function outsidePaths(root, file, text) {
  const lines = text.split(/\r?\n/u);
  const hits = [];
  const allowed = (index) => MARKER.test(lines[index] ?? "") || MARKER.test(lines[index - 1] ?? "");
  const add = (index, target, what) => {
    if (leaves(root, file, target) && !allowed(index)) hits.push({ line: index + 1, target, what });
  };
  const each = (regex, what, group = 1) =>
    lines.forEach((line, index) => {
      for (const match of line.matchAll(regex)) add(index, match[group], what);
    });
  const base = file.split("/").pop();
  if (CODE.test(file)) {
    // Static imports are statements: they start a line (fixtures that embed import text in strings do not).
    each(/^\s*\}\s*from\s*["']([^"']+)["']/gu, "import");
    each(/^\s*(?:import|export)\b[^"'`]*?(?:\bfrom\s*)?["']([^"']+)["']/gu, "import");
    each(/(?:\bimport\s*\(\s*|\brequire\s*\(\s*|\b(?:vi|jest)\.mock\s*\(\s*)["'`]([^"'`]+)["'`]/gu, "import");
  } else if (base === "package.json") {
    const json = parse(text);
    for (const field of ["dependencies", "devDependencies", "optionalDependencies", "peerDependencies", "overrides"]) {
      for (const [name, spec] of Object.entries(json?.[field] ?? {})) {
        if (typeof spec === "string" && /^(?:file|link|portal):/u.test(spec)) add(lineOf(lines, name), spec, `${field}.${name}`);
      }
    }
    const workspaces = Array.isArray(json?.workspaces) ? json.workspaces : json?.workspaces?.packages ?? [];
    for (const glob of workspaces) add(lineOf(lines, glob), String(glob).startsWith(".") || isAbsolute(String(glob)) ? glob : `./${glob}`, "workspaces");
  } else if (/^tsconfig.*\.json$|^jsconfig.*\.json$/u.test(base)) {
    const json = parse(text);
    const extend = [json?.extends ?? []].flat();
    for (const target of extend) add(lineOf(lines, target), target, "extends");
    for (const reference of json?.references ?? []) add(lineOf(lines, reference.path), reference.path, "references");
    for (const targets of Object.values(json?.compilerOptions?.paths ?? {})) {
      for (const target of targets) add(lineOf(lines, target), join(json?.compilerOptions?.baseUrl ?? ".", target), "paths");
    }
  } else if (base === "Cargo.toml") {
    each(/\bpath\s*=\s*"([^"]+)"/gu, "path");
  } else if (base === "go.mod" || base === "go.work") {
    each(/=>\s*(\.{1,2}\/\S*|\/\S+)/gu, "replace");
    each(/^\s*(?:use\s+)?(\.{1,2}\/\S*)\s*$/gu, "use");
  } else if (base === "pyproject.toml" || /^requirements.*\.txt$/u.test(base)) {
    each(/file:\/\/([^\s"']+)/gu, "file://");
    each(/^\s*-e\s+(\S+)/gu, "-e");
    each(/\bpath\s*=\s*"([^"]+)"/gu, "path");
  } else if (/(?:^|[-.])compose[^/]*\.ya?ml$|^docker-compose[^/]*\.ya?ml$/u.test(base)) {
    each(/^\s*(?:context|dockerfile):\s*["']?([^"'\s#]+)/gu, "compose");
  } else if (/(?:^|\.)Dockerfile(?:\.[^/]*)?$|^Dockerfile/u.test(base)) {
    lines.forEach((line, index) => {
      const match = /^\s*(?:COPY|ADD)\s+(?:--\S+\s+)*(.+?)\s*$/iu.exec(line);
      if (!match) return;
      const parts = match[1].split(/\s+/u).slice(0, -1);
      for (const part of parts) if (part.startsWith("../")) add(index, part, "COPY");
    });
  }
  return hits.sort((a, b) => a.line - b.line);
}

const parse = (text) => {
  try {
    return JSON.parse(text);
  } catch {
    return null; // tsconfig with comments: fall through without findings rather than guess
  }
};
const lineOf = (lines, needle) => Math.max(0, lines.findIndex((line) => line.includes(String(needle))));

/** Pure: which files to read. */
export const relevant = (file) =>
  CODE.test(file) ||
  /(?:^|\/)(?:package\.json|tsconfig[^/]*\.json|jsconfig[^/]*\.json|Cargo\.toml|go\.mod|go\.work|pyproject\.toml|requirements[^/]*\.txt|(?:docker-)?compose[^/]*\.ya?ml|[^/]*\.compose[^/]*\.ya?ml|Dockerfile[^/]*|[^/]*\.Dockerfile)$/u.test(file);

function trackedFiles(root) {
  const result = spawnSync("git", ["ls-files", "--cached", "--others", "--exclude-standard"], { cwd: root, encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  if (result.status !== 0) throw new Error(`git ls-files failed in ${root}: ${result.stderr.trim()}`);
  return result.stdout.split("\n").filter(Boolean);
}

function main(argv) {
  const started = Date.now();
  const root = spawnSync("git", ["rev-parse", "--show-toplevel"], { encoding: "utf8" }).stdout.trim() || process.cwd();
  const filesArg = argv.includes("--files") ? (argv[argv.indexOf("--files") + 1] ?? "") : null;
  const candidates = (filesArg === null ? trackedFiles(root) : filesArg.split(",").filter(Boolean))
    .filter((file) => relevant(file) && !/(?:^|\/)(?:node_modules|vendor|third_party|dist|build|target|\.next)\//u.test(file));
  const problems = [];
  let read = 0;
  for (const file of candidates) {
    const full = join(root, file);
    if (!existsSync(full) || !statSync(full).isFile() || statSync(full).size > 2 * 1024 * 1024) continue;
    read += 1;
    for (const hit of outsidePaths(root, file, readFileSync(full, "utf8"))) problems.push(`${file}:${hit.line}: ${hit.what} reaches outside this repository: ${hit.target}`);
  }
  const seconds = ((Date.now() - started) / 1000).toFixed(2);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`check-repo-imports: ${problem}`);
    console.error(
      `check-repo-imports: ${problems.length} path(s) into another repository (${read} files, ${seconds}s). Use the provider's ` +
        "pinned package, release asset or vendored contract (agentteam-deployments docs/development/CONTRACTS.md C4, C6).",
    );
    return 1;
  }
  console.log(`check-repo-imports: no path leaves this repository (${read} files, ${seconds}s)`);
  return 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) process.exit(main(process.argv.slice(2)));
