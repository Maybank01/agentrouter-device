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

Types: **entry** (root `README.md`, `AGENTS.md`, `SECURITY.md`), **index** (this map, ARCHIVE), **charter** (`docs/CHARTER.md`, at most 40 lines), **development**
(`docs/development/`, how to work here), **adr** (`docs/adr/NNNN-name.md`, dated decisions), **module** (a short README
of a directory, at most 40 lines), **protocol** (this repository only: `docs/HOSTD-MCP.md`, the link protocol and
`hostd` design; it keeps its path because Cloud code, docs and the vendored runner cite it and its section numbers over
a hundred times).

## Documents

| Path | Type | Module | What | Last verified |
| --- | --- | --- | --- | --- |
| [README.md](../README.md) | entry | all | direction (2026-11-04: Go client embedding NetBird replaces `hostd`); the current connector for people (zh) and for developers: crates, CI, local checks | 2026-11-04 |
| [AGENTS.md](../AGENTS.md) | entry | all | agent instructions: direction, scope, checks, landing, release, open-source base | 2026-11-04 |
| [SECURITY.md](../SECURITY.md) | entry | all | security policy and reporting; design points of the current chain | 2026-11-03 |
| [docs/README.md](README.md) | index | all | this map | 2026-11-03 |
| [docs/CHARTER.md](CHARTER.md) | charter | all | the Go client embedding NetBird (direction 2026-11-04): what belongs here and what never does, contracts consumed, how it is pinned, checks | 2026-11-04 |
| [docs/ARCHIVE.md](ARCHIVE.md) | index | all | docs removed or moved and how to recover them | 2026-11-03 |
| [docs/HOSTD-MCP.md](HOSTD-MCP.md) | protocol | hostd-link, aggregator, sandbox, connector-cli | `hostd` + MCP aggregation: link protocol, host tools, sandbox, placements | 2026-11-03 |
| [docs/development/AV-HYGIENE.md](development/AV-HYGIENE.md) | development | connector-cli, client (builds) | build rules against antivirus false positives (hostd; Go client: one visible console window), enforced by `scripts/av-hygiene.sh` | 2026-11-04 |
| [docs/adr/0001-code-signing.md](adr/0001-code-signing.md) | adr | releases | code signing: deferred, owner 2026-10-08 | 2026-10-22 |
| [docs/adr/0002-own-ssh-server.md](adr/0002-own-ssh-server.md) | adr | client | the Go client serves SSH itself on the NetBird address; NetBird is the network only (P3 evidence, #221) | 2026-11-04 |
| [docs/adr/0003-sandbox-egress-relay.md](adr/0003-sandbox-egress-relay.md) | adr | client | a sandbox's client relays NetBird's TLS by SNI through the node's HTTP proxy; AgentCompute maps NetBird's names to 127.0.0.1 (P5, #223) | 2026-11-04 |
| [client/README.md](../client/README.md) | module | client | the Go device client: purpose, boundary, interfaces, submodule index, known gaps | 2026-11-04 |
| [npm/agentrouter/README.md](../npm/agentrouter/README.md) | module | npm package | README shipped in `@agentrouter-top/cli` | 2026-10-14 |

## Not documentation

Markdown that code, a build or a tool reads. The check skips these; change them with what reads them. A pattern
added here must match a file.

| Pattern | What | Read by |
| --- | --- | --- |
| `skills/**/*.md` | Agent skills (SKILL.md and references) shipped with the repository | coding agents (Claude Code skills, `~/.claude/skills` links) |
