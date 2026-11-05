# client: the Go device client (base layers v1)

**Purpose**: `agentdevice` makes a computer an execution device the cloud reaches by SSH over its owner's private
NetBird network (R45, Cloud #3976). It replaces the Rust `hostd` once verified on Dev; until then both live here.
**Boundary**: network (NetBird `client/embed`, userspace), our own SSH server, login, enrolment, facts, tray,
autostart, device name. Never here: MCP, tools of its own, admin rights, an inbound port on the computer's own network,
placement or environments (AgentCompute, AgentHost). Why our own SSH server: [ADR 0002](../docs/adr/0002-own-ssh-server.md).

**Interfaces**: consumes identity/v1 (AgentIdentity "Device login": RFC 8628 at ZITADEL, confirmed on Login v2's
`/ui/v2/login/device`, then each short token = exchange at the STS, `aud` agentcompute or netbird; STS JWKS for SSH) and
compute/v1 (`POST /v1/devices`, `PUT /v1/computers/{id}/facts`, `DELETE /v1/devices/{id}`; sandbox join file, §7.1);
provides SSH on the NetBird address, port 22 (vendored, sha256: `testdata/contracts/`). **Login** by `config.json`:
`zitadel {url, projectId, clientId}` + `issuer` = STS (`exchangeUrl` default `<issuer>/v1/identity/exchange`; unbind
revokes at ZITADEL); without it NewAPI's `/v1/identity/device/*` (legacy, deleted after agentrouter-newapi#168).

| Submodule | Current state |
| --- | --- |
| `cmd/agentdevice` | commands `run` (default), `login`, `status`, `autostart on\|off\|status`, `unbind`, `facts`, `sandbox`, `connect` (ProxyCommand through the environment's proxy), `version`; `--home` |
| `internal/app` | the flow: login → enrol → join → SSH + facts + tray state; `sandbox.go`: a sandbox, headless (below) |
| `internal/egress` | relays NetBird's TLS by SNI through an HTTP CONNECT proxy ([ADR 0003](../docs/adr/0003-sandbox-egress-relay.md)) |
| `internal/network` | embed v0.82.0, NetBird's SSH server off; skips NetBird's slow static-info probes (Windows 20 s) |
| `internal/sshd` | exec, terminal (ConPTY / PTY), SFTP, loopback-only forwarding; token auth; command text via files (R21) |
| `internal/identity` | ES256 verifier (contract vectors), JWKS cache; device login: keeps only the newest refresh token (stored before use, one refresh at a time), ZITADEL's access token in memory only, short tokens per audience (exchanged at the STS; an `invalid_grant` there refreshes once and exchanges again; a refused refresh = removed device) |
| `internal/compute` | compute/v1 client; RFC 9457 problems kept whole |
| `internal/facts` | os, arch, shell, cpu, memory, gpu, software; no WMI, no admin |
| `internal/autostart` | systemd user unit, LaunchAgent, Task Scheduler at least privilege; off by default |
| `internal/tray`, `internal/winconsole` | minimal tray; the one visible console window 「AgentRouter 设备」 with the activity log (Windows) |
| `internal/proc`, `internal/state` | child processes (no window, tree kill); the per-user folder and its files |

**Checks**: locally `go build` / `go vet` only (also windows, darwin); tests on GitHub-hosted runners, `client.yml` (R47).
**Sandboxes**: `agentdevice sandbox [--join <file>]` (compute/v1 §7.1): no login, enrolment call, facts or tray.
Linux pre-releases `client-v0.0.0-<sha7>` (static amd64/arm64, `SHA256SUMS`) feed AgentCompute's environment image.
**Removed device** (a refused refresh, or compute/v1 `COMPUTE_COMPUTER_NOT_FOUND` asked when the network drops, #231):
「此设备已被移除，请重新登录」, SSH sessions closed, login ended at ZITADEL, enrolment forgotten, stopped.

**Known gaps** (2026-11-05): Windows end to end only on a hosted runner (#231, `windows-e2e.yml`: non-admin account,
ZITADEL device login, SSH/SFTP, removal) where the tray cannot register (elevated Explorer), so the tray is untested on
a real Windows desktop; ZITADEL rotates refresh tokens without grace (a lost refresh answer = log in again); no release
flow for people; no Windows version resource or icon (AV-HYGIENE.md "Go 客户端"); a sandbox reports no facts.
