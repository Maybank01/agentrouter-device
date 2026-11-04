# AgentDevice: the device connector (hostd)

Layer: behind NewAPI (Agent\*), the executor on a person's own computer. Workspace rules first: `AgentTeam/AGENTS.md`
(owner rules apply by name: Open source first, Production hold, Merge path in minutes, No data through command lines,
Specific errors, Root causes). Model: Cloud `docs/development/AGENT-FRAMEWORK.md` (never restated here). Docs: the map
is [docs/README.md](docs/README.md) (framework: Cloud `docs/development/DOCS.md`); `node scripts/check-docs.mjs`
checks it.
Charter (what belongs here, the contract it provides, how Cloud pins it): [docs/CHARTER.md](docs/CHARTER.md).

## What this repo is / is not
- Direction (owner 2026-11-04, Cloud #3976): the client becomes a Go program embedding NetBird (`client/embed` + its
  JWT-authenticated SSH server); the device only executes over SSH, no MCP, no tools of its own. It replaces everything
  below; until it is verified on Dev the bullets below describe the current code. Purpose and boundaries: the charter.
- `hostd` (`agentrouter link`, npm `@agentrouter-top/cli`): a pure remote MCP client that links the user's computer to
  our server, outbound only. It links, relays and installs/supervises the local MCP services; it has no file, command
  or screen tools of its own and opens no inbound port. Crates and roles: [README.md](README.md) "Development".
- Design and link protocol: [docs/HOSTD-MCP.md](docs/HOSTD-MCP.md) (type `protocol`; Cloud and the runner cite its
  section numbers: keep them stable or change the citations in the same release).
- Not here: the Cloud side of the link (gateway, environments; agentrouter-cloud), the executor protocol spec
  (AgentCompute `contracts/executor-protocol/v0`; vendored copy in `vendor/agentcompute/`, tested by `crates/connector-cli/tests/executor_contract.rs`). The desktop shell `app/` is frozen (source kept, outside CI and releases).
- Public repository: never commit secrets, hosts or account names; the parity bench's secrets live in the
  environment `dev-bench`.

## Build, check, test
- Locally build, check and lint only: `cargo build`, `cargo check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo fmt --all`, `bash scripts/av-hygiene.sh`, `node scripts/check-docs.mjs`.
- Tests run on GitHub-hosted runners only (`.github/workflows/ci.yml`: Linux, Windows, sandbox jobs on Linux, macOS and
  Windows; free for a public repository). Required checks and the after-merge `Sandbox (Windows)` rule: README.md
  "Development". A red `main` is re-run once, then reverted or reported (`main-red.yml`).

## Landing
- From the Cloud checkout: `npm run task -- start <n> --repo AgentDevice --agent <name>`, then `npm run task -- pr <n>
  --repo AgentDevice` (label `merge-on-green`, then stop): `.github/workflows/merge-on-green.yml` (a vendored copy of the
  private shared workflow, #216) merges it once the required checks are green. A PR that changes `.github/workflows` is
  merged by hand once green.
- The PR body names the root cause / structure, not only the symptom.

## Release
- Every push to `main` that changes the program publishes a GitHub pre-release `hostd-v<version>-<sha7>`
  (`.github/workflows/hostd.yml`, with `SHA256SUMS`). Cloud pins one in `deploy/versions.json` `components.agentDevice`
  (`follow: manual` until the owner turns on per-release bumps; each bump rebuilds the session image).
- npm: `.github/workflows/npm.yml` packs `@agentrouter-top/cli` + the platform package; publishing needs the npm token
  the owner holds. Version bumps only at release time.
- Code signing is deferred (owner 2026-10-08, `docs/adr/0001-code-signing.md`).

## Open-source base
- MCP: official Rust SDK `rmcp`; sandbox: Anthropic `sandbox-runtime` (pinned); checkpoints: git. Vet licence and
  activity before adding a dependency and say so in the PR.
- `.ignore` keeps lock files and the vendored error catalog out of searches.
