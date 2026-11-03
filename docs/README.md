# Documentation map

Every Markdown file in this repository is a row below or matches "Not documentation". The framework is the one of
every AgentTeam repository (Cloud `docs/development/DOCS.md`): one doc home `docs/`, fixed types, this map, removed and
moved docs in [ARCHIVE.md](ARCHIVE.md). The Agent model and the Cloud side of the link are not documented here: they
live in Cloud `docs/development/AGENT-FRAMEWORK.md` and `docs/reference/ENVIRONMENTS.md`.

`node scripts/check-docs.mjs` (CI job "Linux (core + command line)", seconds) fails when a Markdown file is neither a
row nor allowlisted, a row's type does not fit its place, a `module` README is over 40 lines, a `reference` doc lacks
its `Last verified: YYYY-MM-DD against <commit>` line or the map shows another date, a removed path exists again, or a
link in a mapped doc does not resolve (lychee, offline: repository files and `#fragments`). `node
scripts/check-docs.mjs --stale` lists reference docs not verified for 30 days.

Types: **entry** (root `README.md`, `SECURITY.md`), **index** (this map, ARCHIVE), **development**
(`docs/development/`, how to work here), **adr** (`docs/adr/NNNN-name.md`, dated decisions), **module** (a short README
of a directory, at most 40 lines), **protocol** (this repository only: `docs/HOSTD-MCP.md`, the link protocol and
`hostd` design; it keeps its path because Cloud code, docs and the vendored runner cite it and its section numbers over
a hundred times).

## Documents

| Path | Type | Module | What | Last verified |
| --- | --- | --- | --- | --- |
| [README.md](../README.md) | entry | all | the device connector for people (zh) and for developers: crates, CI, local checks | 2026-11-03 |
| [SECURITY.md](../SECURITY.md) | entry | all | security policy and reporting; design points of the current chain | 2026-11-03 |
| [docs/README.md](README.md) | index | all | this map | 2026-11-03 |
| [docs/ARCHIVE.md](ARCHIVE.md) | index | all | docs removed or moved and how to recover them | 2026-11-03 |
| [docs/HOSTD-MCP.md](HOSTD-MCP.md) | protocol | hostd-link, aggregator, sandbox, connector-cli | `hostd` + MCP aggregation: link protocol, host tools, sandbox, placements | 2026-11-03 |
| [docs/development/AV-HYGIENE.md](development/AV-HYGIENE.md) | development | connector-cli (builds) | build rules against antivirus false positives, enforced by `scripts/av-hygiene.sh` | 2026-10-22 |
| [docs/adr/0001-code-signing.md](adr/0001-code-signing.md) | adr | releases | code signing: deferred, owner 2026-10-08 | 2026-10-22 |
| [npm/agentrouter/README.md](../npm/agentrouter/README.md) | module | npm package | README shipped in `@agentrouter-top/cli` | 2026-10-14 |

## Not documentation

Markdown that code, a build or a tool reads. None in this repository today; a pattern added here must match a file.

| Pattern | What | Read by |
| --- | --- | --- |
