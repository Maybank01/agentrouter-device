#!/usr/bin/env node
// Pins a copy of the one error catalog (Maybank01/agentrouter-cloud contracts/errors/catalog.json, Cloud #2449) into
// crates/device-core/src/error-catalog.json, stamped with that checkout's commit. The catalog is defined in Cloud only;
// the device answers with its codes (device-core `errors.rs`; a test checks every code there is in this copy).
//   node scripts/sync-error-catalog.mjs --from ../agentrouter-cloud   (a checkout of Maybank01/agentrouter-cloud)
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const at = process.argv.indexOf("--from");
const from = resolve(at > 0 ? process.argv[at + 1] : "../agentrouter-cloud");
const catalog = JSON.parse(readFileSync(resolve(from, "contracts/errors/catalog.json"), "utf8"));
const commit = execFileSync("git", ["-C", from, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const out = resolve(import.meta.dirname, "../crates/device-core/src/error-catalog.json");
writeFileSync(out, `${JSON.stringify({ source: `agentrouter-cloud@${commit}`, ...catalog }, null, 2)}\n`);
console.log(`error catalog from agentrouter-cloud@${commit.slice(0, 7)}: ${catalog.errors.length} entries`);
