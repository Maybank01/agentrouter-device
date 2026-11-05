---
name: agentrouter-client
description: Understand, develop or assess the AgentRouter client — the device client `agentdevice` (repository agentrouter-top/AgentDevice): a Go program embedding NetBird that makes a person's computer an execution device the cloud reaches by SSH over its owner's private network; no MCP, no tools of its own. Use for the device client, device login and enrolment, the device's SSH server, NetBird on the device, client builds and pre-releases, and the Windows end-to-end check. The Rust hostd / `agentrouter link` (deleted 2026-11-05) and the DSH plugin / Desktop-adapter client (abandoned 2026-10-18) are history.
---

# AgentRouter client = the device client (`agentdevice`)

Owner 2026-11-04 (R45, R53; base layers v1, Cloud #3976): 「设备端只做执行」, 「装上客户端，云端就有一个按照SSH协议的执行设备。所有连接的复杂度全部在这一层里面」, 「可以，就用NetBird」.
A Go program embedding NetBird `client/embed` (userspace: no administrator rights, no driver) with our own
JWT-authenticated SSH server on the NetBird address (ADR 0002). It joins its owner's self-hosted NetBird network and
opens no port on the computer's own network. Agents work on it through standard SSH / SFTP, reached through
AgentCompute's SSH gateway (`compute/v1`). An SSH session acts as the device's user on the whole computer: no folder or
machine access levels (R58). Its own: device-code login, enrolment and facts, device name, tray, autostart.
AgentCompute's cloud sandboxes run the same program (`agentdevice sandbox`).

## Where to read

| Need | Read |
| --- | --- |
| What belongs here, contracts consumed, how it is pinned | This repository's `docs/CHARTER.md` |
| Modules, commands, interfaces, known gaps | `client/README.md`; repository overview `README.md` |
| Why our own SSH server; sandbox egress | `docs/adr/0002-own-ssh-server.md`, `docs/adr/0003-sandbox-egress-relay.md` |
| Security design points | `SECURITY.md` |
| Build rules against antivirus false positives; code signing (deferred) | `docs/development/AV-HYGIENE.md`, `docs/adr/0001-code-signing.md` |
| Computers, the SSH gateway, NetBird management, enrolment API | AgentCompute (`compute/v1`) |
| Device login, tokens, JWKS | AgentIdentity (`identity/v1`) |
| How a computer fits the Agent model (environments, grants) | Cloud `docs/development/AGENT-FRAMEWORK.md`; design Cloud #3976 |
| Product direction for devices (我的设备) | Cloud `docs/product/PRODUCT.md` |
| All docs of this repository | `docs/README.md` |

## Working rules

- Client work is parked behind Cloud (owner 2026-10-09) unless the owner asks (R48).
- Tests run on GitHub-hosted runners only (R47, `.github/workflows/ci.yml`); locally in `client/` only `go build
  ./...` and `go vet ./...` (also `GOOS=windows` / `darwin`), plus the root checks `bash scripts/av-hygiene.sh`,
  `node scripts/check-docs.mjs`, `node scripts/check-repo-imports.mjs`.
- Never add MCP, file / command / screen tools, per-folder sandboxes or access levels to the device. Never restore the
  Rust `hostd` / `agentrouter link` (source: tag `hostd-final`).
- Delivery: pre-releases `client-v0.0.0-<sha7>` from `main` (static Linux for sandboxes, Windows with Dev endpoints);
  releases for people `client-vX.Y.Z` by `release.yml` (per-user Windows installer and Linux archive per complete
  `release/environments/*.json`; production incomplete until the cutover's P5; `release/README.md`). npm
  `@agentrouter-top/cli` was `hostd`'s channel and is deprecated at the base cutover. Do not bump or cut a release per
  task; release authority is separate (R14).
- The DSH plugin / Desktop-adapter line (`agentrouter-dsh-plugins`, `plugins/dsh-codex`, `agentrouter-desktop-releases`)
  is history only (R46): never develop, publish or document it as the client.
- Model login, LLM keys and model requests are not client features.
