# client: the Go device client (base layers v1)

`agentdevice` makes a computer an execution device the cloud reaches by SSH over its owner's private NetBird network
(R45, Cloud #3976). It replaces the Rust `hostd` once verified on Dev; until then both live in this repository.

- **Network**: NetBird `client/embed` v0.82.0 in userspace; no admin rights, no driver, no inbound port
  (`internal/network`; the 20 s Windows startup delay is skipped there).
- **SSH**: its own server on the NetBird address, port 22 (`internal/sshd`): exec, terminal, SFTP, local
  forwarding to this computer's loopback services; identity/v1 token as password. Why: [ADR 0002](../docs/adr/0002-own-ssh-server.md).
- **Enrolment**: device-code login to the NewAPI account (`internal/identity`), compute/v1 enrol / facts / unbind
  (`internal/compute`, `internal/facts`).
- **Person-facing**: tray (`internal/tray`), autostart per user without admin (`internal/autostart`), device name.
- State in the per-user config folder (`internal/state`): `config.json` (endpoints), `device.json`, `credential`,
  `ssh_host_ed25519_key`, `netbird.json`, `agentdevice.log`.
- Contracts: vendored copies with sha256 in `testdata/contracts/` (identity/v1, compute/v1).

Known gaps (current state, 2026-11-04): not yet run end to end on Dev (needs AgentCompute#14 and the NewAPI Dev
issuer with device-code login, agentrouter-newapi#157); no release flow yet (CI artifacts only); sandboxes' headless
enrolment is not here; `scripts/av-hygiene.sh` does not scan `client/` (CREATE_NO_WINDOW, ADR 0002).

Commands: `agentdevice [run]`, `login`, `status`, `autostart on|off|status`, `unbind`, `facts`, `version`
(`cmd/agentdevice/main.go`).

Locally only build and vet (R47):

```sh
go build ./... && go vet ./...
GOOS=windows go vet ./... && CGO_ENABLED=0 GOOS=darwin go vet ./...
```

Tests run on GitHub-hosted Linux, macOS and Windows runners (`.github/workflows/client.yml`). The Windows build is a
GUI program (`-ldflags "-H windowsgui"`); from a terminal it attaches to that console.
