# client: the Go device client (base layers v1)

**Purpose**: `agentdevice` makes a computer an execution device the cloud reaches by SSH over its owner's private
NetBird network (R45, Cloud #3976). It replaced the Rust `hostd` (deleted 2026-11-05, #236).
**Boundary**: network (NetBird `client/embed`, userspace), our own SSH server, login, enrolment, facts, tray,
autostart, device name. Never here: MCP, tools of its own, admin rights, an inbound port on the computer's own network,
placement or environments (AgentCompute, AgentHost). Why our own SSH server: [ADR 0002](../docs/adr/0002-own-ssh-server.md).

**Interfaces**: consumes identity/v1 (AgentIdentity "Device login": RFC 8628 at ZITADEL, confirmed on Login v2's
`/ui/v2/login/device`, then each short token = exchange at the STS, `aud` agentcompute or netbird; STS JWKS for SSH) and
compute/v1 (`POST /v1/devices`, `PUT /v1/computers/{id}/facts`, `DELETE /v1/devices/{id}`; sandbox join file, §7.1);
provides SSH on the NetBird address, port 22 (vendored, sha256: `testdata/contracts/`). **Endpoints**: `environment.json`
beside the program (the installer's environment, [release/](../release/README.md)), then `config.json` over it. **Login**:
`zitadel {url, projectId, clientId}` + `issuer` = STS (`exchangeUrl` default `<issuer>/v1/identity/exchange`; unbind
revokes at ZITADEL); without it NewAPI's `/v1/identity/device/*` (legacy, deleted after agentrouter-newapi#168).

| Submodule | Current state |
| --- | --- |
| `cmd/agentdevice` | commands `run` (default; one per folder), `login`, `status` (also the update check), `autostart on [--no-start]\|off\|status`, `unbind`, `stop`, `uninstall` (the uninstaller's), `facts`, `sandbox`, `connect` (ProxyCommand), `version`; `--home` |
| `internal/app` | the flow: login → enrol → join → SSH + facts + tray state; `sandbox.go`: a sandbox, headless (below) |
| `internal/egress` | relays NetBird's TLS by SNI through an HTTP CONNECT proxy ([ADR 0003](../docs/adr/0003-sandbox-egress-relay.md)) |
| `internal/network` | embed v0.82.0, NetBird's SSH server off; skips NetBird's slow static-info probes (Windows 20 s) |
| `internal/sshd` | exec, terminal (ConPTY / PTY), SFTP, loopback-only forwarding; token auth; command text via files (R21) |
| `internal/identity` | ES256 verifier (contract vectors; account ids are the contract's one pattern, identity/v1 1.1.0 `acc_[0-9A-Za-z_-]{1,64}`, tested against the vendored schema), JWKS cache; device login: keeps only the newest refresh token (stored before use, one refresh at a time), ZITADEL's access token in memory only, short tokens per audience (exchanged at the STS; an `invalid_grant` there refreshes once and exchanges again; a refused refresh = removed device) |
| `internal/compute` | compute/v1 client; RFC 9457 problems kept whole |
| `internal/facts` | os, arch, shell, cpu, memory, gpu, software; no WMI, no admin |
| `internal/autostart` | systemd user unit, LaunchAgent, Task Scheduler at least privilege; off by default |
| `internal/tray`, `internal/winconsole` | minimal tray; the one visible console window 「AgentRouter 设备」 with the activity log (Windows) |
| `internal/proc`, `internal/state`, `internal/instance`, `internal/update` | child processes (no window, tree kill); the per-user folder, `environment.json` + `config.json`; the run lock (one client per folder) and ending the running client by its path; the newer-release notice (tells, never replaces) |

**Checks**: locally `go build` / `go vet` only (also windows, darwin); tests on GitHub-hosted runners, `ci.yml` (R47).
**Sandboxes**: `agentdevice sandbox [--join <file>]` (compute/v1 §7.1): no login, enrolment call, facts or tray; Linux
pre-releases `client-v0.0.0-<sha7>` (static amd64/arm64, `SHA256SUMS`) feed AgentCompute's environment image.
**Removed device** (a refused refresh, or compute/v1 `COMPUTE_COMPUTER_NOT_FOUND` asked when the network drops, #231):
「此设备已被移除，请重新登录」, SSH sessions closed, login ended at ZITADEL, enrolment forgotten, stopped.
**Known gaps** (2026-11-05): Windows end to end only on a hosted runner (#231, #243 `windows-e2e.yml`), where the tray
cannot register, so the tray is untested on a real desktop; ZITADEL rotates refresh tokens without grace (a lost refresh
answer = log in again); no production endpoints yet (release/README.md); the logon task `AgentDevice` is named per
computer (a second Windows account on one PC cannot turn autostart on); no version resource or icon in the program.
