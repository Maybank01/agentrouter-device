// The pinned lychee link checker (lycheeverse/lychee, Apache-2.0) for the documentation check (scripts/check-docs.mjs).
// Copied from agentrouter-cloud scripts/lychee.mjs (Cloud docs/development/DOCS.md); keep the pin the same in every
// AgentTeam repository and change it there first.
//
// lycheeBinary() looks, in order, for: a CI runner's copy ($AGENTROUTER_CI_TOOLS/bin/lychee), a cached copy
// (~/.cache/agentrouter-ci/lychee-<version>/lychee), then downloads the release asset for this platform, checks its
// SHA-256 against the pin and caches it (about 7 MB, once per machine). It never throws: it returns { path } or
// { error } naming what failed, and the caller decides what to do without it.
// Justification: a pinned, checksum-verified fetch of a release binary; lychee ships no npm package.
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const LYCHEE = Object.freeze({
  version: "0.24.2",
  // Release assets of lychee-v<version> and their SHA-256 (the release's own .sha256 files).
  assets: Object.freeze({
    "linux-x64": { file: "lychee-x86_64-unknown-linux-musl.tar.gz", sha256: "73657a111819a30c47c08352896796f23d64e4eb2b3ed39b6d32149241566fc5" },
    "linux-arm64": { file: "lychee-aarch64-unknown-linux-musl.tar.gz", sha256: "5d0b0e3aeab240f41920c633a6eaf97599be6eedda034b36e858ede7dba5e535" },
    "darwin-x64": { file: "lychee-x86_64-apple-darwin.tar.gz", sha256: "887503a9cff667d322b8d0892b40bf49976eb9507af8483220a3706cdad55978" },
    "darwin-arm64": { file: "lychee-aarch64-apple-darwin.tar.gz", sha256: "c9d3740ea2d891854d37116c9fba840f37b6e7c89d330e7db84ac333631c4977" },
  }),
});

const releaseUrl = (file) => `https://github.com/lycheeverse/lychee/releases/download/lychee-v${LYCHEE.version}/${file}`;

/** The version a lychee binary reports, or null when it does not run. */
export function lycheeVersion(path) {
  const out = spawnSync(path, ["--version"], { encoding: "utf8", timeout: 10_000, windowsHide: true });
  return out.status === 0 ? /lychee\s+(\S+)/u.exec(out.stdout)?.[1] ?? null : null;
}

export function cacheDir(env = process.env) {
  const base = env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "agentrouter-ci", `lychee-${LYCHEE.version}`);
}

/** Resolve the pinned lychee: { path, source } or { error }. */
export async function lycheeBinary({ env = process.env, platform = `${process.platform}-${process.arch}`, timeoutMs = 30_000 } = {}) {
  const candidates = [];
  if (env.AGENTROUTER_CI_TOOLS) candidates.push({ path: join(env.AGENTROUTER_CI_TOOLS, "bin", "lychee"), source: "light runner image" });
  const cached = join(cacheDir(env), "lychee");
  candidates.push({ path: cached, source: "cache" });
  for (const candidate of candidates) {
    if (existsSync(candidate.path) && lycheeVersion(candidate.path) === LYCHEE.version) return candidate;
  }
  const asset = LYCHEE.assets[platform];
  if (!asset) return { error: `lychee: no pinned release asset for platform ${platform} (pinned: ${Object.keys(LYCHEE.assets).join(", ")})` };
  const url = releaseUrl(asset.file);
  let body;
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(timeoutMs) });
    if (!response.ok) return { error: `lychee: download ${url} answered HTTP ${response.status}; a retry may help` };
    body = Buffer.from(await response.arrayBuffer());
  } catch (error) {
    return { error: `lychee: download ${url} failed (${error?.cause?.code ?? error?.name ?? "error"}: ${error?.message ?? error}); a retry may help` };
  }
  const digest = createHash("sha256").update(body).digest("hex");
  if (digest !== asset.sha256) return { error: `lychee: ${asset.file} has SHA-256 ${digest}, pinned ${asset.sha256}; refusing it (do not retry; check the pin in scripts/lychee.mjs)` };
  const dir = cacheDir(env);
  let work;
  try {
    // Extract beside the cache (same filesystem), then rename into place: a half-written binary is never found.
    mkdirSync(dirname(dir), { recursive: true });
    work = mkdtempSync(join(dirname(dir), ".lychee-"));
    const archive = join(work, asset.file);
    writeFileSync(archive, body);
    const inner = asset.file.replace(/\.tar\.gz$/u, "");
    const untar = spawnSync("tar", ["-xzf", archive, "-C", work, `${inner}/lychee`], { encoding: "utf8", windowsHide: true });
    if (untar.status !== 0) return { error: `lychee: extracting ${asset.file} failed: ${(untar.stderr || untar.error?.message || "").trim()}` };
    mkdirSync(dir, { recursive: true });
    renameSync(join(work, inner, "lychee"), cached);
  } catch (error) {
    return { error: `lychee: installing ${asset.file} into ${dir} failed (${error?.code ?? "error"}: ${error?.message ?? error})` };
  } finally {
    if (work) rmSync(work, { recursive: true, force: true });
  }
  if (lycheeVersion(cached) !== LYCHEE.version) return { error: `lychee: the downloaded binary at ${cached} does not report version ${LYCHEE.version}` };
  return { path: cached, source: "downloaded" };
}
