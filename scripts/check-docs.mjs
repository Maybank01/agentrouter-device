#!/usr/bin/env node
// Fast documentation check (docs framework, owner 2026-11-03 「做好文档的记录框架，不要再让各个文档散落在各处」).
// Fails when a Markdown file sits outside the allowed places, is missing from the map `docs/README.md`, the map names
// a file that does not exist, or a repository-internal link points at nothing. No dependencies; runs in well under a
// second. `--stale` instead prints reference docs whose `Last verified:` line is older than 30 days (a report, not a gate).
//
// Allowed places: `docs/**`; root `README.md`, `AGENTS.md`, `CLAUDE.md`, `SECURITY.md`; one `README.md` per
// directory (what the directory is + links into docs/); and the ALLOW list below (non-doc Markdown read by tools or
// at runtime, each with its reason).
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

const ROOT_FILES = new Set(["README.md", "AGENTS.md", "CLAUDE.md", "SECURITY.md"]);
// { prefix, why, map }: a path equal to or below `prefix` is allowed outside docs/; `map: false` = not documentation,
// so it is neither required in the map nor link-checked.
const ALLOW = [
  { prefix: "skills/", why: "agent skills, read through the ~/.claude/skills symlinks (workspace AGENTS.md)" },
];
const SKIP = [/(^|\/)node_modules\//, /(^|\/)CHANGELOG\.md$/, /(^|\/)vendor\//];
const MAP = "docs/README.md";
const STALE_DAYS = 30;

const root = execFileSync("git", ["rev-parse", "--show-toplevel"], { encoding: "utf8" }).trim();
const files = execFileSync("git", ["ls-files", "-co", "--exclude-standard", "--", "*.md"], { cwd: root, encoding: "utf8" })
  .split("\n").filter(Boolean).filter((f) => !SKIP.some((re) => re.test(f)) && existsSync(path.join(root, f)));
const read = (f) => readFileSync(path.join(root, f), "utf8");
const allowed = (f) => ALLOW.find((a) => f === a.prefix || f.startsWith(a.prefix.endsWith("/") ? a.prefix : `${a.prefix}/`));

if (process.argv.includes("--stale")) {
  const limit = Date.now() - STALE_DAYS * 86400e3;
  for (const f of files.filter((f) => f.startsWith("docs/reference/"))) {
    const m = /Last verified:\s*(\d{4}-\d{2}-\d{2})/.exec(read(f));
    if (!m || Date.parse(m[1]) < limit) console.log(`${f}: last verified ${m ? m[1] : "never"}`);
  }
  process.exit(0);
}

const errors = [];
const map = existsSync(path.join(root, MAP)) ? read(MAP) : "";
if (!map) errors.push(`${MAP}: the map is missing`);
// The map is a table whose first column is the path in backticks; other backticked paths (prose, other repositories) are not entries.
const mapped = new Set([...map.matchAll(/^\|\s*`([^`\s]+\.md)`\s*\|/gm)].map((m) => m[1]));

for (const f of files) {
  const allow = allowed(f);
  const placed = f.startsWith("docs/") || ROOT_FILES.has(f) || path.basename(f) === "README.md" || allow;
  if (!placed) errors.push(`${f}: Markdown outside docs/ (move it into docs/, or add it to ALLOW in scripts/check-docs.mjs with the tool that reads it)`);
  if (f !== MAP && allow?.map !== false && !mapped.has(f)) errors.push(`${f}: not listed in ${MAP}`);
}
for (const m of mapped) if (!existsSync(path.join(root, m))) errors.push(`${MAP}: lists ${m}, which does not exist`);

for (const f of files.filter((f) => allowed(f)?.map !== false)) {
  const text = read(f).replace(/^(```|~~~)[^\n]*\n[\s\S]*?^\1[^\n]*$/gm, "").replace(/`[^`\n]*`/g, "");
  for (const [, raw] of text.matchAll(/\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/g)) {
    if (/^(?:[a-z][a-z0-9+.-]*:|#|\/\/)/i.test(raw)) continue;
    const target = decodeURIComponent(raw.split("#")[0]);
    if (!target) continue;
    const abs = target.startsWith("/") ? path.join(root, target) : path.join(root, path.dirname(f), target);
    if (!existsSync(abs)) errors.push(`${f}: broken link ${raw}`);
  }
}

if (errors.length) {
  console.error(`check-docs: ${errors.length} problem(s)\n${errors.map((e) => `  ${e}`).join("\n")}`);
  process.exit(1);
}
console.log(`check-docs: ${files.length} Markdown files placed, mapped and linked`);
