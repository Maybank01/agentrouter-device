---
name: agentrouter-client
description: Understand, develop or assess the AgentRouter client — the device connector (repository agentrouter-top/AgentDevice, `agentrouter link` / hostd): a pure remote MCP client that links a person's computer to our server so cloud Agents work in its folders through sandboxed local MCP services. Use for device-link architecture, local MCP services, sandboxing, audit and undo, and npm CLI delivery. The DSH plugin / Desktop-adapter client is abandoned (owner 2026-10-18).
---

# AgentRouter client = the device connector

Owner decision 2026-10-18: the client is a pure remote MCP client. It links the
person's computer outbound to our server; a cloud Agent then works there through
the computer's local MCP services. It has no file, command or screen tools of
its own and opens no inbound ports.

## Where to read

| Need | Read |
| --- | --- |
| What the client does, commands, boundaries, crates, how tests run | This repository's `README.md` |
| hostd + MCP aggregation design | `docs/HOSTD-MCP.md` (all docs: `docs/README.md`) |
| Build rules against antivirus false positives; code signing (deferred) | `docs/development/AV-HYGIENE.md`, `docs/adr/0001-code-signing.md` |
| How a linked computer fits the Agent model (devices, environment types, holding and entering environments) | Cloud `docs/development/AGENT-FRAMEWORK.md`, then the execution-layer reference it names (Cloud `docs/README.md` lists all docs) |
| Cloud side of the link | Cloud `services/control-plane/modules/devices` |
| Product direction for devices | Cloud `docs/product/PRODUCT.md` |
| Registry of record | agentteam-deployments `products/agentrouter/client-repositories.json` |

## Working rules

- Client work is parked behind Cloud (owner 2026-10-09) unless the owner asks.
- Tests run on GitHub-hosted runners only; locally `cargo build`, `cargo check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`.
- Delivery is npm `@agentrouter-top/cli` (`.github/workflows/npm.yml`). Do not
  bump, publish or build installers per task; release authority is separate.
- The desktop shell (`app/`) is frozen. The DSH plugin / Desktop-adapter line
  (`agentrouter-dsh-plugins`, `plugins/dsh-codex`, `agentrouter-desktop-releases`)
  is history only: never develop, publish or document it as the client.
- Model login, LLM keys and model requests are not client features.
