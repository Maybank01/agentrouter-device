# Charter: AgentDevice

Our client: a branded Go program a person installs so that their computer becomes a computer of the compute layer,
reachable by SSH inside the owner's own private network and nothing else. Owner 2026-11-04: 「装上客户端，云端就有一个按照SSH
协议的执行设备」, 「设备端只做执行」 (base layers v1 design §4.3, Cloud #3976). AgentCompute's cloud sandboxes run the same
program, so a sandbox and a person's computer are reached the same way. Layer: the device side of the compute layer.
The Rust `hostd` / `agentrouter link` was deleted on 2026-11-05 (#236); released copies keep working against production
until the base cutover, which retires them.

**Belongs here**: embedding NetBird `client/embed` (BSD-3, pinned release): userspace networking, no admin rights, no
driver; it joins the owner's NetBird network on our self-hosted management. NetBird's SSH server (`client/ssh/server`:
shell, commands, SFTP, port forwarding, Windows included), JWT only: its `JWTConfig` (`Issuer`, `KeysLocation` = JWKS,
`Audiences` = `netbird`, `MaxTokenAge`) comes per peer from management; the client allows the server. Our login (device
code to the person's NewAPI account), device name, tray, autostart, logout (= the peer is removed), updates, per-OS
release builds (signing deferred, ADR 0001). Honest `facts` (OS, architecture, shell, CPU / memory / GPU, installed
software) in AgentCompute's shape, with no flattening of OS differences.

**Never here**: MCP, or any file, command, screen or other tool of its own: the device only executes what arrives over
SSH. Per-folder sandboxes or access levels (the session runs as the user on that computer; folders only tell the Agent
where to look, owner 2026-11-04). Placement, sandboxes, environments, spaces, Agents (AgentCompute, AgentHost,
AgentSpace, ECS). Accounts, login, token issuance (AgentIdentity), prices (NewAPI). NetBird management, signal, relay
and the SSH gateway (AgentCompute; NetBird's AGPL never enters this program). Patches to NetBird (fix upstream).
The abandoned DSH-plugin / Desktop-adapter client (R46).

**Contracts provided** (C1–C3): none; it provides a computer, an SSH endpoint in the owner's network.

**Consumes** (pinned, C4, C6): AgentIdentity `identity/v1` (ES256 JWT, JWKS, device code at ZITADEL + STS exchange,
`aud: netbird`; NewAPI's device login until the issuer switch); AgentCompute `compute/v1` (`kind: device`, `name`, `facts`);
NetBird by `go.mod` version; our NetBird management by URL only (contract copies: `client/testdata/contracts/`).

**How it is pinned**: per-OS binaries with `SHA256SUMS`, pre-releases `client-v0.0.0-<sha7>` and releases `client-vX.Y.Z`
(release/README.md); a consumer pins asset + `sha256` (AgentCompute's environment image pins the static Linux build).
**Dependencies** (C5): only those two contracts and NetBird; nothing upward (online / offline is what NetBird reports).
**Checks on the PR path** (GitHub-hosted, R47): `check-docs.mjs`, `check-repo-imports.mjs` (C6), AV hygiene, gofmt,
vet, tests and build on Linux, macOS and Windows (`ci.yml`).

**Owner rules** (`workspace/RULES.md`): R45 R53 (the client), R58 (no access levels), R46 R54 (DSH plugin
client abandoned), R47 (tests on GitHub-hosted runners only), R48, R14 (own release flow), R17, R21, R38.
