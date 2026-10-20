# `hostd` + MCP aggregation — one chain for every execution host

> Terminology (owner 2026-10-20, §0.3): the program formerly called **Linker** is **`hostd`** (Host Daemon, 宿主守护进程).
> Each sandbox runs one `hostd`; the Agent works in a sandbox through its `hostd`. The person's PC and a Mac mini run
> `hostd` the same way. Daytona creates and stops sandboxes. People still run `agentrouter link`, which starts `hostd`.
> "Link" names only the connection (and the `lnk_` id). Renaming the binary, package and this file waits for #80.

Status: **design, owner-reviewed** (2026-10-19, issue #40; 2026-10-20 additions §0.1, issue #64; 2026-10-20 host positioning §0.2, §0.3 + §13, issue #78; owner answers in §12). Nothing here is implemented yet; the
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
  - **Linker** (now `hostd`'s **link layer**, §0.3) — the stable outbound link: identity, auth, reconnect, heartbeat,
    frame relay. It has **no capabilities of its own** (no file, command, git, worktree, deploy or screen action).
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

### 0.1 Owner decisions 2026-10-20: several folders, real paths, 完全访问, SSH feel

> Folder-picker draft (`.artifacts/brand-draft-anime/folder-picker-20261020.html`): 「这个可以」 ·
> 「直接展示真实目录可以不。没必要说限制特别死，因为用户是具备开启完全访问的能力的。虚拟目录可能是个伪需求」 ·
> 「就和Agent使用SSH同样的感觉会更好吧」 (Cloud `docs/product/PRODUCT.md` v1.58)

- **One link, several folders.** `agentrouter link <dir> [<dir>…]`; a folder can be added to a running link with a
  local `y` (§5). The sandbox's writable set is all listed folders + scratch; outside stays denied in folder mode.
- **Real paths, no virtual directory layer.** The Agent sees and uses real paths (`D:\work\my-mod`,
  `D:\Games\Wukong\Mods`); `hello.link.folders` carries them (§3.2). Copy between folders is a plain local
  operation (`cp`, a build's output path): the bytes never pass through the cloud or the model context.
- **完全访问 (full access)**, chosen locally only: `agentrouter link --full` (§5, §6.4). Never switchable from the cloud.
- **SSH feel, MCP underneath.** Each linked computer is like an ssh host named by its link (`台式机`); the Agent runs
  commands in a real shell with a real cwd and reads/writes files by real path through the device's `host` server
  (§3.7); folder mode is like a jailed ssh account, 完全访问 like a normal login. Cross-computer (and cloud sandbox ↔
  computer) transfer is scp-like (`<host>:<path>` → `<host>:<path>`), streamed by the platform outside the model context
  and audited on both ends. The Agent-facing tools are kernel-neutral and minimal (Cloud `remote_hosts`,
  `remote_exec`, `remote_read_file`, `remote_write_file`, `remote_copy`).
- **Several computers in one conversation:** a conversation may bind several links (Cloud `device_link_bindings`).

### 0.2 Owner decision 2026-10-20: Linker = 宿主机 (the host)

> 「是的，linker改成宿主机更合理。然后可以带着默认安装的MCP。然后也可以安装第三方MCP。我们可以维护一个列表，告诉Agent一些推荐的MCP」

Earlier the same day: three independent layers — Agent runtime (DSH on shared cloud Agent hosts) / views / execution
host (Cloud `PROJECT-SERVICE.md`); DSH's native tools reach the session's bound host through our replacement plugin;
「B站这个，其实最好还是第三方MCP提供」 (platform connectors are third-party MCP, not our code; generic MCP integration is
Cloud #2201).

- **The program is the host (宿主机).** It turns any machine into an execution host + view provider: the person's PC,
  a rented Mac mini, later GPU boxes — and **also the inside of every cloud Daytona sandbox**. Daytona keeps only the
  sandbox lifecycle (create / start / stop / snapshot / destroy); the host does execution and views (exec, files with
  versions, stat/list, search, jobs, PTY, change feed, host info: the `host` server, §3.7). **One host protocol for every
  execution host;** the policy differs per placement (§13.1).
- **Default-installed MCP bundle:** the built-in `host` server always; `serena` installed by default and started on first
  use; `files`, `git`, `shell` (DesktopCommander) leave the bundle because `host` covers them (§13.2).
- **Third-party MCP:** from the catalog or added by the person, run under the host's rules, credentials from the vault,
  per-tool ask-first, argument audit (§13.3).
- **Recommended MCP list:** a maintained catalog as data, searchable by Agents through a platform tool; an install
  request goes to the host, which decides by its rules; Agent definitions preselect MCPs; people add their own (§13.4).

### 0.3 Owner decision 2026-10-20 (later): `hostd`

> 「宿主机是不是应该说成 demon啥的」→「可以，按照这个修正吧」 · 「一个沙箱，里面运行一个daemon程序。运行了这个程序，Agent才能在里面干活。」

- The program is **`hostd`** (Host Daemon). Each sandbox runs one `hostd`; the Agent works in a sandbox through its
  `hostd`. The person's PC and a Mac mini run `hostd` the same way. Daytona creates and stops sandboxes.
- In this document "the host" below means `hostd` and the machine or sandbox it runs on.
- **Every place is the same kind of place.** Owner 2026-10-20: 「是不是所有的设备都统一了，统一靠hostd来扩展运行边界。甚至MCP都是一样的，可以在沙箱中运行」 —
  yes. Anything that runs `hostd` (a cloud sandbox, the person's PC, a rented Mac mini) is a place an Agent can work;
  adding a place means running `hostd` there, nothing else. MCP servers are the same everywhere: the default ones and
  third-party ones (e.g. Bilibili) run under `hostd`, in a sandbox or on a PC, with the same catalog, credentials and
  audit (§13).
- **`hostd` runs an environment** (owner 2026-10-20: 「可以，这个合理多了，空间主要负责内容管理。环境负责执行命令。MCP和服务平台作为调用动作。View负责主动/被动为Agent提供信息。」,
  replacing the brief "everything is a space" wording). Four categories: **spaces** = content (作品, memory, Agent
  definitions; Cloud Space Server); **environments** = executing commands (a sandbox, a PC, a Mac mini, each running
  `hostd`: exec, processes, isolation, audit, lifecycle, checkpoints); **actions** = MCP servers under `hostd` (e.g.
  Bilibili) and platform services — side effects, ask-first, rate limits, credentials, audit; **views** = active or
  passive information for the Agent (e.g. a new-comment feed).
- **`hostd` is not a space client.** It does exec, processes, terminals, the machine's local files, isolation, audit
  and MCP. Spaces reach a cloud sandbox through a separate **space client** (Cloud), which `hostd` calls to
  mount views; on a PC the Agent works on real folders, so no space client runs there unless a 作品 is synced to it.
- The one-page picture across Cloud and this repository: Cloud `docs/development/ARCHITECTURE-OVERVIEW.md`.

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
Link layer (host) ─ identity, token, reconnect, heartbeat; demux frames by session
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
| Link layer (host) | Device key, pairing, token, one WebSocket, reconnect with backoff, ping/silence, session demux | Any capability; spawning processes |
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

`hello.link` = `{id, folder: "<display name of the first folder>", folders: ["<real absolute path>", …], full,
os, arch, app: "cli", version, sandbox: "<backend>@<version>" | "", readOnly}`. Running `link` on folders is the
person's choice to show their real paths (owner 2026-10-20, §0.1); `folders` holds at most 32 paths, the first is
the link's home (relative paths resolve there). In 完全访问 `full` is `true`, `folders` lists the folders named on the
command line (checkpointed, §7.2) or the start folder, and `sandbox` is `""` (nothing is confined, §6.4). The `lnk_`
id is derived from device id + the first folder's canonical path (`full:` + it in 完全访问), so adding a folder keeps
the id.

```text
         device → gateway  {"op": "info", "link": {…same fields as hello.link, same id…}}  (the folders changed)
```

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
- `link__folder_add {path, reason?}`: the Agent asks for another folder; the person answers `y/N` on the computer
  (§5). On `y` the folder joins the writable set, every running server restarts with the new policy, `info` goes
  out and the answer lists the folders. Refused: `DENIED_BY_USER`, `FOLDER_INVALID` (missing, a file, the home
  folder, a drive root, the device data folder). `link__servers` also reports `folders`, `full` and `readOnly`.
- `tools/list` results are merged across servers and paginated; a server's `list_changed` becomes the
  aggregator's `list_changed`.

### 3.7 The `host` server: the SSH-like surface (owner 2026-10-20)

Every link starts one local server named `host` (`agentrouter mcp-serve host`, the device's own binary; started by the
console, never by the cloud, so no `y`). It is an ordinary MCP server: confined by the sandbox in folder mode like
any other (§6), run with the person's rights in 完全访问 (§6.4). The aggregator still has no capabilities.

| Tool | Arguments | Answer (`structuredContent`) |
| --- | --- | --- |
| `host__exec` | `command`, `cwd?` (default: the first folder), `timeout_s?` (1–600, default 120) | `exit_code` (null on timeout, cancel or a signal), `signal` (Unix), `timed_out`, `cancelled`, `stdout`, `stderr` (each ≤ 256 KiB, a clip marker inside), `cwd`, `shell`, `ms`; streamed while it runs (below) |
| `host__read` | `path`, `offset?`, `length?` (≤ 4 MiB, default whole file up to that), `encoding?` `utf8`/`base64`, `sha256?` | `path` (absolute, real), `size`, `offset`, `length`, `eof`, `encoding`, `data`, `sha256` (whole file, when asked) |
| `host__write` | `path`, `data`, `encoding?` `utf8`/`base64`, `offset?`, `truncate?` | `path`, `size`, `written` |
| `host__stat` | `paths` (≤ 1000), `content_max?` (≤ 256 KiB) | `entries`: per path `path` (real), `exists`, `kind` `file`/`dir`/`symlink`/`other`, `size`, `mtime_ms`, `readonly`, `symlink?`, `error?`; files up to `content_max` also `data` + `encoding` (utf8, else base64; ≤ 4 MiB per call) |
| `host__list` | `path` or `paths` (≤ 64), `depth?` (1–16, default 1), `hidden?` (default true), `ignore?` (.gitignore/.ignore like ripgrep, default false), `limit?` (default 5000) | `listings`: per folder `path`, `entries` (`name` relative, `kind`, `size`, `mtime_ms`; sorted by name, pre-order), `truncated`; or `error` |
| `host__search` | `mode` `files` (= `rg --files`) / `content` (= `rg --regexp`, default), `pattern`, `path?`, `cwd?`, `globs?` (rg `--glob`, `!` excludes), `hidden?`, `no_ignore?`, `ignore_case?`, `fixed_strings?`, `sort?` `path`/`modified`, `max_results?` (default 10 000), `max_line_bytes?` (default 2000), `timeout_s?` | `root`; files: `paths`; content: `matches` (`path`, `line_number`, `text` without its line ending, or base64 `bytes` for non-UTF-8, `clipped?`), `files`; both `count` (all), `truncated`, `timed_out`. ripgrep's own crates: .gitignore/.ignore/.rgignore inside a git repo, hidden skipped, NUL = binary (walked files skipped, a named file converted), BOM-sniffed UTF-16, Rust regex; paths as `rg` prints them; order = path (or mtime ascending) |
| `host__job_start` | `command`, `cwd?` | `job` (`job-<n>`), `pid`, `cwd`, `shell`: the command keeps running in the background (its own process group / kill-on-close job object) |
| `host__job_read` | `job`, `stdout_offset?`, `stderr_offset?`, `max_bytes?` (≤ 4 MiB, default 1 MiB), `wait_ms?` (≤ 30 000: wait for new output or the end) | `running`, `exit_code`, `signal`, `stdout`/`stderr`: `data` (UTF-8 from the offset), `offset`, `next_offset`, `more`, `dropped` (the last 8 MiB per stream are kept) |
| `host__job_stop` | `job`, `grace_ms?` (default 2000) | the job's summary after its whole tree ended (SIGTERM, then a kill after the grace); it is forgotten |
| `host__jobs` | — | `jobs`: `job`, `command`, `cwd`, `pid`, `running`, `exit_code`, `signal`, `ms`, `stdout_bytes`, `stderr_bytes` (≤ 32 running, 64 remembered) |
| `host__pty_open` | `command?` (default: the person's login shell; PowerShell on Windows), `cwd?`, `cols?` (120), `rows?` (30) | `pty`, `pid`, `cwd`, `shell`: a pseudo-terminal (openpty / ConPTY) for prompts, REPLs, TUIs; ≤ 16 open |
| `host__pty_write` | `pty`, `data` (keys as typed: `\r` Enter, `\u0003` Ctrl+C), `encoding?` | `written` |
| `host__pty_read` | `pty`, `offset?`, `max_bytes?` (default 256 KiB), `wait_ms?` (≤ 30 000) | terminal output from the offset (escape sequences included), `next_offset`, `more`, `dropped`, `running`, `exit_code` |
| `host__pty_resize` / `host__pty_close` | `pty`, `cols`, `rows` / `pty` | the new size / `closed` (the program's whole tree ended) |

- The shell is real: `/bin/sh -lc` on Linux/macOS, PowerShell (`pwsh`, else Windows PowerShell) on Windows; `shell`
  names it. The command's whole process tree ends on timeout (process group / job object). Paths are real; relative ones resolve against the first folder.
- **Streamed output:** a `host__exec` call that carries `_meta.progressToken` gets its stdout/stderr as it arrives, as
  `notifications/progress` (`progress` counts up; `message` = the JSON text of `{"stream": "stdout"|"stderr", "data":
  "<utf-8>"}`, pushed every 100 ms or 32 KiB, characters never split, ≤ 16 MiB per stream). `notifications/cancelled`
  for a running call ends the command's whole process tree (answer `cancelled: true`); so does the timeout.
- **Background jobs and terminals live as long as the link.** When the `host` server's input closes (the link ends, the server is
  stopped: the aggregator closes its input and waits up to 3 s before killing its process group) or it gets a stop
  signal, it cancels every running call and ends every job's and terminal's process tree; on Windows a killed server takes its jobs
  along anyway (kill-on-close job objects).
- `host__write` without `offset` creates or replaces the file (parents created); with `offset` it writes there,
  after truncating to `offset` when `truncate` is true (chunked transfer: first chunk `offset: 0, truncate: true`).
- A write the OS refuses answers `isError` with the OS text (the console shows 拦下：在文件夹外).
- Answers carry the bytes once: `structuredContent` has `data`; the text copy replaces any `data` over 64 KiB with a
  marker, so a 4 MiB base64 chunk fits the 8 MiB frame. `host__read` without `length` reads at most 4 MiB.
- Transfer (`remote_copy`, Cloud) is chunks of `host__read`/`host__write` (4 MiB, base64) relayed by the control
  plane from link to link (or the conversation's cloud sandbox), never through the model; each chunk is one audited
  call on each computer, the copy ends with `sha256` compared on both sides.

### 3.5 Errors

One table, JSON-RPC errors with `data: {code, next}` (`next` = one sentence for the model, as today):
`SIGNATURE_INVALID`, `REQUEST_EXPIRED`, `REPLAYED`, `WRONG_LINK`, `OUTSIDE_FOLDER`,
`SANDBOX_UNAVAILABLE`, `SERVER_STARTING`, `SERVER_NOT_RUNNING`, `SERVER_CRASHED`, `RUNTIME_MISSING`,
`PACKAGE_NEEDED`, `DIGEST_MISMATCH`, `TIMEOUT`, `CANCELLED`, `TOO_LARGE`, `DENIED_BY_USER`,
`LINK_LOST`. A tool's own failure stays a normal `tools/call` result with `isError: true`.

### 3.6 Reconnect and in-flight calls

The link layer reconnects with exponential backoff (1 s → 30 s, jitter) using a fresh challenge/token.
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

- **Starting `link` on folders is the consent** for those folders (owner 2026-10-10: folder-scoped full
  freedom, no prompts, undo as the safety). Inside them the Agent works without questions.
- **Outside the folders a call is denied** (by the OS, §6) — except that **a folder can be added with a local `y`**
  (owner 2026-10-20): the person types `+ <path>` in the link's terminal (typing it is the consent), or the Agent
  calls `link__folder_add` and the terminal asks `y/N` (120 s, no answer = no; `--yes-servers` does not pre-answer
  it). Nothing from the cloud can widen the set without that `y`.
- **完全访问 is chosen on the computer only:** `agentrouter link --full`. There is no tool, frame, flag or env var
  through which the cloud can turn it on or off; ending it means Ctrl+C and running `link` again without `--full`.
- **The only local question** is adding a server that is not a pinned catalog entry
  (`link__server_add` with a custom command): the terminal shows the command line and asks `y/N`;
  no answer in 120 s = no. `--yes-servers` on the command line pre-answers it for unattended machines
  (a local flag, never an environment variable a child or the cloud can set).
- **Deleted:** `AGENTROUTER_CONSENT`, every consent decision stored or asserted in the cloud for device
  calls (§9). The cloud keeps a **binding** ("this conversation uses `desk:my-mod`"), which is a routing
  choice the person makes in the web UI, not a permission.
- Read-only mode: `agentrouter link --read-only` makes the folders read-only in the sandbox too (not combinable
  with `--full`).

## 6. Sandbox — the folder boundary is the OS's

Every MCP server process (and every process it spawns: shells, `git`, interpreters, hooks) runs inside
one OS sandbox scoped to the folder. The aggregator and link layer run unconfined but **never execute
anything from the folder** (no git in the folder, no hooks, no folder config).

### 6.1 Policy (same on every OS)

| Resource | Rule |
| --- | --- |
| Write / delete / rename | Allowed only in the folder and a per-link scratch dir (`TEMP`, `TMP`, npm/uv/pip caches redirected there). Denied everywhere else, including the person's home and the device data dir |
| Write inside the folder, denied anyway | `.git/hooks/**`, `.git/config` (planting code that the person's own git would later run at full rights) |
| Read | Allowed broadly (tools need runtimes and system libraries) **except** a deny list: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, `~/.kube`, `~/.docker/config.json`, `~/.config/gh`, `~/.git-credentials`, `~/.npmrc`, browser profile roots, OS credential stores, the AgentRouter device data dir |
| Network | Allow-list per server from the catalog (`network: [domains]`) plus the package mirrors; custom servers get registries only unless the local `y` included network. **Open web** (owner Q7, 2026-10-20): a pinned catalog preset whose list is `["*"]` (the browser presets `browser`, `browser-firefox`) may reach any public host on any port; loopback/`localhost`, link-local (incl. `169.254.169.254`), multicast, cloud-metadata endpoints, private ranges (`10/8`, `172.16/12`, `192.168/16`, `100.64/10`, `fc00::/7`) and this computer's own addresses stay closed, by name and by IP literal. `*` from a custom server, the project's `.agentrouter/mcp.json` or any other local definition is dropped; no local flag or env var widens it. Mechanism: srt 0.0.80 has no `*` allow entry, so the device starts srt's own CLI through `open-web.mjs`, which passes srt's documented ask callback (`SandboxManager.initialize(config, askCallback)`): it allows public destinations and judges IP literals with srt's resolved-address guard; `network.deniedResolvedAddresses` adds the private ranges to that guard for names. Enforced through the sandbox's egress proxy |
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

### 6.4 完全访问 (`--full`)

The person's own choice to give the Agent a normal login (owner 2026-10-20): the local servers (and the commands they
run) start **without the sandbox**, as the person's own user, with the network unrestricted. Decision on the secret
read deny list: **not kept in 完全访问.** Reasons: (1) on Windows the only backend runs servers as a separate sandbox
account that cannot use the person's rights at all, so "normal rights + deny list" does not exist there; on
macOS/Linux srt could keep it, but then 完全访问 would mean different things per OS; (2) a deny list enforced by
reading arguments (not the kernel) is bypassed by any shell command (`cat ~/.ssh/id_ed25519`), so keeping it would
promise a protection that is not there. The console says it in one line (§8). Kept: argument-level audit (the chain
head still goes to the cloud on every heartbeat, so a rewritten log is detected), checkpoints (§7.2), the local `y`
for custom servers, Ctrl+C. Folder mode is unchanged (deny list, OS boundary). A full link works on a computer whose
sandbox is unavailable (e.g. Ubuntu 24.04 with restricted user namespaces).

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

**Several folders (2026-10-20):** one shadow repository per folder, keyed by the folder's canonical path
(`<data>/checkpoints/f-<hash>.git`), so `agentrouter undo` run in any linked folder undoes that folder's last turn,
whatever link it was linked through. A turn snapshots every linked folder (an unchanged folder costs one tree compare);
the audit line carries the ids (`folder → id`).

**完全访问:** the folders named on the command line (or the start folder, unless it is the home folder or a drive root)
are snapshotted as above. Beyond them, a **file journal** keeps the previous content of each file a tool names by path
before a non-read-only call (`host__write`'s `path`; other servers' `path`/`file_path`/`destination`/`target`
arguments): files up to 64 MiB are copied to `<data>/checkpoints/journal/<turn>/`, a file that did not exist is
recorded as absent. `agentrouter undo --journal [turn]` puts them back (and removes the absent ones). What a shell
command changes outside the snapshotted folders is **not** undoable; the audit holds the command line.

## 8. Console UX

```text
PS D:\work> npx @agentrouter-top/cli link D:\work\my-mod D:\Games\Wukong\Mods
AgentRouter · 链接文件夹
  文件夹   D:\work\my-mod
           D:\Games\Wukong\Mods   云端 Agent 只能在这些文件夹里干活
  账号     这台电脑已链接（台式机）
  沙箱     Windows 隔离账户（srt@0.0.80）
已连上。网页对话里选「台式机」就能用。要再加文件夹，在这里输入 + 路径。Ctrl+C 断开。
12:03:41  改 mod 配置   终端 dotnet build -o D:\Games\Wukong\Mods        ✓ 3.2s
12:04:02  云端想再用一个文件夹：D:\Assets\textures（换贴图）。同意吗？ (y/N，120 秒内不答就是不同意)
```

```text
PS C:\Users\me> agentrouter link --full
AgentRouter · 完全访问
  完全访问 云端 Agent 用你的账户权限，能读写这台电脑上的所有文件（包括密钥文件）。只在你信任的对话里用；审计照记，Ctrl+C 随时断开。
  检查点   C:\Users\me 是主目录，不做文件夹检查点；Agent 写过的文件会先留一份旧版（agentrouter undo --journal）
```

Before (one folder, 2026-10-19):

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

Commands after the change: `link [<dir>…] [--read-only | --full] [--yes-servers]`, `undo [--list] [--journal]`,
`audit [verify]`, `sandbox setup|status|remove`, `status`, `logout` (unpair this computer).
Removed: `workspace …`, `deploy …`, `access …`, `mcp on|off`, `screen on|off`, `run --background`,
`agent list`, `mcp-serve workspace`. (`mcp-serve local` is pending the communication design, §4.)

Default bundle (owner 2026-10-20 「可以带着默认安装的MCP」, decided in §13.2): `host` (§3.7), started with every link,
plus `serena`, installed by default and started on its first call. `files`, `git` and `shell` (DesktopCommander) are
no longer in the bundle: `host` covers them. Before: `files`, `git`, `shell`, `serena` as the bundle; earlier on
2026-10-20: `host` only, everything else on demand.
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
  link layer speaks only `v: 2`; when it is the `next` version, Cloud answers `v: 1` hellos with
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
| Q7 | May the browser presets reach the whole internet? (2026-10-20) | 「可以开放到完整的互联网」 | Accepted (§6.1): browser presets get the open web; the LAN, loopback, link-local/cloud metadata and the PC's own services stay closed; other servers unchanged |
| Q8 | Folder picker draft (2026-10-20) | 「这个可以」 | Approved; extended for several folders / several links / 完全访问 badge, re-shown before the web build (Cloud #2154) |
| Q9 | Several folders and copy between them (2026-10-20) | 「直接展示真实目录可以不。没必要说限制特别死，因为用户是具备开启完全访问的能力的。虚拟目录可能是个伪需求」 | No virtual directory: real paths, several folders per link, 完全访问 by local choice (§0.1, §5, §6.4) |
| Q10 | How the Agent should feel on a linked computer (2026-10-20) | 「就和Agent使用SSH同样的感觉会更好吧」 | Each link is an ssh-like host: real shell, real paths, scp-like transfer; MCP underneath (§3.7) |

## 13. The host everywhere (owner 2026-10-20, §0.2)

### 13.1 `hostd` on a PC, a rented machine, or in a sandbox: a policy per placement

The same program, `hostd` (`agentrouter link`: link layer + aggregator + the `host` server + bundled/third-party servers) runs
wherever an Agent needs hands. Everything in §3 is identical in every placement: frames, signing (§3.3), the `host`
tools and their answers (§3.7), `link__*` management tools, errors, audit format. Only the policy differs:

| | Person's PC | Machine we rent (Mac mini; later GPU boxes) | Cloud Daytona sandbox |
| --- | --- | --- | --- |
| Started by | the person (`agentrouter link …`) | a service on the machine, under a dedicated OS account per tenant lease | the sandbox's entrypoint (`hostd` is baked into the empty snapshot, Cloud PROJECT-SERVICE §7.8) |
| Identity | device key paired by the person (§4) | device key paired by us (ops), leased to one person's session tree at a time | **no pairing:** a key pair generated in the sandbox at start; the control plane registers its public key through the lifecycle call and issues a host token bound to (sandbox id, session tree), revoked at destroy |
| Boundary | folder mode: `srt` OS sandbox (§6); 完全访问: the person's own rights (§6.4) | 完全访问 inside the lease account (the account and the machine are the boundary); wiped between leases | **the sandbox is the boundary:** servers run without `srt`, like 完全访问 inside the container |
| Folders | named by the person; more only with a local `y` (§5) | the lease's working folders | the session's mounted views (workspace manifest, Cloud PROJECT-SERVICE); `link__folder_add` answers `DENIED_BY_USER` |
| Who decides server installs | the person: catalog presets run, custom ones need `y` (§5) | platform policy (as cloud) | platform policy: catalog presets named by the session's definition or the person's list; the person's own custom entries only in that person's sandboxes (§13.3) |
| Network | catalog allow-list per server (§6.1) | catalog allow-list per server | the sandbox's egress policy + the catalog allow-list per server at the node's egress proxy |
| Undo | checkpoints + journal (§7.2) | checkpoints | off: the project service's per-turn changesets are the undo |
| Audit | §7.1 + `audit_head` | same | same (the chain head is the only copy outside the sandbox that matters) |

- `hello.link` gains `placement: "pc" | "machine" | "cloud"` (planned field; Cloud shows hosts by placement and never
  offers a `cloud` host to anyone outside its session tree).
- **What Daytona keeps:** lifecycle (create / start / stop / snapshot / destroy), starting `hostd` at boot, and the
  port proxy for previews until the gateway gives each sandbox its own addresses. Agent execution and files stop using
  Daytona's toolbox (`/toolbox/process/execute`, `/toolbox/files/*` in Cloud `daytona-provider.ts`); they go to `hostd`
  like on a PC. Cloud PROJECT-SERVICE §10.8 is the Cloud side of this split.
- A cloud host dials out to the same gateway endpoint as a PC (`/device/v2/link`), so the relay, the MCP client and
  the DSH plugin have one path; the sandbox needs no inbound port either.

### 13.2 The default-installed bundle, re-evaluated now that `host` exists

| Server | Covered by `host`? | Verdict | Why |
| --- | --- | --- | --- |
| `host` (built-in) | — | **Always**, not removable | exec (streamed, cancel), read/write with versions and atomic guarded writes, stat/list, search (ripgrep's crates), jobs, PTY, info, change feed (§3.7) |
| `files` (`@modelcontextprotocol/server-filesystem`) | yes, fully | **Out of the bundle**; catalog entry kept, not recommended | `host__read/write/stat/list/search` do more (versions, ranges, atomic writes); two file tools for one folder only confuse the model |
| `shell` (DesktopCommander) | yes, fully | **Out**; catalog entry kept, not recommended | `host__exec` + jobs + PTY cover commands, background processes and interactive programs; it also ships its own file tools |
| `git` (`mcp-server-git`) | mostly | **Out**; catalog entry kept | Agents use the `git` CLI through `host__exec` (and know it far better than a tool schema); `host__info.programs.git` says whether it exists |
| `serena` | no | **Kept: installed by default, started on its first call** | LSP-backed symbol search, references, rename and symbol-level edits; `host__search` is text only. Language servers cost seconds and memory, so the aggregator lists its tools from the names the catalog recorded when vetting (no start for `tools/list`) and starts it on the first call. GPL-3.0-or-later: a separate process installed by pin from PyPI, never linked into or redistributed with our binary (as today) |
| `browser` (Playwright MCP) | no | **Default on cloud compute, on demand on PCs** | Checking previews and E2E is routine in the cloud (the browser base layer, Cloud PROJECT-SERVICE §7.8); on a PC it downloads a browser and needs the open web (Q7), so it is installed when an Agent asks |

How they run (unchanged): each server is a child of the aggregator inside the placement's boundary (§13.1), started from
the exact pinned version after the registry digest check, with the catalog's network allow-list. Bundle versions are
pinned in the catalog; a bump is a catalog change with re-vetting (licence, archived?, last release).

### 13.3 Third-party MCP

- **Sources:** a catalog entry (vetted, pinned); an entry the person added (command + arguments, or a remote MCP URL
  through the pinned `mcp-remote` bridge), kept in the person's own list in Cloud; a project's `.agentrouter/mcp.json`
  (custom, §8). Platform connectors (B站, 小红书, …) are third-party servers: we vet and pin them, we do not write or
  fork them (owner 「B站这个，其实最好还是第三方MCP提供」).
- **Install = `link__server_add` on a host,** and the host decides by its placement's rules (§13.1): on a PC a catalog
  entry runs, a custom one needs the local `y`; on a cloud host the platform's policy decides.
- **Same rules as the bundle:** the placement's boundary; network from the catalog, custom entries registries only
  unless the person declared domains when adding (on a PC in the `y` prompt); never `*` for custom (§6.1).
- **Credentials vault:** an entry declares the secrets it needs (`credentials: [{env, label}]`). They live in Cloud's
  vault per person and go to the host only inside the signed start of that server, as that process's environment;
  never in tool arguments or results, never in the audit (redacted, §7.1), never in the model's context. On a PC the
  person may instead keep them local (set in the terminal or the server's own config), then they never leave the PC.
  Revoking or rotating a credential in the web restarts the server.
- **Per-tool ask-first:** for tools that act as the person on an external account (send, post, publish, delete, pay).
  The catalog marks each tool `ask: never | first_per_conversation | always` (default: `never` for `readOnlyHint`
  tools, `first_per_conversation` for the rest); an Agent definition may change it per tool. It is a question about the
  person's external account, not about the computer, so Cloud's MCP client asks in the conversation before sending the
  call; access to the computer stays decided on the computer (§5). Unattended sessions (triggers, public channels) cannot
  answer: such a tool fails with `ASK_UNAVAILABLE` unless the definition sets `never`, which the publish check shows.
- **Argument audit:** every third-party call is in the host's argument-level audit (§7.1) and in Cloud's
  `platform_tool_calls` (server, tool, argument digest, ask decision). Cloud #2201 carries the Cloud side.

### 13.4 Recommended MCP catalog (data, for Agents)

- **One maintained catalog as data,** in Cloud (one file per entry, replacing the constant list in
  `modules/devices/src/linked-devices/mcp-catalog.ts`, which becomes the loader + schema check), edited by us through
  PRs. Per entry: `id`, `title`, `summary` (what it does), `when_to_use` / `when_not`, `platforms` (OS, arch,
  placements), `licence`, `source`, `vetted` (date, last release, archived?), `pin` (runner, package, version, digest,
  package file), `tools` (names, read-only, ask default), `network`, `credentials`, `risks`, `recommended` (+ tags),
  `superseded_by` (e.g. `files` → `host`).
- **Agents see it through one platform tool** (Cloud `mcp_catalog`): `search`, `recommend {task}` (entries with their
  `when_to_use`), `installed {host}`, `install {id, host, reason}` → `link__server_add` on that host, which answers
  installed / refused / waiting for the person's `y`.
- **Agent definitions preselect MCPs** (Cloud AGENT-DEFINITION §3.7); **people add their own** entries (private list;
  sharing later).

### 13.5 Follow-up tasks (to file after #69–#77 land)

| Repo | Task |
| --- | --- |
| device | #80: rename to `hostd` (binary, crates, console text, this file) once #69–#77 merge; `agentrouter link` stays |
| device | `placement: cloud`: `hostd` started from a sandbox entrypoint, platform-issued key + token, no `srt`, no checkpoints, no `y`, `link__folder_add` refused |
| device | Lazy start of bundle/catalog servers from recorded tool names (`serena`) |
| device | Credentials in a signed server start: env for that process only, redacted everywhere |
| cloud | Catalog as data + `mcp_catalog` tool; definitions' `mcp` field compiled into installs (AGENT-DEFINITION §3.7) |
| cloud | Execution-host interface split: lifecycle (Daytona) vs host protocol (PROJECT-SERVICE §10.8) |
| cloud | Vault entries for MCP credentials, ask-first in the MCP client, `ASK_UNAVAILABLE` (#2201) |

### 13.6 Names

The program is **`hostd`** (Host Daemon, 宿主守护进程; owner 2026-10-20, §0.3); "link" names only the connection (`lnk_`
ids, `/device/v2/link`). People keep running `agentrouter link`, which starts `hostd`. Renaming the binary, crates,
console text and this file waits until #69–#77 merge, so those nine PRs do not conflict (#80).
