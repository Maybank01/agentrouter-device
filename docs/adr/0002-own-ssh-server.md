# ADR 0002: The Go client serves SSH itself; NetBird is the network only (2026-11-04)

- Status: decided (coordinator, 2026-11-04, on #221, within Cloud ADR 0021 §9's stated fallback for P2/P3)
- Date: 2026-11-04
- Context: base layers v1 (Cloud #3976; owner 「设备端只做执行」, 「装上客户端，云端就有一个按照SSH协议的执行设备」,
  「可以，就用NetBird」, approved 「两处都确认，开工吧」). The design planned NetBird's own SSH server on the device.

## Evidence (P3, #221)

On a real Windows 10 PC, NetBird v0.82.0 embedded in userspace: NetBird's SSH server ran SFTP but **failed every
command and terminal** in every token tried (UAC-filtered interactive, Safer basic user, even elevated). It starts
each command through an S4U logon (`LsaRegisterLogonProcess`), which needs `SeTcbPrivilege`: only LocalSystem has it.
It also treats any member of local Administrators as privileged and refuses everything unless `EnableSSHRoot`. A
prototype of our own server on the same NetBird listener passed exec, an interactive ConPTY terminal and SFTP as the
filtered standard token.

## Decision

- One SSH server, ours, on every OS: `charm.land/ssh` v0.4.3 (BSD-3, fork of gliderlabs/ssh), ConPTY through
  `github.com/charmbracelet/x/conpty` v0.2.0 (MIT) on Windows and a Unix PTY elsewhere, `github.com/pkg/sftp`
  v1.13.12 (BSD-2) in-process; JWTs with `github.com/golang-jwt/jwt/v5` v5.3.1 (MIT) against the identity/v1 JWKS.
  It listens on the device's NetBird address (netstack, port 22). NetBird's SSH server stays off.
- Every session runs as the account the client runs as, on the whole computer (R58): no user switching, no service,
  no administrator rights (R45). The SSH user name is not an authority.
- The password is an identity/v1 token: `aud: netbird`, `scp` covering `computer:<this id>:ssh`, `own` equal to the
  device's owner, `iat` at most 600 s old. The device enforces the scope itself (NetBird's server checks only `sub`);
  AgentCompute's gateway checks it too.
- Port forwarding: local (`direct-tcpip`) to the computer's own loopback services only, which is how a named endpoint
  reaches its program; no remote forwarding, no agent forwarding.
- The host key is the client's (Ed25519, made at first start) and is reported at enrolment (`hostKey`, AgentCompute
  #18), not taken from NetBird's network map.
- A command's text never travels in a command line (R21): it goes into an owner-only file that a fixed runner reads
  and deletes (the runners proven by the Rust hostd).

## Why not the alternatives

- NetBird's server on Unix plus ours on Windows: two paths (R15); `EnableSSHRoot` on most home PCs; NetBird re-launches
  its binary with the command as an argument (the 128 KiB limit, R21).
- A Windows service installed once with UAC: admin at install (R45), S4U sessions without the person's network
  credentials, DPAPI secrets or desktop, and an SSH server running as SYSTEM.
- An upstream patch: not needed for v1; can be proposed later without changing this contract.

## One visible window (Windows; owner decision 2026-11-04)

The client is a tray program (a GUI build). Owner: no hidden windows and no window per command. `agentdevice run`
keeps **one** resident, visible, minimizable console window titled 「AgentRouter 设备」 (`AllocConsole`, or the
terminal it was started from). Its header says 「关闭此窗口即断开连接」 and it shows one line per thing an Agent does
(time, which conversation, the command's first line, exit code). Non-terminal commands start with no console creation
flag, so console programs attach to that window while their streams are pipes; terminals run in a pseudo console
(ConPTY). Closing the window disconnects cleanly. This keeps [AV-HYGIENE.md](../development/AV-HYGIENE.md)'s "no
hidden windows" (its "Go 客户端" section; `scripts/av-hygiene.sh` scans `client/`).

## Consequences

- AgentCompute compute/v1 says the target is the client's SSH server (AgentCompute #18).
- NetBird's static-info probes (WMI, about 20 s on a standard Windows account) are skipped in this client
  (`client/internal/network/staticinfo.go`).
