# client: the Go device client (base layers v1)

**Purpose**: `agentdevice` makes a computer an execution device the cloud reaches by SSH over its owner's private
NetBird network (R45, Cloud #3976). It replaces the Rust `hostd` once verified on Dev; until then both live here.

**Boundary**: network (NetBird `client/embed`, userspace), our own SSH server, login, enrolment, facts, tray,
autostart, device name. Never here: MCP, tools of its own, admin rights, an inbound port on the computer's own network,
placement or environments (AgentCompute, AgentHost). Why our own SSH server: [ADR 0002](../docs/adr/0002-own-ssh-server.md).

**Interfaces**: consumes identity/v1 (agentrouter-newapi; "Device login", RFC 8628: `POST /v1/identity/device/code`,
then `POST /v1/identity/device/token` — the device code once, then `grant_type=refresh_token` for every short token,
`audience=agentcompute` or `netbird`; JWKS for SSH tokens) and compute/v1 (AgentCompute: `POST /v1/devices`,
`PUT /v1/computers/{id}/facts`, `DELETE /v1/devices/{id}`; in a sandbox the join file `sandbox-join.schema.json`,
§7.1); provides SSH on the NetBird address, port 22. Vendored copies with sha256: `testdata/contracts/`.

| Submodule | Current state |
| --- | --- |
| `cmd/agentdevice` | commands `run` (default), `login`, `status`, `autostart on\|off\|status`, `unbind`, `facts`, `sandbox`, `version`; `--home` |
| `internal/app` | the flow: login → enrol → join → SSH + facts + tray state; `sandbox.go`: a sandbox, headless (below) |
| `internal/egress` | relays NetBird's TLS by SNI through an HTTP CONNECT proxy ([ADR 0003](../docs/adr/0003-sandbox-egress-relay.md)) |
| `internal/network` | embed v0.82.0, NetBird's SSH server off; skips NetBird's slow static-info probes (Windows 20 s) |
| `internal/sshd` | exec, terminal (ConPTY / PTY), SFTP, loopback-only forwarding; token auth; command text via files (R21) |
| `internal/identity` | ES256 verifier (contract vectors), JWKS cache; device login: keeps only the newest refresh token (rotated on every use, one refresh at a time, a lost answer retried within the 30 s grace), short tokens per audience |
| `internal/compute` | compute/v1 client; RFC 9457 problems kept whole |
| `internal/facts` | os, arch, shell, cpu, memory, gpu, software; no WMI, no admin |
| `internal/autostart` | systemd user unit, LaunchAgent, Task Scheduler at least privilege; off by default |
| `internal/tray`, `internal/winconsole` | minimal tray; the one visible console window 「AgentRouter 设备」 with the activity log (Windows) |
| `internal/proc`, `internal/state` | child processes (no window, tree kill); the per-user folder and its files |

**Checks**: locally `go build` / `go vet` only (also windows, darwin); tests on GitHub-hosted runners, `client.yml` (R47).

**Sandboxes**: `agentdevice sandbox [--join <file>]` (compute/v1 §7.1): no login, enrolment call, facts or tray.
Linux pre-releases `client-v0.0.0-<sha7>` (static amd64/arm64, `SHA256SUMS`) feed AgentCompute's environment image.

**Removed device**: a refused refresh shows 「此设备已被移除，请重新登录」, forgets the enrolment and stops.

**Known gaps** (2026-11-04): Dev ran a Linux device (device code approved through the session API) and a sandbox
(#223); Dev has no NewAPI web origin for the browser approval page; no Windows PC end to end yet; no release flow for
people; no Windows version resource or icon (AV-HYGIENE.md "Go 客户端"); a sandbox reports no facts.
