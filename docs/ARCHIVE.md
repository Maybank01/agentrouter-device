# Archive

Docs removed from this repository. Before the cleanup of 2026-11-03 the default branch was tagged
`docs-archive-2026-11-03`; recover a file with `git show docs-archive-2026-11-03:<path>`.

## Removed

| Path removed | When | Why |
| --- | --- | --- |
| `docs/LINKER-MCP.md` | 2026-11-03 | Redirect stub since the rename to `hostd` (#80); the design is `docs/HOSTD-MCP.md` with the same section numbers. |

## Moved

Other repositories may still cite the old path (2026-11-03: agentteam-deployments
`products/agentrouter/client-repositories.json` cites `docs/SIGNING.md`; Cloud `docs/development/VISION.md` and
`docs/reference/ENVIRONMENTS.md` cite `docs/AV-HYGIENE.md`).

| Old path | New path |
| --- | --- |
| `docs/SIGNING.md` | [docs/adr/0001-code-signing.md](adr/0001-code-signing.md) |
| `docs/AV-HYGIENE.md` | [docs/development/AV-HYGIENE.md](development/AV-HYGIENE.md) |
