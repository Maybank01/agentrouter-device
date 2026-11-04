# Charter: AgentDevice

The device connector: `hostd` + the `agentrouter` CLI (and the frozen desktop shell) on a person's computer — a pure
remote MCP client that links the computer out to our server and runs the local MCP services in the OS sandbox. Layer:
behind NewAPI, the executor side of the compute layer (Cloud AGENT-FRAMEWORK §10.2: it implements the executor
protocol AgentCompute defines). The cloud sandbox image ships the same `hostd`.

**Belongs here**: the link layer (`crates/hostd-link`), the MCP aggregator, the sandbox, checkpoints and undo,
`device-core` (pairing, keys, protocol primitives, audit), `connector-cli` (`agentrouter link`, npm
`@agentrouter-top/cli`), release builds of `hostd` and the sandbox tools.

**Never here**: file, command or screen tools of its own (the tools are the installed MCP services, R45); inbound
ports; decisions about placement, environments or Agents (AgentCompute, AgentHost, ECS); accounts and prices (NewAPI);
the abandoned DSH-plugin / Desktop-adapter client (R46).

**Contracts provided** (C1–C3)

| Contract | Path | Version | Consumers |
| --- | --- | --- | --- |
| Executor side of the link protocol (hello v2, `{s, m}` frames) and `hostd` design | `crates/device-core/src/protocol.rs` + vectors, `docs/HOSTD-MCP.md` | v2 | AgentCompute's link hub and Cloud Control |

The executor protocol is AgentCompute's (`contracts/executor-protocol/v0`; copies in `vendor/agentcompute/`, tested by
`crates/connector-cli/tests/executor_contract.rs`). A released `hostd` is a real external dependency (C3).

**Consumes**: Cloud's pairing (`/device/v1/pair`) and link gateway (`/device/v2/link`, `/device/v2/port/`), over the
network only; pinned MCP packages and `sandbox-runtime` by lockfile/catalog;
nothing of another repository's source (C6).

**How it is pinned**: Cloud `deploy/versions.json` `components.agentDevice` (`hostd` release asset + `sha256` from
`SHA256SUMS`, `follow: manual`, rebuilds the session image) (C4).

**Dependencies** (C5): down to nothing of ours; reports to its gateway by events and live output only.

**Checks on the PR path** (CI `ci.yml`, job "Linux (core + command line)", GitHub-hosted, R47): `cargo fmt`,
`scripts/check-docs.mjs` (map, charter, links), `scripts/check-repo-imports.mjs` (C6), then build, clippy and tests.

**Owner rules** (`workspace/RULES.md`): R45 R53 (the client is the device connector), R46 R54 (DSH plugin client
abandoned), R47 (tests on GitHub-hosted runners only), R48 (client work parked behind Cloud), R14 (own release flow),
R17 (open source first), R38 (secrets).
