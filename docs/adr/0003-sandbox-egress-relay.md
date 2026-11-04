# ADR 0003: A sandbox's client relays NetBird's TLS through the node's proxy (2026-11-04)

- Status: decided (#223, with AgentCompute#27; base layers v1 design §9 P5, Cloud #3976)
- Date: 2026-11-04
- Context: a compute/v1 sandbox runs the same client headless (`agentdevice sandbox`, compute/v1 README §7.1). On the
  Dev Daytona node a sandbox has no direct egress (`AR-SANDBOX-FORWARD` drops it; a direct TCP 443 to NetBird's name
  timed out), only the node's Squid on `172.20.0.1:3128`, which allows CONNECT to public hosts on 443 (it answered).

## Problem

NetBird v0.82.0's management and signal clients are gRPC, but its relay client dials WebSocket with its own
`http.Transport` and no `Proxy` (`shared/relay/client/dialer/ws/ws.go`), so `HTTPS_PROXY` is ignored. Without the relay
the sandbox has no path to any peer (UDP is blocked too). `client/embed` has no option for a dialer or a proxy.

## Decision

- AgentCompute (root, its fixed start script) maps NetBird's host names to 127.0.0.1 in `/etc/hosts` (a marked block,
  written again at every start because Docker rewrites the file) and sets `net.ipv4.ip_unprivileged_port_start=443`.
- The client (`internal/egress`) listens on 127.0.0.1:443, reads each connection's TLS ClientHello server name, refuses
  names the join file does not list, opens `CONNECT <name>:443` at the proxy and copies bytes. TLS stays end to end:
  NetBird checks the real certificate. The client's own HTTPS (JWKS) and its SSH sessions use the proxy variables.

## Rejected

- Allowing sandboxes direct TCP to NetBird's address in the node firewall: a change to every sandbox's isolation,
  bound to an IP that may change; Squid already decides egress by name.
- Patching NetBird's dialer: the client module is one Go module with the AGPL servers; a fork for one `Proxy` line.
- Sandboxes reached only from the gateway's node (design P5 fallback): a second access path beside SSH over NetBird.

## Evidence

Dev 2026-11-04, sandbox `cmp_mer6iox9vouuz0avmao5` on node `dev-1`: the client joined in 0.3 s through the relay
(`joined ip=100.122.228.44 … management=true`), SSH listened on the NetBird address, compute/v1 reported `online`.
