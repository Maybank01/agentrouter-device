// Builds the npm packages from release binaries (publishing is M2):
//   node npm/pack.mjs --version 0.1.0 --binary win32-x64=target/release/agentrouter.exe \
//                     --binary linux-x64=target/x86_64-unknown-linux-gnu/release/agentrouter
// Writes npm/out/agentrouter and npm/out/cli-<os>-<arch>; `npm publish npm/out/<dir> --access public`
// (platform packages first, then the main package). Platforms without a binary are left out of
// optionalDependencies, so the main package never points at a package that was not published.
import { chmodSync, copyFileSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const SCOPE = "@agentrouter-top";
const PLATFORMS = {
  "win32-x64": { os: "win32", cpu: "x64" },
  "win32-arm64": { os: "win32", cpu: "arm64" },
  "linux-x64": { os: "linux", cpu: "x64" },
  "linux-arm64": { os: "linux", cpu: "arm64" },
  "darwin-x64": { os: "darwin", cpu: "x64" },
  "darwin-arm64": { os: "darwin", cpu: "arm64" },
};

const args = process.argv.slice(2);
let version;
const binaries = {};
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--version") version = args[++i];
  else if (args[i] === "--binary") {
    const [key, file] = args[++i].split("=");
    if (!PLATFORMS[key]) throw new Error(`unknown platform ${key}`);
    binaries[key] = file;
  } else throw new Error(`unknown argument ${args[i]}`);
}
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) throw new Error("--version X.Y.Z is required");
if (!Object.keys(binaries).length) throw new Error("at least one --binary <os>-<arch>=<file>");

const out = path.join(here, "out");
rmSync(out, { recursive: true, force: true });
const main = JSON.parse(readFileSync(path.join(here, "agentrouter", "package.json"), "utf8"));
const common = { license: main.license, repository: main.repository, homepage: main.homepage };

for (const [key, file] of Object.entries(binaries)) {
  const { os, cpu } = PLATFORMS[key];
  const dir = path.join(out, `cli-${key}`);
  const exe = os === "win32" ? "agentrouter.exe" : "agentrouter";
  mkdirSync(path.join(dir, "bin"), { recursive: true });
  copyFileSync(file, path.join(dir, "bin", exe));
  if (os !== "win32") chmodSync(path.join(dir, "bin", exe), 0o755);
  const pkg = {
    name: `${SCOPE}/cli-${key}`,
    version,
    description: `The ${key} binary of the agentrouter command line (installed by the agentrouter package).`,
    ...common,
    repository: { ...common.repository, directory: "npm" },
    os: [os],
    cpu: [cpu],
    files: [`bin/${exe}`],
    preferUnplugged: true,
  };
  writeFileSync(path.join(dir, "package.json"), `${JSON.stringify(pkg, null, 2)}\n`);
}

const mainDir = path.join(out, "agentrouter");
mkdirSync(path.join(mainDir, "bin"), { recursive: true });
copyFileSync(path.join(here, "agentrouter", "bin", "agentrouter.js"), path.join(mainDir, "bin", "agentrouter.js"));
main.version = version;
main.optionalDependencies = Object.fromEntries(
  Object.keys(binaries).sort().map((key) => [`${SCOPE}/cli-${key}`, version]),
);
writeFileSync(path.join(mainDir, "package.json"), `${JSON.stringify(main, null, 2)}\n`);
console.log(`npm packages for ${version}: agentrouter + ${Object.keys(binaries).map((k) => `cli-${k}`).join(", ")} in ${out}`);
