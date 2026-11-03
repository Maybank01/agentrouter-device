# Documentation map

Every Markdown file of this repository, checked by `node scripts/check-docs.mjs` (CI job "Linux (core + command
line)"): a file missing here, a file outside `docs/` that is not an entry file, or a broken internal link fails it.
Removed docs: [`ARCHIVE.md`](ARCHIVE.md). The Agent model and the Cloud side of the link live in Cloud
(`docs/development/AGENT-FRAMEWORK.md`, `docs/development/ENVIRONMENTS.md`).

| Path | Type | Module | What | Last verified |
| --- | --- | --- | --- | --- |
| `README.md` | entry | all | the device connector for people (zh) and for developers: crates, CI, local checks | 2026-10-23 |
| `SECURITY.md` | entry | all | security policy and reporting | 2026-10-11 |
| `docs/HOSTD-MCP.md` | design | hostd-link, aggregator, sandbox, connector-cli | `hostd` + MCP aggregation: link protocol, host tools, sandbox, placements | 2026-11-03 |
| `docs/AV-HYGIENE.md` | development | connector-cli (builds) | build rules against antivirus false positives, enforced by `scripts/av-hygiene.sh` | 2026-10-22 |
| `docs/SIGNING.md` | decision | releases | code signing: deferred, owner 2026-10-08 | 2026-10-22 |
| `docs/ARCHIVE.md` | archive | all | docs removed and how to recover them | 2026-11-03 |
| `docs/README.md` | map | all | this map | 2026-11-03 |
| `npm/agentrouter/README.md` | module | npm package | README shipped in `@agentrouter-top/cli` | 2026-10-14 |
