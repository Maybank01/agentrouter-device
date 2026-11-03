#!/usr/bin/env node
// The documentation framework's check (owner 2026-11-03 「做好文档的记录框架，不要再让各个文档散落在各处」). Same
// rules and code as agentrouter-cloud scripts/check-docs.mjs (Cloud docs/development/DOCS.md); only the block
// "Repository settings" below differs between repositories. Seconds, no install.
//
//   node scripts/check-docs.mjs                  places, map and links (exit 1 with every problem)
//   node scripts/check-docs.mjs --no-links       places and map only
//   node scripts/check-docs.mjs --stale [days]   report (exit 0): reference docs whose `Last verified` is older than
//                                                <days> (default 30) and the docs still typed `pending`;
//                                                `--summary <file>` also appends it there
//
// Every Markdown file in the repository (git ls-files, untracked included) is either a row of the map in
// docs/README.md ("## Documents": path, type, module, what, last verified) or matched by its allowlist ("## Not
// documentation": Markdown read at runtime or by tools). A row's type fixes where the file may live; a `reference` doc
// starts with `Last verified: YYYY-MM-DD against <commit>` and the map carries the same date. Rows of docs/ARCHIVE.md
// ("## Removed") name removed paths, so none of them may exist again. Links: lychee, offline (repository files and
// #fragments), over every mapped doc; when lychee cannot be had the built-in relative-link check runs instead and
// says so.
// Justification: the map/places rules are ours (no standard tool knows our doc types); link checking is lychee.
import { spawnSync } from "node:child_process";
import { appendFileSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, posix, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { lycheeBinary } from "./lychee.mjs";

// ---- Repository settings ------------------------------------------------------------------------------------------
/** Root files of type `entry`. */
const ENTRY_FILES = ["README.md", "SECURITY.md"];
/** Types this repository adds to the shared ones: { name: { where, ok(path) } }. */
const EXTRA_TYPES = {
  // The link protocol and hostd design. It stays at its old path: Cloud code, docs and the vendored runner
  // (deploy/coolify/runtime/vendor/) cite `docs/HOSTD-MCP.md` and its section numbers over a hundred times.
  protocol: { where: "docs/HOSTD-MCP.md (cited by path from Cloud code and the vendored runner)", ok: (p) => p === "docs/HOSTD-MCP.md" },
};
/** Module ids the Module cell must use, or null when the cell is free text (it must not be empty). */
const MODULE_IDS = null;
// ---- End of repository settings -----------------------------------------------------------------------------------

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const INDEX = "docs/README.md";
export const ARCHIVE = "docs/ARCHIVE.md";
export const MODULE_README_MAX_LINES = 40;

const ENTRY = new Set(ENTRY_FILES);
/** Where each doc type may live (Cloud docs/development/DOCS.md "Types"). `pending` = awaiting its cleanup verdict. */
export const TYPES = Object.freeze({
  index: { where: "docs/README.md or docs/ARCHIVE.md", ok: (p) => p === INDEX || p === ARCHIVE },
  entry: { where: `the repository root (${ENTRY_FILES.join(", ")})`, ok: (p) => ENTRY.has(p) },
  product: { where: "docs/product/", ok: (p) => p.startsWith("docs/product/") },
  development: { where: "docs/development/", ok: (p) => p.startsWith("docs/development/") },
  reference: { where: "docs/reference/", ok: (p) => p.startsWith("docs/reference/") },
  operations: { where: "docs/operations/", ok: (p) => p.startsWith("docs/operations/") },
  adr: { where: "docs/adr/NNNN-name.md", ok: (p) => /^docs\/adr\/\d{4}-[a-z0-9-]+\.md$/u.test(p) },
  module: { where: `a README.md, AGENTS.md or CLAUDE.md of a module or package directory outside docs/ (at most ${MODULE_README_MAX_LINES} lines)`, ok: (p) => !p.startsWith("docs/") && p.includes("/") && /\/(?:README|AGENTS|CLAUDE)\.md$/u.test(p) },
  pending: { where: "anywhere (until its cleanup verdict lands)", ok: () => true },
  ...EXTRA_TYPES,
});
export const LAST_VERIFIED = /^Last verified: (\d{4}-\d{2}-\d{2}) against ([0-9a-f]{7,40})\b/mu;

/** A glob from the allowlist as a RegExp: `**` any depth, `*` and `?` inside one segment. */
export function globToRegExp(glob) {
  let out = "";
  for (let i = 0; i < glob.length; i += 1) {
    const c = glob[i];
    if (c === "*" && glob[i + 1] === "*") { out += glob[i + 2] === "/" ? "(?:.*/)?" : ".*"; i += glob[i + 2] === "/" ? 2 : 1; }
    else if (c === "*") out += "[^/]*";
    else if (c === "?") out += "[^/]";
    else out += c.replace(/[.+^${}()|[\]\\]/gu, "\\$&");
  }
  return new RegExp(`^${out}$`, "u");
}

const cells = (line) => line.trim().replace(/^\|/u, "").replace(/\|$/u, "").split("|").map((cell) => cell.trim());
const unticked = (cell) => cell.replace(/^`(.*)`$/u, "$1");

/** Rows of the first table under the heading `## <title>` of a Markdown text: arrays of cells, header dropped. */
export function tableUnder(text, title) {
  const lines = text.split(/\r?\n/u);
  const start = lines.findIndex((line) => line.trim() === `## ${title}`);
  if (start < 0) return null;
  const rows = [];
  for (let i = start + 1; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^## /u.test(line)) break;
    if (line.trim().startsWith("|")) rows.push({ cells: cells(line), line: i + 1 });
    else if (rows.length) break;
  }
  return rows.slice(2); // header + separator
}

/** The map and the allowlist of docs/README.md. */
export function parseIndex(text) {
  const problems = [];
  const docs = [];
  const documents = tableUnder(text, "Documents");
  if (!documents) problems.push(`${INDEX}: no "## Documents" table`);
  for (const { cells: row, line } of documents ?? []) {
    const at = `${INDEX}:${line}`;
    const link = /^\[`?([^\]`]+)`?\]\(([^)\s]+)\)$/u.exec(row[0] ?? "");
    if (!link) { problems.push(`${at}: the Path cell must be a link [path](href), found "${row[0]}"`); continue; }
    const [, path, href] = link;
    const target = posix.normalize(posix.join(posix.dirname(INDEX), decodeURIComponent(href.split("#")[0])));
    if (target !== path) problems.push(`${at}: link text ${path} and target ${target} differ`);
    docs.push({ path, type: row[1] ?? "", module: unticked(row[2] ?? ""), what: row[3] ?? "", verified: row[4] ?? "", at });
  }
  const allow = [];
  const allowRows = tableUnder(text, "Not documentation");
  if (!allowRows) problems.push(`${INDEX}: no "## Not documentation" table (the allowlist)`);
  for (const { cells: row, line } of allowRows ?? []) {
    const pattern = unticked(row[0] ?? "");
    if (!pattern) continue;
    allow.push({ pattern, re: globToRegExp(pattern), what: row[1] ?? "", at: `${INDEX}:${line}` });
  }
  return { docs, allow, problems };
}

/** Removed paths named by docs/ARCHIVE.md ("## Removed" table, first cell `path`). */
export function parseArchive(text) {
  // The first `code` span of the Path cell is the removed path (a row may name more files after it).
  return (tableUnder(text, "Removed") ?? []).map(({ cells: row, line }) => ({ path: /`([^`]+)`/u.exec(row[0] ?? "")?.[1] ?? "", at: `${ARCHIVE}:${line}` })).filter((row) => row.path);
}

/**
 * Pure: the problems of a repository's documentation. `files` = every Markdown path; `index`/`archive` = parsed;
 * `read(path)` = text; `modules` = module ids or null (free text). Returns { errors, pending }.
 */
export function checkDocs({ files, index, archive = [], read, modules = MODULE_IDS }) {
  const errors = [...index.problems];
  const fileSet = new Set(files);
  const byPath = new Map();
  for (const doc of index.docs) {
    if (byPath.has(doc.path)) errors.push(`${doc.at}: ${doc.path} is listed twice`);
    byPath.set(doc.path, doc);
  }
  const allowedBy = (path) => index.allow.find((entry) => entry.re.test(path));
  for (const file of files) {
    const entry = allowedBy(file);
    if (entry && byPath.has(file)) errors.push(`${byPath.get(file).at}: ${file} is both a mapped doc and allowlisted (${entry.pattern}); keep one`);
    if (!entry && !byPath.has(file)) {
      errors.push(`${file}: not in the map. Add a row to ${INDEX} "Documents" (path, type, module, what, last verified), or, if it is not documentation (read at runtime or by a tool), an allowlist pattern under "Not documentation"`);
    }
  }
  for (const entry of index.allow) {
    if (!files.some((file) => entry.re.test(file))) errors.push(`${entry.at}: allowlist pattern ${entry.pattern} matches no file; remove it`);
  }
  const pending = [];
  for (const doc of index.docs) {
    if (!fileSet.has(doc.path)) {
      errors.push(`${doc.at}: ${doc.path} does not exist; remove the row (and record a removal in ${ARCHIVE})`);
      continue;
    }
    const type = TYPES[doc.type];
    if (!type) { errors.push(`${doc.at}: type "${doc.type}" is not one of ${Object.keys(TYPES).join(", ")}`); continue; }
    if (!type.ok(doc.path)) errors.push(`${doc.at}: a ${doc.type} doc lives in ${type.where}; move ${doc.path} or change its type`);
    if (modules ? doc.module !== "—" && !modules.has(doc.module) : !doc.module) errors.push(`${doc.at}: module "${doc.module}" is ${modules ? `not one of ${[...modules].join(", ")} (or — for none)` : "empty (name the module or —)"}`);
    if (!doc.what) errors.push(`${doc.at}: the What cell is empty`);
    if (doc.type === "pending") pending.push(doc);
    if (doc.type === "module") {
      const lines = read(doc.path).split(/\r?\n/u).length;
      if (lines > MODULE_README_MAX_LINES) errors.push(`${doc.at}: ${doc.path} has ${lines} lines; a module README says what the directory is and links into docs/ (at most ${MODULE_README_MAX_LINES}); move the body to a docs/ type`);
    }
    if (doc.type === "reference") {
      const head = read(doc.path).split(/\r?\n/u).slice(0, 10).join("\n");
      const verified = LAST_VERIFIED.exec(head);
      if (!verified) errors.push(`${doc.path}: a reference doc starts (first 10 lines) with "Last verified: YYYY-MM-DD against <commit>"`);
      else if (doc.verified !== verified[1]) errors.push(`${doc.at}: Last verified is ${verified[1]} in ${doc.path}; the map says "${doc.verified}"`);
    }
  }
  for (const removed of archive) {
    if (fileSet.has(removed.path) || byPath.has(removed.path)) errors.push(`${removed.at}: ${removed.path} is recorded as removed but exists or is mapped; drop the row or the file`);
  }
  return { errors, pending };
}

/** Pure: reference docs whose Last verified is older than `days` before `now`. */
export function staleDocs({ docs, now = new Date(), days = 30 }) {
  const limit = now.getTime() - days * 86_400_000;
  return docs.filter((doc) => doc.type === "reference" && /^\d{4}-\d{2}-\d{2}$/u.test(doc.verified) && Date.parse(`${doc.verified}T00:00:00Z`) < limit);
}

/** Pure: relative links of one Markdown text that point at no file (the fallback when lychee is unavailable). */
export function brokenRelativeLinks(path, text, exists) {
  const problems = [];
  const body = text.replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gmu, "").replace(/`[^`\n]*`/gu, "");
  for (const [, raw] of body.matchAll(/\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/gu)) {
    if (/^(?:[a-z][a-z0-9+.-]*:|#|\/\/)/iu.test(raw)) continue;
    const target = decodeURIComponent(raw.split("#")[0]);
    if (!target) continue;
    const resolved = target.startsWith("/") ? target.slice(1) : posix.normalize(posix.join(posix.dirname(path), target));
    if (!exists(resolved)) problems.push(`${path}: broken link ${raw}`);
  }
  return problems;
}

function markdownFiles() {
  const out = spawnSync("git", ["-c", "core.quotepath=off", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.md", "*.mdx"], { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, windowsHide: true });
  if (out.status !== 0) throw new Error(`check-docs: git ls-files failed: ${out.stderr}`);
  return [...new Set(out.stdout.split("\0").filter(Boolean))].filter((file) => existsSync(join(root, file))).sort();
}

/** Run lychee offline over the mapped docs. Returns { ok, output, via }. */
async function checkLinks(paths) {
  const lychee = await lycheeBinary();
  if (lychee.error) {
    const problems = paths.flatMap((path) => brokenRelativeLinks(path, readFileSync(join(root, path), "utf8"), (target) => existsSync(join(root, target))));
    return { ok: problems.length === 0, output: problems.join("\n"), via: `built-in relative-link check (no fragments) — ${lychee.error}` };
  }
  const work = mkdtempSync(join(tmpdir(), "check-docs-"));
  try {
    const list = join(work, "inputs.txt");
    writeFileSync(list, `${paths.join("\n")}\n`);
    const out = spawnSync(lychee.path, ["--offline", "--include-fragments", "--no-progress", "--root-dir", root, "--files-from", list], { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024, windowsHide: true });
    return { ok: out.status === 0, output: `${out.stdout}${out.stderr}`.trim(), via: `lychee (${lychee.source})` };
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

async function main() {
  const args = process.argv.slice(2);
  const after = (name) => { const i = args.indexOf(name); return i >= 0 ? args[i + 1] : undefined; };
  const read = (path) => readFileSync(join(root, path), "utf8");
  const index = parseIndex(read(INDEX));
  const archive = existsSync(join(root, ARCHIVE)) ? parseArchive(read(ARCHIVE)) : [];

  if (args.includes("--stale")) {
    const raw = after("--stale");
    const days = raw && /^\d+$/u.test(raw) ? Number(raw) : 30;
    const stale = staleDocs({ docs: index.docs, days });
    const pending = index.docs.filter((doc) => doc.type === "pending");
    const report = [
      `### Documentation: reference docs not verified for ${days} days`, "",
      ...(stale.length ? stale.map((doc) => `- \`${doc.path}\` — last verified ${doc.verified}`) : ["None."]), "",
      `### Documentation: docs still typed \`pending\` (${pending.length})`, "",
      ...(pending.length ? pending.map((doc) => `- \`${doc.path}\``) : ["None."]), "",
    ].join("\n");
    console.log(report);
    const summary = after("--summary");
    if (summary) appendFileSync(summary, `${report}\n`);
    return;
  }

  const started = Date.now();
  const files = markdownFiles();
  const { errors, pending } = checkDocs({ files, index, archive, read });
  let links = { ok: true, output: "", via: "skipped (--no-links)" };
  if (!args.includes("--no-links")) links = await checkLinks(index.docs.map((doc) => doc.path).filter((path) => files.includes(path)));
  if (errors.length) console.error(errors.join("\n"));
  if (!links.ok) console.error(`links (${links.via}):\n${links.output}`);
  const ok = errors.length === 0 && links.ok;
  console.log(JSON.stringify({ ok, check: "docs", files: files.length, mapped: index.docs.length, pending: pending.length, links: links.via, seconds: Number(((Date.now() - started) / 1000).toFixed(1)) }));
  if (!ok) process.exit(1);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => { console.error(`check-docs: ${error?.stack ?? error}`); process.exit(1); });
}
