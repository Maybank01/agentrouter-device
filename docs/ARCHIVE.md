# Archive

Docs removed from this repository. Before the cleanup of 2026-11-03 the default branch was tagged
`docs-archive-2026-11-03`; recover a file with `git show docs-archive-2026-11-03:<path>`. The last commit with the Rust
`hostd` (crates, desktop shell, npm packaging, its workflows and docs; deleted 2026-11-05, #236) is tagged
`hostd-final`: `git show hostd-final:<path>`.

## Removed

| Path removed | When | Why |
| --- | --- | --- |
| `docs/HOSTD-MCP.md` | 2026-11-05 | `hostd` + MCP aggregation design and link protocol v2: `hostd` was deleted (#236); the client only executes over SSH (R45). Other repositories may still cite it and its section numbers (AgentCompute `contracts/executor-protocol/v0`, Cloud): they go with their own deletions (AgentCompute #52, Cloud #4085). |
| `npm/agentrouter/README.md` | 2026-11-05 | README shipped in `@agentrouter-top/cli` (`hostd`'s npm package): the packaging was deleted with `hostd` (#236); published versions keep it. |
| `docs/LINKER-MCP.md` | 2026-11-03 | Redirect stub since the rename to `hostd` (#80); the design is `docs/HOSTD-MCP.md` with the same section numbers. |

## Moved

Other repositories may still cite the old path (2026-11-03: agentteam-deployments
`products/agentrouter/client-repositories.json` cites `docs/SIGNING.md`; Cloud `docs/development/VISION.md` and
`docs/reference/ENVIRONMENTS.md` cite `docs/AV-HYGIENE.md`).

| Old path | New path |
| --- | --- |
| `docs/SIGNING.md` | [docs/adr/0001-code-signing.md](adr/0001-code-signing.md) |
| `docs/AV-HYGIENE.md` | [docs/development/AV-HYGIENE.md](development/AV-HYGIENE.md) |
