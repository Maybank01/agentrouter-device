# client: the Go device client (base layers v1)

**Purpose**: `agentdevice` makes a computer an execution device the cloud reaches by SSH over its owner's private
NetBird network (R45, Cloud #3976). It replaces the Rust `hostd` once verified on Dev; until then both live here.

**Boundary**: network (NetBird `client/embed`, userspace), our own SSH server, login, enrolment, facts, tray,
autostart, device name. Never here: MCP, tools of its own, admin rights, an inbound port on the computer's own network,
placement or environments (AgentCompute, AgentHost). Why our own SSH server: [ADR 0002](../docs/adr/0002-own-ssh-server.md).

**Interfaces**: consumes identity/v1 (agentrouter-newapi: JWKS, `POST /v1/identity/token`; device-code login proposed
in agentrouter-newapi#157) and compute/v1 (AgentCompute: `POST /v1/devices`, `PUT /v1/computers/{id}/facts`,
`DELETE /v1/devices/{id}`); provides SSH on the NetBird address, port 22. Vendored copies with sha256:
`testdata/contracts/`.

| Submodule | Current state |
| --- | --- |
| `cmd/agentdevice` | commands `run` (default), `login`, `status`, `autostart on\|off\|status`, `unbind`, `facts`, `version`; `--home` |
| `internal/app` | the flow: login → enrol → join → SSH + facts + tray state |
| `internal/network` | embed v0.82.0, NetBird's SSH server off; skips NetBird's slow static-info probes (Windows 20 s) |
| `internal/sshd` | exec, terminal (ConPTY / PTY), SFTP, loopback-only forwarding; token auth; command text via files (R21) |
| `internal/identity` | ES256 verifier (contract vectors), JWKS cache, token client, RFC 8628 login |
| `internal/compute` | compute/v1 client; RFC 9457 problems kept whole |
| `internal/facts` | os, arch, shell, cpu, memory, gpu, software; no WMI, no admin |
| `internal/autostart` | systemd user unit, LaunchAgent, Task Scheduler at least privilege; off by default |
| `internal/tray`, `internal/winconsole` | minimal tray; the Windows GUI build attaching to a parent console |
| `internal/proc`, `internal/state` | child processes (no window, tree kill); the per-user folder and its files |

**Checks**: locally only `go build ./...` and `go vet ./...` (also `GOOS=windows`, `CGO_ENABLED=0 GOOS=darwin`);
tests on GitHub-hosted Linux, macOS, Windows in `.github/workflows/client.yml` (R47).

**Known gaps** (2026-11-04): not run end to end on Dev (needs AgentCompute#14 and the NewAPI Dev issuer with
device-code login); no release flow (CI artifacts only); sandboxes' headless enrolment not here;
`scripts/av-hygiene.sh` does not scan `client/` (`CREATE_NO_WINDOW`, ADR 0002, owner to confirm).
