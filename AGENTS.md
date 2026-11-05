# AgentDevice: the device client (agentdevice)

Layer: behind NewAPI (Agent\*), the device side of the compute layer. Workspace rules first: `AgentTeam/AGENTS.md`
(owner rules apply by name: Open source first, Production hold, Merge path in minutes, No data through command lines,
Specific errors, Root causes) and the client pack (`workspace/packs/client.md`: R45, R47, R53, R58). Docs: the map is
[docs/README.md](docs/README.md) (framework: Cloud `docs/development/DOCS.md`); `node scripts/check-docs.mjs` checks it.
Charter (what belongs here, contracts consumed, how it is pinned): [docs/CHARTER.md](docs/CHARTER.md).

## What this repo is / is not
- The client (owner 2026-11-04 「设备端只做执行」, Cloud #3976): a Go program embedding NetBird `client/embed`
  (userspace) with our own JWT-authenticated SSH server ([ADR 0002](docs/adr/0002-own-ssh-server.md)); the computer
  joins its owner's private network and only executes what arrives over SSH. Its own: device-code login, enrolment and
  facts (AgentCompute `compute/v1`), device name, tray, autostart. Modules: [client/README.md](client/README.md).
- Never here: MCP, file / command / screen tools of its own, per-folder sandboxes or access levels (R58), placement,
  environments, NetBird management or the SSH gateway (AgentCompute), accounts and tokens (AgentIdentity). The Rust
  `hostd` / `agentrouter link` and its MCP chain were deleted on 2026-11-05 (#236): never restore them; their source is
  tag `hostd-final`. The DSH plugin / Desktop-adapter client is abandoned (R46).
- Public repository: never commit secrets, hosts or account names; the Windows end-to-end check's secret lives in the
  environment `dev-device-e2e`.

## Build, check, test
- Locally build and check only, in `client/`: `go build ./...`, `go vet ./...` (also `GOOS=windows` / `darwin`),
  `gofmt -l .`; at the root `bash scripts/av-hygiene.sh`, `node scripts/check-docs.mjs`, `node
  scripts/check-repo-imports.mjs`.
- Tests run on GitHub-hosted runners only (R47): `.github/workflows/ci.yml` (`Repository checks`, `Client (Linux)`,
  `Client (macOS)`, `Client (Windows)`: the required checks). A red `main` is re-run once, then reverted or reported
  (`main-red.yml`). The Windows end-to-end check runs on demand (`windows-e2e.yml`, README.md "Development").

## Landing
- From the Cloud checkout: `npm run task -- start <n> --repo AgentDevice --agent <name>`, then `npm run task -- pr <n>
  --repo AgentDevice` (label `merge-on-green`, then stop): `.github/workflows/merge-on-green.yml` (a vendored copy of the
  private shared workflow, #216) merges it once the required checks are green; no review (workspace rule R60). A PR that
  changes `.github/workflows`, or one blocked only by a check that cannot run, is admin-merged by hand once its own
  checks are green (R60). Changing a required check's name changes the branch protection and `REQUIRED` in
  `merge-on-green.yml` together.
- The PR body names the root cause / structure, not only the symptom.

## Release
- Every push to `main` that changes `client/` publishes a GitHub pre-release `client-v0.0.0-<sha7>` (`ci.yml`, with
  `SHA256SUMS`): static Linux builds for compute/v1 sandboxes (AgentCompute's environment image pins one) and a Windows
  build with Dev's endpoints. No release for people yet (no signing, no installer, no channel): it is a known gap.
- Code signing is deferred (owner 2026-10-08, `docs/adr/0001-code-signing.md`). Version bumps only at release time (R14).
- npm `@agentrouter-top/cli` was `hostd`'s channel; nothing here packs it any more. Its published versions stay until
  the base cutover deprecates them (agentteam-deployments `docs/operations/base-v1-production-cutover.md` 5.8).

## Open-source base
- NetBird `client/embed` (BSD-3), `charm.land/ssh`, `pkg/sftp`, `golang-jwt` (ADR 0002). Vet licence and activity
  before adding a dependency and say so in the PR.
- `.ignore` keeps `go.sum` and the vendored contract copies out of searches.
