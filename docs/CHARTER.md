# Charter: AgentDevice

Our client: a branded Go program a person installs so that their computer becomes a computer of the compute layer,
reachable by SSH inside the owner's own private network and nothing else. Owner 2026-11-04: 「装上客户端，云端就有一个按照SSH
协议的执行设备」, 「设备端只做执行」 (base layers v1 design §4.3, Cloud #3976). AgentCompute's cloud sandboxes run the same
program, so a sandbox and a person's computer are reached the same way. Layer: the device side of the compute layer.
**Transition**: the Rust `hostd` / `agentrouter link` here today keeps releasing until the Go client is verified on Dev,
then it is deleted (C3: released copies on people's computers hold it).

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
AgentSpace, ECS). Accounts, token issuance, prices (NewAPI). NetBird management, signal, relay and the SSH gateway
(AgentCompute; NetBird's AGPL directories never enter this program). Patches to NetBird (pin a release, fix upstream).
The abandoned DSH-plugin / Desktop-adapter client (R46).

**Contracts provided** (C1–C3): none; it provides a computer, an SSH endpoint in the owner's network. Until deleted, the
`hostd` link protocol v2 (`crates/device-core/src/protocol.rs`, `docs/HOSTD-MCP.md`) is held by released `hostd`.

**Consumes** (pinned, C4, C6): NewAPI `identity/v1` (EdDSA JWT, JWKS, device-code login, `aud: netbird` tokens);
AgentCompute `compute/v1` conventions (`kind: device`, `name`, `facts`; `state` comes from the network, not from us);
NetBird by `go.mod` version; our NetBird management by URL only. Until deleted, `hostd` implements AgentCompute
`contracts/executor-protocol/v0` (copy in `vendor/agentcompute/`).

**How it is pinned**: per-OS release binaries with `SHA256SUMS`; a consumer pins asset + `sha256` (today Cloud
`deploy/versions.json` `components.agentDevice` pins `hostd`; AgentCompute's sandbox image will pin the Go client).
**Dependencies** (C5): only those two contracts and NetBird; nothing upward (online / offline is what NetBird reports).
**Checks on the PR path** (GitHub-hosted, R47): `check-docs.mjs`, `check-repo-imports.mjs` (C6), build, lint, tests.

**Owner rules** (`workspace/RULES.md`): R45 R53 (the client; superseded 2026-11-04 by this model), R46 R54 (DSH plugin
client abandoned), R47 (tests on GitHub-hosted runners only), R48, R14 (own release flow), R17, R21, R38.
