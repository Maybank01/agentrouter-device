# Linker + MCP aggregation — the one device chain (console mode)

Status: **design, owner-reviewed** (2026-10-19, issue #40; owner answers in §12). Nothing here is implemented yet; the
phased tasks at the end are filed as `agent-ready` issues. This document supersedes the device-action
parts of Cloud `docs/product/workspace-v1/DEVICE-PROTOCOL.md` (§5, §13, §18) and `LINKED-DEVICES.md`
(§23) once Phase 3 lands; until then live behaviour follows those documents.

## 0. Owner decision

Owner, 2026-10-19:

> 「客户端本体可以先不做，但是可以先落地控制台模式。就是我能够在任意文件夹启动，然后云端Agent可以控制这个文件夹。优先把协议这些收敛成一条清晰的链路。我想要的是Linker+MCP聚合。链接负责稳定链接，MCP负责提供云端Agent可以在本地做的事」

Read as:

- **No desktop shell now.** `app/` (Tauri) is frozen: out of CI and releases, source kept.
- **Console mode.** The person runs one command in any folder
  (`npx @agentrouter-top/cli link`, or `agentrouter link`); the cloud Agent can act **in that folder
  only**, for as long as the command runs. Ctrl+C ends it.
- **Two layers, nothing else.**
  - **Linker** — the stable outbound link: identity, auth, reconnect, heartbeat, frame relay. It has
    **no capabilities of its own** (no file, command, git, worktree, deploy or screen action).
  - **MCP aggregation** — everything the cloud Agent can do locally comes from local MCP servers,
    aggregated into **one MCP endpoint**, every server process confined by the OS to the started folder.
- **One protocol end to end: MCP JSON-RPC.** No custom device action set.

It does not change the 2026-10-18 decision (the client is a pure remote MCP client); it removes what grew
around it since (signed device actions, built-in workspace/exec/deploy/worktree tools, three consent
stores, the M2a device-project registry).

Owner answers to the open questions, 2026-10-19 (details in §12):

> Q1 「屏幕操作可以先关掉」 · Q2 「需要设计一下或者找一下合理的开源项目。主要是我怕DS设计的架构有问题」 ·
> Q3 「分享码功能也先移除，只做本地和云端个人账户的链接」 · Q4 「可以」 · Q5 「可以」 · Q6 「可以」

- Screen tools are **off** (now on today's chain too, not only in console mode).
- Share codes are **removed**: a link connects a folder to the person's **own** cloud account only.
- Local Agents and cloud↔local Agent messaging are **pending a separate communication design**; nothing
  in this document builds on them.

## 1. Why: the structural causes this replaces

| Today (device `main` @ #38, Cloud `dev`) | Structural problem |
| --- | --- |
| Two protocols: signed device actions (`info`, `mcp_*`, `workspace_*`, `worktree_*`) **and** MCP inside `mcp_call` | Two models of "what the device can do"; every new capability needed both |
| Built-in `mcp-serve workspace` / `mcp-serve local` (exec, deploy, worktrees, confinement) | The connector grew its own capabilities again, contradicting "link only" |
| Access level gates only the files server's `allowedDirectories`; `mcp_start` / `start_process` run anything as the full user (#39) | **No enforced boundary**: the folder is advisory |
| Low-integrity confinement only for the built-in `exec`, not for other MCP servers; MSYS2 tools do not start low; the connector itself runs `git` at medium integrity inside the folder (worktrees, checkpoints) | A low process can plant `.git/hooks`, `core.fsmonitor` or filter config that the medium connector then executes (#39 "escape via git hooks") |
| `AGENTROUTER_CONSENT=allow` is read from the MCP child's environment, which the cloud supplies in `mcp_start.env` | The cloud can switch local consent off (#39) |
| Consent in four places: device dialogs, `device_consent_requests` (#1961), `resource_grants` device rows + web consent cards (#1981/#1983, `platform_tool_calls.result.answered`), the ui-stream pending Map | One fact ("may this be done here") stored and decided in several places, partly by the side that should not decide it |
| Device `audit.log` records `<server>.<tool>` only, no arguments | An incident cannot be answered from the PC (#39) |
| Checkpoints write hidden refs into the person's own repo via their repo config | Same escape class; metadata-only outside git |

## 2. The chain

```text
cloud Agent (kernel in an Agent sandbox)
  │  platform tool call  e.g.  desk:my-mod  files__write_file {path, content}
  ▼
Control plane · devices module ─ MCP client (official TypeScript SDK), one MCP session per
  │                               (conversation, link); signs each request in params._meta
  │  MCP Streamable HTTP (internal, gateway key)
  ▼
Gateway · link relay ─ maps (link, MCP session) ⇄ the link's WebSocket; no decisions, no capabilities
  │  WebSocket (device dials out; TLS; no inbound port)   frame = {s, m}  where m is a JSON-RPC message
  ▼
Linker (device) ─ identity, token, reconnect, heartbeat; demux frames by session
  │  in-process
  ▼
MCP aggregator (device, rmcp) ─ verifies the signature, local policy, argument-level audit,
  │                              checkpoint before non-read-only calls, routes `<server>__<tool>`
  │  stdio MCP (official Rust SDK client)
  ▼
MCP servers (files, git, shell, serena, …) ─ each launched by the sandbox launcher:
     OS-enforced: write = the folder + a scratch dir; read denied on secrets; network by allow-list
```

The person's terminal shows the link state and a one-line feed of every call (§8).

### 2.1 What each layer owns (and never does)

| Layer | Owns | Never |
| --- | --- | --- |
| Control plane (devices) | Which conversation uses which link (a binding, not consent); the catalog of servers (pins, digests, network allow-lists); vault secrets for server env; `platform_tool_calls` | Decide local policy; assert consent; hold device-side state |
| Gateway | Authenticate links, relay frames, close revoked links, per-link connection uniqueness | Sign, inspect arguments, add tools |
| Linker | Device key, pairing, token, one WebSocket, reconnect with backoff, ping/silence, session demux | Any capability; spawning processes |
| Aggregator | MCP server side towards the cloud, MCP client side towards the servers; signature check; policy; audit; checkpoints; server lifecycle | Its own file/command/git tools (only the reserved management tools in §3.4) |
| Sandbox launcher | Turning a server definition + the folder into an OS-confined process tree | Policy decisions beyond the folder rules in §6 |

## 3. Protocol

### 3.1 Transport

- **Device ⇄ gateway:** one WebSocket per running `link` (the device dials out:
  `wss://<gateway>/device/v2/link`). Text frames, max 8 MiB, gateway ping every 20 s, device treats 75 s
  of silence as a dead link (kept from today's `net.rs`).
- **Control plane ⇄ gateway:** MCP **Streamable HTTP** (spec 2025-06-18 or newer, official TypeScript SDK
  `StreamableHTTPClientTransport`) at an internal endpoint `/internal/links/<lnk_id>/mcp`. The
  `Mcp-Session-Id` the gateway issues is bound to one conversation.
- **Aggregator ⇄ servers:** stdio MCP (official Rust SDK `rmcp` client, `TokioChildProcess`).

MCP allows custom transports; the WebSocket leg is a transport, not a protocol: it carries MCP JSON-RPC
messages unchanged inside a two-field envelope.

### 3.2 Frames on the link

```text
data     {"s": "<mcp session id>", "m": <JSON-RPC 2.0 message, unchanged>}
control  device → gateway  {"op": "hello", "v": 2, "token": "ldt_…", "link": {…}}       (first frame, ≤10 s)
         gateway → device  {"op": "welcome", "link": "lnk_…"} | {"op": "error", "code"} | {"op": "revoked", "reason"}
         either way        {"op": "close", "s": "<session>"}                              (session ended)
         device → gateway  {"op": "audit_head", "seq": n, "hash": "<sha256>"}            (§7, every heartbeat)
```

`hello.link` = `{folder: "<display name>", os, arch, app: "cli", version, sandbox: "<backend>@<version>"}`.
The absolute folder path stays on the device unless the person chose to show it; the cloud sees a name
(`my-mod`) and a stable `lnk_` id derived from device id + canonical path.

That is the entire custom surface. Everything else is MCP: `initialize`,
`notifications/initialized`, `tools/list`, `tools/call`, `notifications/tools/list_changed`,
`notifications/cancelled`, `notifications/progress`, `ping`.

### 3.3 Signing (keeps today's trust model inside MCP)

Today only the control plane can sign device requests, so a broken gateway cannot forge them
(DEVICE-PROTOCOL §1). That stays, carried in MCP's own extension point `params._meta`:

```json
{"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
  "name": "files__write_file", "arguments": {"path": "src/a.txt", "content": "…"},
  "_meta": {"agentrouter.top/sig": {"v": 3, "kid": "…", "link": "lnk_…", "session": "ags_…",
            "digest": "<sha256(canonical JSON of params without _meta)>", "exp": 1760000060000,
            "nonce": "<32 hex>", "sig": "<Ed25519 base64url>"}}}}
```

Signed string: `agentrouter-link-request/v3\n<link>\n<session>\n<method>\n<digest>\n<exp>\n<nonce>`.
Canonical JSON, key format, expiry window (60 s + 5 s skew) and the replay cache are unchanged from
`protocol.rs`. Signed: `initialize` (binds the MCP session to the conversation) and every `tools/call`.
Unsigned (harmless, read-only shape): `tools/list`, `ping`, notifications. Failure → JSON-RPC error
`-32001` with `data.code` `SIGNATURE_INVALID` / `REQUEST_EXPIRED` / `REPLAYED` / `WRONG_LINK`.

### 3.4 Tools the aggregator exposes

- Every server tool as `<server>__<tool>` (the MCP-safe spelling already used by the cloud,
  `device__<server>__<tool>` minus the `device__` prefix, which the control plane adds back). Tool
  `annotations` (`readOnlyHint`, `destructiveHint`) are passed through; they drive checkpoints (§7.2).
- Reserved management server `link` (implemented by the aggregator, still plain `tools/call`):
  `link__servers` (list with state and tools count), `link__server_add` (a catalog preset with its pin,
  or a custom definition — custom ones need the local `y` in §5), `link__server_remove`,
  `link__server_logs`, `link__package_put` (pinned package bytes, digest-checked; replaces
  `mcp_package`). These manage *which* servers run; they are not capabilities.
- `tools/list` results are merged across servers and paginated; a server's `list_changed` becomes the
  aggregator's `list_changed`.

### 3.5 Errors

One table, JSON-RPC errors with `data: {code, next}` (`next` = one sentence for the model, as today):
`SIGNATURE_INVALID`, `REQUEST_EXPIRED`, `REPLAYED`, `WRONG_LINK`, `OUTSIDE_FOLDER`,
`SANDBOX_UNAVAILABLE`, `SERVER_STARTING`, `SERVER_NOT_RUNNING`, `SERVER_CRASHED`, `RUNTIME_MISSING`,
`PACKAGE_NEEDED`, `DIGEST_MISMATCH`, `TIMEOUT`, `CANCELLED`, `TOO_LARGE`, `DENIED_BY_USER`,
`LINK_LOST`. A tool's own failure stays a normal `tools/call` result with `isError: true`.

### 3.6 Reconnect and in-flight calls

The Linker reconnects with exponential backoff (1 s → 30 s, jitter) using a fresh challenge/token.
Aggregator sessions survive a link drop for 120 s (same `s` resumes); calls in flight when the link
drops are answered `LINK_LOST` and **not** replayed by the device; the control plane retries only tools
with `readOnlyHint: true`. Servers keep running across a reconnect.

## 4. Identity

| Who | Identity | Where it lives |
| --- | --- | --- |
| Computer + OS user ("device") | Ed25519 key pair, paired once by code in the browser (unchanged §3–§4 of DEVICE-PROTOCOL) | Private key in DPAPI / Keychain / libsecret; never leaves the device |
| One running console ("link") | `lnk_<hash(device id, canonical folder)>` | Announced in `hello`; the gateway keeps at most one connection per link (a new one replaces it, close code 4409), several links per device are fine |
| Conversation ("session") | MCP session bound to `ags_…` by the signed `initialize` | Aggregator memory; ends on `close` or 120 s after link loss |
| Control plane | Pinned signing key (`kid`) | Device config |

A link belongs to exactly one person's own cloud account (the paired device's owner). There are no
share codes and no links into another person's account (Q3).

**Pending the communication design (Q2):** the local-Agent entry (`mcp-serve local`, `lag_`/`lses_`,
#37/#38) and any cloud↔local Agent messaging. The owner asked for a proper design (or a mature open
source project) before anything is built here, because the current architecture is not trusted. Until
that design is decided: no task builds on `dispatch/v1` `external_agent` or `ext_…` sessions, no task
extends `mcp-serve local`, and the deletion task (#49) leaves `mcp-serve local` and the modules only it
needs in place rather than deciding its fate.

## 5. Consent — decided on the device only

- **Starting `link` in a folder is the consent** for that folder (owner 2026-10-10: folder-scoped full
  freedom, no prompts, undo as the safety). Inside it the Agent works without questions.
- **Outside the folder nothing is asked — it is denied** (by the OS, §6). No prompt can widen the
  boundary at run time; a person who wants another folder runs `link` there.
- **The only local question** is adding a server that is not a pinned catalog entry
  (`link__server_add` with a custom command): the terminal shows the command line and asks `y/N`;
  no answer in 120 s = no. `--yes-servers` on the command line pre-answers it for unattended machines
  (a local flag, never an environment variable a child or the cloud can set).
- **Deleted:** `AGENTROUTER_CONSENT`, every consent decision stored or asserted in the cloud for device
  calls (§9). The cloud keeps a **binding** ("this conversation uses `desk:my-mod`"), which is a routing
  choice the person makes in the web UI, not a permission.
- Read-only mode: `agentrouter link --read-only` makes the folder read-only in the sandbox too.

## 6. Sandbox — the folder boundary is the OS's

Every MCP server process (and every process it spawns: shells, `git`, interpreters, hooks) runs inside
one OS sandbox scoped to the folder. The aggregator and Linker run unconfined but **never execute
anything from the folder** (no git in the folder, no hooks, no folder config).

### 6.1 Policy (same on every OS)

| Resource | Rule |
| --- | --- |
| Write / delete / rename | Allowed only in the folder and a per-link scratch dir (`TEMP`, `TMP`, npm/uv/pip caches redirected there). Denied everywhere else, including the person's home and the device data dir |
| Write inside the folder, denied anyway | `.git/hooks/**`, `.git/config` (planting code that the person's own git would later run at full rights) |
| Read | Allowed broadly (tools need runtimes and system libraries) **except** a deny list: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker/config.json`, `~/.config/gh`, `~/.git-credentials`, `~/.npmrc`, browser profile roots, OS credential stores, the AgentRouter device data dir |
| Network | Allow-list per server from the catalog (`network: [domains]`) plus the package mirrors; custom servers get registries only unless the local `y` included network. Enforced through the sandbox's egress proxy |
| Processes | Whole tree in the sandbox; killed with the link (job object / process group) |

### 6.2 Tool choice

| Candidate | Licence | Activity (checked 2026-10-19) | Verdict |
| --- | --- | --- | --- |
| **Anthropic `sandbox-runtime` (`srt`)** | Apache-2.0 | Active: v0.0.80 on 2026-10-16, 5.6k stars | **Chosen.** Built to sandbox "agents, local MCP servers, bash commands and arbitrary processes"; one policy format on all three OSes: macOS `sandbox-exec` (Seatbelt), Linux bubblewrap + network namespace, Windows a dedicated `srt-sandbox` local account + per-session explicit ACEs on the working tree + WFP egress fence. Used as a process wrapper (`srt --settings <file> -- <cmd>`), so no linking and a narrow contract. Risks: pre-1.0 (pin exact version + integrity like a catalog package, re-vet on each bump); needs Node (present for `npx` users; bundled later if a shell returns); Windows needs a one-time elevated setup (Q4) |
| OpenAI Codex sandbox crates (`codex-rs/windows-sandbox-rs`, `linux-sandbox`, Seatbelt) | Apache-2.0 | Very active (rust-v0.163.0, 2026-10-15) | **Fallback.** Rust, same primitives (Windows: restricted token/sandbox identity + ACLs + WFP; Linux: Landlock + seccomp/bwrap). Not published as a library with a stable API; vendoring a fast-moving monorepo crate is the cost |
| Today's low-integrity token + Low labels (S2b, `confine.rs`) | ours | — | **Deleted.** Covers only built-in `exec`; MSYS2 programs do not start low; labels the person's folders; escape via medium-integrity git |
| Windows AppContainer / LPAC directly (`windows` crate) | MIT/Apache-2.0 | Active | Not chosen now: strongest primitive, but node/python/uv installed per-user and most MCP servers break without many brokered exceptions; revisit if `srt` Windows proves insufficient |
| Windows Sandbox (Hyper-V) | OS | — | Rejected: not on Windows Home (the owner's PC), a VM per server |
| Sandboxie-Plus | GPL-3.0 | Active | Rejected: kernel driver install, copyleft |
| bubblewrap / Landlock (`rust-landlock`) directly | LGPL-2.0+ / MIT-Apache-2.0 | Active (bwrap v0.13.1 2026-10-14) | Used **through** `srt` on Linux, not directly |

No fallback to "unconfined": if the sandbox is not available (setup refused, unsupported filesystem,
Linux without user namespaces), the link still connects, `tools/list` contains only `link__*`, and every
call answers `SANDBOX_UNAVAILABLE` with a `next` that tells the person what to run
(`agentrouter sandbox setup`).

### 6.3 Per OS notes

- **Windows:** first `link` runs `agentrouter sandbox setup` (one UAC prompt: create the `srt` sandbox
  account and WFP filters). The sandbox account cannot read the person's profile at all by default
  (stronger than the read deny list). Runtimes installed per-user (`%APPDATA%\npm`, `~\.local\bin`,
  nvm/fnm) get explicit read+execute ACEs, recorded and removed by `agentrouter sandbox remove`.
  FAT/exFAT folders and network shares cannot carry ACEs: `SANDBOX_UNAVAILABLE` for them.
- **macOS:** Seatbelt profile generated per link; no setup step.
- **Linux:** needs `bwrap` and unprivileged user namespaces (Ubuntu 24.04+ AppArmor may block them; the
  `next` says how to allow it).
- Screen (Windows-MCP) cannot be confined to a folder and is **off** (owner Q1 「屏幕操作可以先关掉」):
  not in the catalog, `screen on` / `--screen` removed. A separately started screen mode can be designed
  later; it is not planned.

## 7. Audit and undo

### 7.1 Audit (append-only, argument-level, on the PC)

- One line per JSON-RPC request the aggregator receives and one per answer, in
  `<data>/audit/link-<lnk>.jsonl`: time, link, session (`ags_`), method, server, tool,
  **arguments** (canonical JSON; values of secret env keys and fields named like secrets replaced by
  `[secret]`; strings over 8 KiB stored as sha256 + length + first 512 bytes), signature nonce,
  decision (allowed / denied + code), duration, result summary (`isError`, content types and sizes,
  sha256 of the result), checkpoint id.
- Plus every server start/stop with its full command line (no env values) and the sandbox policy hash,
  and sandbox violations reported by the backend where it reports them (macOS violation log).
- Tamper evidence: the existing hash chain (`audit.rs`), the file opened append-only by the
  unconfined aggregator and outside every sandbox's write set, and the chain head sent to the cloud
  on every heartbeat (`audit_head`) — a rewritten local log no longer matches the head the platform
  stored. `agentrouter audit` shows, `agentrouter audit verify` checks both.

### 7.2 Checkpoints and undo

Before the first call in a turn to a tool **without** `readOnlyHint: true`, the aggregator snapshots the
folder into a **shadow git repository outside the folder** (`<data>/checkpoints/<lnk>.git`, `--work-tree`
= the folder, `.gitignore` respected, size cap), with hooks, fsmonitor and all repo/global config
disabled (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` empty, `-c core.hooksPath=` ,
`-c core.fsmonitor=false`). It never touches the person's own `.git`. This is the pattern Claude Code
and Cline use for checkpoints. `agentrouter undo` restores the last turn (or `--list`). Deletes inside
the folder are therefore recoverable; deletes outside cannot happen.

## 8. Console UX

```text
PS D:\work\my-mod> npx @agentrouter-top/cli link
AgentRouter · 链接这个文件夹
  文件夹   D:\work\my-mod   云端 Agent 只能在这个文件夹里干活
  账号     第一次使用：打开 https://agentrouter.top/device 输入 ABCD-EFGH
  本机工具 文件 · Git · 终端 · 代码理解     沙箱：Windows 隔离账户
已连上。网页对话里选「my-mod」就能用。Ctrl+C 断开。
12:03:41  改 mod 配置   文件 写入 config/items.json            ✓
12:03:44  改 mod 配置   终端 dotnet build                       ✓ 3.2s
12:03:50  改 mod 配置   文件 写入 C:\Users\me\.ssh\config      拦下：在文件夹外
```

Commands after the change: `link [--read-only] [--yes-servers]`, `undo [--list]`,
`audit [verify]`, `sandbox setup|status|remove`, `status`, `logout` (unpair this computer).
Removed: `workspace …`, `deploy …`, `access …`, `mcp on|off`, `screen on|off`, `run --background`,
`agent list`, `mcp-serve workspace`. (`mcp-serve local` is pending the communication design, §4.)

Default servers (catalog bundle, pinned): `files` (`@modelcontextprotocol/server-filesystem` on the
folder), `git` (`mcp-server-git` on the folder), `shell` (DesktopCommander, confined), `serena`.
Others (`browser`, `bilibili`, …) on demand through `link__server_add`, each with its network
allow-list. A project may add servers in `.agentrouter/mcp.json` (the common `mcpServers` format);
those count as custom and need the local `y`.

## 9. What is deleted

**Device repo (`agentrouter-device`)**

- The signed device action set and its dispatcher: `protocol.rs` request v1/v2 strings and actions,
  `device.rs` action routing, `info`/`mcp_*`/`workspace_*`/`worktree_*` actions (→ MCP + `_meta`).
- Home-grown MCP client `mcp.rs` (≈1.5k lines) → `rmcp` client; `mcp_pkg.rs` folds into `link__package_put`.
- Built-in servers: `mcp-serve workspace` in `connector-cli/src/mcp_serve.rs` and `exec.rs`, `shell.rs`,
  `deploy.rs` (S2a deploy targets, owner Q5 「可以」), `workspace.rs`, `worktree.rs`, `lockfile.rs`.
  `mcp-serve local` and `local_agent.rs` (plus whatever only they need) stay until the communication
  design decides (§4, Q2).
- Confinement S2b: `confine.rs`, `confined.rs` (with a one-time cleanup that removes the Low labels it
  recorded in `confined.json`).
- `AGENTROUTER_CONSENT`, access levels `confirm`/`full` (only folder and read-only remain), native
  consent dialogs in `consent.rs` (terminal `y/N` replaces them), `screen.rs` + `screen/` (Q1, turned
  off earlier by #51).
- In-repo checkpoint refs (`refs/agentrouter/cp/*` in the person's repo) → shadow repo.
- `e2e/device-m1-driver.mjs` → the new E2E (Phase 4). `app/` frozen (out of CI/release), not deleted.

**Cloud (`agentrouter-cloud`)**

- `modules/devices/src/linked-devices`: the per-action tools in `tool.ts` (`exec`, `job`, `read_file`,
  `write_file`, edit/patch/search/list tools), `edits.ts`, `screen.ts`, the action signing in
  `protocol.ts` (→ `_meta` signing), the `mcp_*` relays in `mcp-host.ts` (→ SDK MCP client),
  `device-stub.mjs` (→ a stub aggregator), the `workspace` catalog preset and its rollout gate.
- Consent duplicates for devices: `device_consent_requests` (#1961, migration down), device rows in
  `resource_grants` as a gate, the device/MCP consent cards (`device-consent.tsx`, the device branch of
  `tool-consent.tsx`) and their `PUT …/sessions/:id` answer route, `platform_tool_calls.result.answered`
  for device calls (#1981/#1983), the pending-consent entries in the ui-stream Map. (`runtime_tool_consents`
  stays: it serves runtime grants outside devices.)
- M2a (#1949, direction rejected): `projects.ts`, `device_projects` table, `agent_sessions.device_project_id`
  and `project_kind 'device'`, the binding's workspace block, routes `/personal/devices/projects`, flag
  `CONTROL_PLANE_DEVICE_WORKSPACES`.
- Gateway `device-gateway.mjs` rpc/console-event frames (→ link relay). The gateway console stream is
  replaced by the device audit + `audit_head` + `platform_tool_calls`.
- Docs: DEVICE-PROTOCOL §5, §13, §18 and LINKED-DEVICES §23 shrink to a pointer to this document.

- Shares (分享码, DEVICE-PROTOCOL §11–§17, flag `CONTROL_PLANE_DEVICE_SHARES`): removed now on the old
  protocol (owner Q3), not rebased onto the link — `shares.ts`, `share-vectors.json` /
  `make-share-vectors.mjs`, the share routes and web UI, share routing in platform tools,
  `linked_device_shares` (migration down), the flag, the stub's share frames, DEVICE-PROTOCOL §11–§17.
  The device repo carries no share code. Screen: the Windows-MCP catalog entry and `screen.ts` go off
  now (Q1).

## 10. Migration

- The client is on the `next` channel only and parked behind Cloud, so the cutover is a **hard switch**
  with no long dual-stack: Cloud ships the new relay and MCP client behind `CONTROL_PLANE_DEVICE_LINK_V2`
  (Dev on, prod off) while the old path still serves `v: 1` devices; the first device release with the
  Linker speaks only `v: 2`; when it is the `next` version, Cloud answers `v: 1` hellos with
  `DEVICE_OUTDATED` (`next`: update with `npx @agentrouter-top/cli@next link`) and the deletion PRs land.
- Data: `device_mcp_servers` rows (per-device installs) are dropped — the server set is per link and
  decided on the device; `linked_devices`, pairing and tokens are kept (same device keys, no re-pairing).
- The S2b Low labels on folders linked before are removed by the first new release (`confined.json`).
- Security (#39) lands first and on the old host too: Phase 1 puts every MCP server spawned by today's
  `mcp.rs` through the sandbox launcher and removes `AGENTROUTER_CONSENT`, so the boundary does not wait
  for the protocol switch.

## 11. Phased tasks (one PR, one module each)

| # | Phase | Repo · module | Task |
| --- | --- | --- | --- |
| 1 | 1 Security base | device · `sandbox` (new crate) | Sandbox launcher on `srt` (pinned, digest-checked), policy from §6.1, `agentrouter sandbox setup/status/remove`; today's `mcp.rs` spawns every server through it; no-sandbox = refuse |
| 2 | 1 | device · `connector-cli` + `mcp.rs` | Remove `AGENTROUTER_CONSENT` and cloud-supplied consent; strip reserved env names from `mcp_start.env` |
| 3 | 1 | device · `audit` | Argument-level append-only audit (§7.1) incl. server command lines, redaction, `audit` / `audit verify` |
| 4 | 2 Link | device · `linker` (new crate from `net.rs`) | `/device/v2/link`, `hello v2` with `lnk_`, envelope `{s, m}`, reconnect/heartbeat, `audit_head`; no capabilities |
| 5 | 2 | device · `aggregator` (new crate, `rmcp`) | MCP server towards the link, clients towards servers, `<server>__<tool>`, `link__*` management tools, `_meta` signature check, `list_changed`, cancel/progress passthrough |
| 6 | 2 | device · `checkpoint` | Shadow-git checkpoints outside the folder (hooks/config off) driven by tool annotations; `undo` |
| 7 | 2 | device · `connector-cli` | Console UX (§8): one command, live feed, `--read-only`, `--yes-servers`, local `y/N` for custom servers |
| 8 | 3 Cloud | cloud · `runtime-gateway` | Link relay: `/device/v2/link` WebSocket + internal Streamable HTTP endpoint per link, per-link uniqueness, `audit_head` store; behind `CONTROL_PLANE_DEVICE_LINK_V2` |
| 9 | 3 | cloud · `devices` | MCP client via the official TS SDK per (conversation, link), `_meta` signing (`protocol.ts` v3 + shared test vectors), tools forwarded as `device__<server>__<tool>`, conversation↔link binding; behind the flag |
| 10 | 3 | cloud · `devices` + web | Delete the device consent duplicates (§9) — the binding replaces them |
| 11 | 3 | cloud · `devices` + `agent-sessions` | Delete M2a (registry, column, routes, flag, migration down) |
| 12 | 4 Cut over | device · whole repo | Delete the old chain (§9 device list), freeze `app/` out of CI, README rewrite |
| 13 | 4 | cloud · `devices` + docs | Delete the old action tools/relays/stub, `v: 1` → `DEVICE_OUTDATED`, DEVICE-PROTOCOL/LINKED-DEVICES pointers |
| 14 | 4 | device · CI | E2E on GitHub-hosted Windows/macOS/Linux runners: stub gateway + real aggregator + sandbox escape cases (write/delete outside the folder, read `~/.ssh`, plant `.git/hooks`, child-process write, network to a non-listed host) |

Issues (all `agent-ready`, not started): device 1 #42, 2 #43, 3 #44, 4 #45, 5 #46, 6 #47, 7 #48,
12 #49, 14 #50; Cloud (Maybank01/agentrouter-cloud) 8 #1997, 9 #1998, 10 #1999, 11 #2000, 13 #2001.
From the owner answers, independent of the phases and doable now on today's chain: device #51 (screen
off), Cloud #2005 (screen off: Windows-MCP catalog entry + `screen.ts`), Cloud #2006
(remove device share codes).

Order: 1–3 first (they close #39 on today's chain), then 4–7 and 8–9 in parallel against the shared test
vectors, then 10–14. Not filed, pending the communication design (Q2): local Agents on the link and
cloud↔local Agent messaging.

## 12. Owner answers (2026-10-19)

| # | Question | Owner | Result |
| --- | --- | --- | --- |
| Q1 | Screen: Windows-MCP cannot be confined to a folder; off in console mode? | 「屏幕操作可以先关掉」 | **Off**, now (device #51, Cloud #2005). A screen mode is not planned |
| Q2 | Local Agents (`mcp-serve local`, #37/#38) and cloud↔local Agent communication | 「需要设计一下或者找一下合理的开源项目。主要是我怕DS设计的架构有问题」 | **Not decided.** A separate communication design is running. Local-Agent entry and cross-Agent messaging are marked pending (§4); nothing builds on `dispatch/v1` `external_agent` |
| Q3 | Shares (分享码): keep or delete? | 「分享码功能也先移除，只做本地和云端个人账户的链接」 | **Removed.** Only links between the PC and the person's own cloud account (§4, §9; Cloud #2006) |
| Q4 | One UAC prompt on the first Windows `link` to create the sandbox account | 「可以」 | Accepted (§6.3) |
| Q5 | Deploy outside the folder goes away; run `link` in the target folder instead | 「可以」 | Accepted (§9: `deploy.rs` deleted) |
| Q6 | Network default: catalog domains + package mirrors; custom servers registries only | 「可以」 | Accepted (§6.1) |
