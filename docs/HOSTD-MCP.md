# `hostd` + MCP aggregation — one chain for every execution host

> Terminology (owner 2026-10-20, §0.3): the program formerly called **Linker** is **`hostd`** (Host Daemon, 宿主守护进程).
> Each sandbox runs one `hostd`; the Agent works in a sandbox through its `hostd`. The person's PC and a Mac mini run
> `hostd` the same way. Daytona creates and stops sandboxes. People still run `agentrouter link`, which starts `hostd`.
> "Link" names only the connection (and the `lnk_` id). The crate is `crates/hostd-link` (`agentrouter-hostd-link`), the
> CLI also answers to `agentrouter hostd`, and this file was `docs/LINKER-MCP.md` (#80).

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
         device → gateway  {"op": "load", cpu_percent, load1, free_memory_mb, free_disk_mb,
                            environments, free_slots}                                  (every heartbeat)
```

`hello.link` = `{id, folder: "<display name of the first folder>", folders: ["<real absolute path>", …], full,
os, arch, app: "cli", version, sandbox: "<backend>@<version>" | "", readOnly}`. Running `link` on folders is the
person's choice to show their real paths (owner 2026-10-20, §0.1); `folders` holds at most 32 paths, the first is
the link's home (relative paths resolve there). In 完全访问 `full` is `true`, `folders` lists the folders named on the
command line (checkpointed, §7.2) or the start folder, and `sandbox` is `""` (nothing is confined, §6.4). The `lnk_`
id is derived from device id + the first folder's canonical path (`full:` + it in 完全访问), so adding a folder keeps
the id.

**Capacity and labels (Cloud VISION §4 D1).** The platform places work by what `hostd` reports and never estimates it
(`agentrouter_device::capacity`). `hello.link` and `host__info` also carry:
- `capacity` = `{cores, physical_cores, memory_mb, disk_mb, gpus: [{vendor, name, memory_mb}], environments}`.
  `disk_mb` is the disk that holds the home folder. GPUs are NVIDIA ones from `nvidia-smi` (any OS) and Apple silicon.
  `environments` is how many Agents' workplaces this host takes at once: `config.json` `max_environments`, else one
  per two cores and 4 GiB of memory, at least one.
- `labels` = `{os, arch, tools, runtimes, custom}`. `tools` and `runtimes` are the names a task's `requires` uses,
  found on PATH. `custom` holds the owner's own labels from `config.json` `labels` (`gpu`, `rack-2`, 「我的渲染机」).
- `capacity.limits` (device #127): the owner's ceiling (`ceiling`) and how this computer enforces limits
  (`enforcedBy`, `unenforced`, `why`), §6.6.
- `pool` (Cloud VISION §4 D2, #2576): the pool the owner puts this host in, `config.json` `pool` (「渲染机」, `rack-2`),
  sent in `hello.link` only when set (1–40 characters). Control records it with the host; a definition admits it as
  `pool:<name>` in `reach.environment.allow`. Unset = the person's default pool.

The `load` frame goes with every heartbeat. An environment is a conversation with an open MCP session on this
host. `load1` is null on Windows. `free_slots` = `environments` capacity minus running ones.

```text
         device → gateway  {"op": "info", "link": {…same fields as hello.link, same id…}}  (the folders changed)
```

**Events (Cloud PROJECT-SERVICE §9.8.2, #2739).** Every event the host produces for a space (a feed row, a runtime's
event, later a device space's file change) is written first into the host's **outbox**: SQLite in hostd's data
directory (`outbox/<link>.sqlite`, WAL, `synchronous=FULL`; `agentrouter_hostd_link::outbox`), with the host's own
monotonic seq and the event id `<producer>:<seq>` (`<producer>` = the link id + a random id of that database, so a
reinstall never repeats an id). After each `welcome` the link sends it in seq order, one batch at a time (≤ 200 items,
≤ 1 MiB; a payload is ≤ 32 KiB):

```text
         device → gateway  {"op": "events", "host": "<producer>", "items": [{seq, id, space, instance?, event | table,
                            payload, key?, cause?, at}]}
         gateway → device  {"op": "events_ack", "host": "<producer>", "seq": n, "refused": [{seq, code, message}],
                            "retry_ms"?}
```

An item is a declared **event** of the space (`event`, its payload) or a **row** of one of its tables (`table`, the
row as `payload`; a feed's rows): exactly one of the two. `seq` is the highest entry the platform is done with (appended, a duplicate, or refused for good); those leave the
outbox, refused ones are kept (last 500) for the status. A partial ack sends the rest again after `retry_ms` (default
5 s); a batch without an ack within 60 s, or cut by a dropped link, is sent again — the platform dedupes by event id.
Past the disk cap (256 MiB) a producer's write is refused `OUTBOX_FULL` and it pauses; nothing accepted is dropped.

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
  `mcp_package`; a preset sent with its `lock` installs from that instead, §6.5). These manage *which* servers run; they are not capabilities.
- A server's log (`link__server_logs`: its last 200 lines) is its stderr, the MCP log messages it sends, and the
  aggregator's own start / handshake / ready / exit lines, secrets blanked. The same lines are appended to
  `<device data>/logs/server-<name>.log` (moved to `.log.1` at 2 MiB), so a server that never got ready can be
  diagnosed after the link ended (Cloud #2703).
- `link__limits {memoryMb?, processes?, cpuPercent?}`: the environment's resource limits, applied live (§6.6).
- `link__feeds {instance, space, server, feeds}` (platform only, Cloud #2739 step 2): the feeds of a space's runtime
  instance on this host (Cloud PROJECT-SERVICE §9.8.5), as the Space Server describes them plus the columns of the
  feed's table (`{name, tool, args?, everyMs, key, items?, extract?: {json}, into: <table>, columns: {column: type},
  backoff?: {on, waitMs}}`); `feeds: []` stops them, an unchanged set keeps its timers. The generic feed runner
  (`agentrouter_aggregator::feeds`) calls the tool of the local server `server` on its timer (±20 % jitter); takes the
  items from `structuredContent` (at `items`, else the result must be the list) or JSON inside a text result at
  `extract.json` — anything else is `FEED_OUTPUT_UNSTRUCTURED` / `FEED_ITEMS_NOT_A_LIST`, prose is never parsed; keeps
  of each item the table's columns (a number for a `text` column becomes text); and writes each item whose key the feed
  has not seen into the outbox (§3.2) as a **row** of `into` (`table` instead of `event`) with key `<feed>:<item key>`
  and cause `feed:<instance>/<feed>@<ms>`. The platform upserts rows by the table's key; the space raises
  `<table>.inserted|updated` once per row when it lands — a feed raises no event of its own. The seen-set lives in the outbox database (`seen`, newest 20,000 keys per feed),
  marked in the same transaction as the write, so a restart never sends a row twice and a refused write
  (`OUTBOX_FULL`) leaves the key for the next poll. Errors carrying a code of `backoff.on` (HTTP status, JSON-RPC code,
  the server's own `code`; `-412` matches `412`) pause the feed for `waitMs` (`backing_off`, `FEED_BACKING_OFF`); other
  errors wait `every` doubled per failure (at most an hour beyond it, `FEED_TOOL_FAILED`); a nearly full outbox pauses
  polling (`paused`); a server that is not running yet is `waiting` (`FEED_SERVER_NOT_READY`). Each feed's status
  (`state`, `nextAt`, `until`, `written`, `skipped`, `error {code, message}`) is in `link__servers.feeds` and the answer;
  a change of state is one audit line and one terminal line, never one per poll. Feeds are kept in memory: the platform
  sends them again after every welcome, with the server.
- `link__folder_add {path, reason?}`: the Agent asks for another folder; the person answers `y/N` on the computer
  (§5). On `y` the folder joins the writable set, every running server restarts with the new policy, `info` goes
  out and the answer lists the folders. Refused: `DENIED_BY_USER`, `FOLDER_INVALID` (missing, a file, the home
  folder, a drive root, the device data folder). `link__servers` also reports `folders`, `full` and `readOnly`.
- `tools/list` results are merged across servers and paginated; a server's `list_changed` becomes the
  aggregator's `list_changed`.

### 3.7 The `host` server: the SSH-like surface (owner 2026-10-20)

Every link starts one local server named `host` (`agentrouter mcp-serve host`, the device's own binary; started by the
console, never by the cloud, so no `y`). It is an ordinary MCP server: confined by the sandbox in folder mode like
any other (§6), run with the person's rights in 完全访问 (§6.4). The aggregator still has no capabilities. It is also
what a bound cloud session uses as its local machine (below).

| Tool | Arguments | Answer (`structuredContent`) |
| --- | --- | --- |
| `host__exec` | `command`, `cwd?` (default: the first folder), `timeout_s?` (1–600, default 120) | `exit_code` (null on timeout, cancel or a signal), `signal` (Unix), `timed_out`, `cancelled`, `stdout`, `stderr` (each ≤ 256 KiB, a clip marker inside), `cwd`, `shell`, `ms`; streamed while it runs (below) |
| `host__read` | `path`, `offset?`, `length?` (≤ 4 MiB, default whole file up to that), `encoding?` `utf8`/`base64`, `sha256?` | `path` (absolute, real), `size`, `offset`, `length`, `eof`, `encoding`, `data`, `version`, `mtime_ms`, `sha256` (whenever the whole file was read; else when asked), whole text files also `line_ending` (`lf`/`crlf`/`mixed`/null) and `bom` |
| `host__write` | `path`, `data`, `encoding?` `utf8`/`base64`, `offset?`, `truncate?`, `expected?` `{version, sha256?}` / `{absent: true}`, `executable?` (whole-file writes, Unix: mode 755 / 644; without it a replaced file keeps its mode) | `path`, `size`, `written`, `created`, `version`, `mtime_ms`, `executable` (Unix), `sha256` (whole-file writes); a failed guard is `isError` with `structuredContent` `{code: STALE_VERSION / EXISTS, message, version}` |
| `host__remove` | `path`, `expected?` `{version, sha256?}` (a file: only that version) | `path`, `removed`, `kind` (`file` / `folder` / `symlink`); a missing path is `removed: false, exists: false`. Exactly what is named, inside the linked folders in every mode (also 完全访问), never a linked folder itself, never recursive (a folder with entries: `NOT_EMPTY`), a symlink is removed itself, not followed; `OUTSIDE_FOLDER` / `STALE_VERSION` as coded `isError`. Cross-platform (before it the space sync ran `rm` through `exec`, which breaks in PowerShell; Cloud #2383 / #2246). Not read-only: checkpointed and audited like `write` |
| `host__stat` | `paths` (≤ 1000), `content_max?` (≤ 256 KiB) | `entries`: per path `path` (real), `exists`, `kind` `file`/`dir`/`symlink`/`other`, `size`, `mtime_ms`, `readonly`, `executable` (files, Unix), `version` (files and folders), `symlink?` + `link` (its target as written), `error?`; files up to `content_max` also `data` + `encoding` (utf8, else base64; ≤ 4 MiB per call) |
| `host__list` | `path` or `paths` (≤ 64), `depth?` (1–16, default 1), `hidden?` (default true), `ignore?` (.gitignore/.ignore like ripgrep, default false), `limit?` (default 5000) | `listings`: per folder `path`, `entries` (`name` relative, `kind`, `size`, `mtime_ms`, `executable` (files, Unix), `link` (symlinks: the target); sorted by name, pre-order), `truncated`; or `error` |
| `host__search` | `mode` `files` (= `rg --files`) / `content` (= `rg --regexp`, default), `pattern`, `path?`, `cwd?`, `globs?` (rg `--glob`, `!` excludes), `hidden?`, `no_ignore?`, `ignore_case?`, `fixed_strings?`, `sort?` `path`/`modified`, `max_results?` (default 10 000), `max_line_bytes?` (default 2000), `timeout_s?` | `root`; files: `paths`; content: `matches` (`path`, `line_number`, `text` without its line ending, or base64 `bytes` for non-UTF-8, `clipped?`), `files`; both `count` (all), `truncated`, `timed_out`. ripgrep's own crates: .gitignore/.ignore/.rgignore inside a git repo, hidden skipped, NUL = binary (walked files skipped, a named file converted), BOM-sniffed UTF-16, Rust regex; paths as `rg` prints them; order = path (or mtime ascending) |
| `host__job_start` | `command`, `cwd?`, `wait_ms?` (≤ 600 000), `tag?` (the caller's label, ≤ 128, shown by `host__jobs`), `lease?` (`{instance, epoch}`: the platform's control lease the job runs under, §3.9) | `job` (`job-<n>`), `pid`, `cwd`, `shell`: the command keeps running in the background (its own process group / kill-on-close job object). With `wait_ms` it first waits for the end, streaming output as progress like `host__exec`, and also answers `running`, `exit_code`, `signal`, `ms`, `stdout`/`stderr` (as `job_read`: the whole output when a stream is ≤ 1 MiB — progress is for watching, the answer is the record; the end's output is never only in progress) and `forgotten` (it ended inside the wait and all output was answered: nothing to stop); cancelling the call while it waits ends the job (Cloud #2307: one call for a short command, no cap for a long one) |
| `host__job_read` | `job`, `stdout_offset?`, `stderr_offset?`, `max_bytes?` (≤ 4 MiB, default 1 MiB), `wait_ms?` (≤ 30 000: wait for new output or the end) | `running`, `exit_code`, `signal`, `stdout`/`stderr`: `data` (UTF-8 from the offset), `offset`, `next_offset`, `more`, `dropped` (the last 8 MiB per stream are kept) |
| `host__job_stop` | `job`, `grace_ms?` (default 2000), `instance?` (stop only when the job runs under that instance's lease, else `LEASE_MISMATCH`), `reason?` (answered back) | the job's summary after its whole tree ended (SIGTERM, then a kill after the grace); it is forgotten |
| `host__jobs` | — | `jobs`: `job`, `command`, `tag`, `cwd`, `pid`, `lease`, `running`, `exit_code`, `signal`, `ms`, `stdout_bytes`, `stderr_bytes` (≤ 32 running, 64 remembered) |
| `host__pty_open` | `command?` (default: the person's login shell; PowerShell on Windows), `cwd?`, `cols?` (120), `rows?` (30) | `pty`, `pid`, `cwd`, `shell`: a pseudo-terminal (openpty / ConPTY) for prompts, REPLs, TUIs; ≤ 16 open |
| `host__pty_write` | `pty`, `data` (keys as typed: `\r` Enter, `\u0003` Ctrl+C), `encoding?` | `written` |
| `host__pty_read` | `pty`, `offset?`, `max_bytes?` (default 256 KiB), `wait_ms?` (≤ 30 000) | terminal output from the offset (escape sequences included), `next_offset`, `more`, `dropped`, `running`, `exit_code` |
| `host__pty_resize` / `host__pty_close` | `pty`, `cols`, `rows` / `pty` | the new size / `closed` (the program's whole tree ended) |
| `host__info` | — | `os`, `family`, `arch`, `os_version`, `shell` (`pwsh`/`powershell`/`sh`) + `shell_program`, `path_separator`, `path_list_separator`, `line_ending` (default for new files: `crlf` on Windows, `lf` elsewhere), `case_insensitive_paths`, `folders`, `home`, `temp_dir`, `programs` (git, node, npm, python, pwsh, bash, rg on PATH, or null), `version` |
| `host__changes` | `cursor?` (none: start watching, answer the baseline), `wait_ms?` (≤ 30 000: long poll), `max?` | `feed` (2: a change after an answered cursor is never folded into an earlier one — cache only with a version you know), `cursor` (pass it next time), `changes` (`path`, `kind` `create`/`modify`/`rename`/`remove`/`metadata`/`any`), `overflow` (changes were missed: drop every cached read), `watching` (per folder `ok`/`error`), `error` |
| `host__spill_write` | `data`, `scope?`, `name?`, `path?` (append to a file it returned) | `path`, `bytes` — text kept in the environment's output folder (`<temp>/agentrouter-out/<scope>/`) |
| `host__out_status` | — | `folder`, `total_bytes`, `free_bytes`, `warn_bytes`, `min_free_bytes`, `low`, `scopes` (largest first: `scope`, `bytes`, `files`, `last_used_ms`, `path`) |
| `host__out_release` | `scope` | `released` — deletes that scope's files; only when the Agent decides so |

**The person's terminal environment (#95, Cloud #2383).** `host__exec`, jobs and PTYs start with the environment of
the person's login shell, not `hostd`'s own: `hostd` runs `$SHELL -i -l -c` once (as VS Code resolves the shell
environment: markers around `env -0`, stdin `/dev/null`, no terminal, a 10 s limit, `AGENTROUTER_RESOLVING_ENVIRONMENT=1`
for start-up files that want to skip work) and keeps it in memory; it captures again when a start-up file of that shell
changes. So `PATH` from `~/.zshrc` (nvm, mise, pyenv, Homebrew), exported variables and `SSH_AUTH_SOCK` are there as in
the person's terminal. `hostd`'s own proxy variables, `TMPDIR` and `AGENTROUTER_*` win (a sandbox's way out). In folder
mode the capture runs inside the same OS sandbox, so it reads only what the sandbox allows; a credential agent's socket
is reachable only if the sandbox allows it (owner decision pending). When the person's shell fails, `/bin/sh`'s login
environment is captured instead; when that fails too, the commands keep `hostd`'s environment; `AGENTROUTER_SHELL_ENV=off`
turns it off. The capture is the only place start-up files run: commands start `/bin/sh -c` (not a login shell) with the
captured environment, so nothing a start-up file prints or fails at reaches a command's output (Cloud #2246: Ubuntu's
`im-config` logs through `systemd-cat`, the sandbox's seccomp refuses the journal socket, and every command's stderr
began with `Failed to create stream fd: Operation not permitted` ahead of its own error). Windows: a process the person starts already has their registry variables;
nothing is captured. Values are never logged.

**The output folder (Cloud #2435, device #100, #102).** A job's stream past its window and an oversize tool result stay
here; only the tail and the path travel. The folder outlives the link (reconnects and reboots keep every path). Nothing in
it is deleted automatically — not on link end, not when the Agent moves away, not by age or size (owner 2026-10-21
「然后事件清理，这个应该是像CC一样，发现空间不够了，通知Agent可以清理了。我认为自动清理这个还是不要做，因为很可能误删」).
When the folder passes `AGENTROUTER_OUT_WARN` (default `1G`) or the disk's free space falls under
`AGENTROUTER_OUT_MIN_FREE` (default `2G`), the next `exec`/`job_start`/`job_read`/`spill_write` answer carries
`storage_low` (the `host__out_status` answer) once per crossing; the runner turns it into the environment's
`environment.storage_low` event and the Agent decides what to delete.

- The shell is real: `/bin/sh -c` with the person's login environment on Linux/macOS, PowerShell (`pwsh`, else Windows PowerShell) on Windows; `shell`
  names it. The command's whole process tree ends on timeout (process group / job object). Paths are real; relative ones resolve against the first folder.
- **Streamed output:** a `host__exec` call that carries `_meta.progressToken` gets its stdout/stderr as it arrives, as
  `notifications/progress` (`progress` counts up; `message` = the JSON text of `{"stream": "stdout"|"stderr", "data":
  "<utf-8>"}`, pushed every 100 ms or 32 KiB, characters never split, ≤ 16 MiB per stream). `notifications/cancelled`
  for a running call ends the command's whole process tree (answer `cancelled: true`); so does the timeout.
- **Background jobs and terminals live as long as the link.** When the `host` server's input closes (the link ends, the server is
  stopped: the aggregator closes its input and waits up to 3 s before killing its process group) or it gets a stop
  signal, it cancels every running call and ends every job's and terminal's process tree; on Windows a killed server takes its jobs
  along anyway (kill-on-close job objects).
- **Versions** (DSH's guarded edits): `version` is an opaque token of the file's stat identity (size, mtime ns; on Unix
  also inode and ctime), cheap enough for every `stat`; `expected.sha256` adds a content check. A guarded write checks,
  then writes; nothing is written on a mismatch (check and write are not one OS operation: another program writing in
  the microseconds between them is not caught).
- `host__write` without `offset` creates or replaces the file atomically (a temporary file next to it renamed over it,
  permissions kept; written in place where the rename is refused) (parents created); with `offset` it writes there,
  after truncating to `offset` when `truncate` is true (chunked transfer: first chunk `offset: 0, truncate: true`).
- A write the OS refuses answers `isError` with the OS text (the console shows 拦下：在文件夹外).
- Answers carry the bytes once: `structuredContent` has `data`; the text copy replaces any `data` over 64 KiB with a
  marker, so a 4 MiB base64 chunk fits the 8 MiB frame. `host__read` without `length` reads at most 4 MiB.
- Transfer (`remote_copy`, Cloud) is chunks of `host__read`/`host__write` (4 MiB, base64) relayed by the control
  plane from link to link (or the conversation's cloud sandbox), never through the model; each chunk is one audited
  call on each computer, the copy ends with `sha256` compared on both sides.

#### Bound session: the `host` server as the Agent's local machine (Cloud PROJECT-SERVICE §10.6)

A cloud session may be **bound** to a link (owner 2026-10-20 「实际上更改Agent的视图，就能让Agent有种在用户电脑运行的感觉了」):
the kernel's own file, search and command tools then act on this computer, through a runner plugin that is a client of
the `host` server (DSH first: `agentrouter-dsh-world` replaces DSH's `fs`, `subprocess` and `tool-fs-search` rows).
The device side is only this server; nothing on the device knows or cares whether a call comes from a bound session or
from `remote_*` tools.

| Kernel need (DSH) | `host` tool |
| --- | --- |
| `fs.stat` / `listDir` / `resolve`, prefetch of `AGENTS.md` and read-before-edit files | `host__stat` (many paths, small contents inline), `host__list` |
| `fs.readText` / `readBytes` / `streamText` | `host__read` (ranges; `version`, `sha256`, `line_ending`, `bom`) |
| `fs.writeText` / `editText` with `expected` (`replaceIfVersion`, `createIfAbsent`) | `host__write` with `expected` (atomic; `STALE_VERSION` / `EXISTS`) |
| `glob`, `grep` | `host__search` (`files` / `content`; ripgrep's crates, rg's display paths) |
| `bash` (streamed), timeouts, abort | `host__exec` with a progress token; `notifications/cancelled` |
| background jobs (`run_in_background`), their output and kill | `host__job_start` / `job_read` / `job_stop` / `jobs` |
| persistent terminal, interactive programs | `host__pty_open` / `pty_write` / `pty_read` / `pty_resize` / `pty_close` |
| "this host is Windows; commands run in PowerShell 7", path form, new-file line ending | `host__info` |
| per-turn read cache invalidation | `host__changes` (cursor + long poll; `overflow` = drop the cache) |

- **The device stays in charge.** Folder mode or 完全访问, the sandbox, the local `y`, argument-level audit (every call,
  including each long poll) and checkpoints before every tool without `readOnlyHint` (`write`, `remove`, `exec`, `job_start`,
  `job_stop`, `pty_*` except `pty_read`) apply exactly as for any other call; a bound session cannot reach anything the
  link does not already allow. `agentrouter undo` undoes a bound session's turn like any other.
- **One native host per session.** A conversation binds one host as the session's "local machine"; other links stay
  reachable through `remote_*`.
- **Latency.** A link call costs 30–200 ms: the plugin batches (`stat` with `content_max`, several paths per call),
  caches reads per turn by `version`/`sha256`, and drops them on its own writes and on `host__changes`.
- **Not on the device:** the kernel's own transcripts, spill files and session index stay on the Agent host (they are
  the kernel's state, not the person's files).

### 3.9 Lease epochs: a platform instance's job after a reconnect (Cloud #2677, #2246)

The platform starts an instance's process (a 作品's runtime) as a job with `lease: {instance, epoch}` (Cloud
PROJECT-SERVICE §4.8 "Lease epochs": the epoch moves on whenever control of the instance changes hands or it ends). The
job keeps running while the link is down, and the platform may end the instance meanwhile (its lease ran out, its holder
stopped it). After every `welcome` hostd sends the running leased jobs; the platform fences each against the instance
record (valid while the instance runs that very job on this host, whoever holds it now) and answers the others:

```text
device → gateway  {"op":"instances","instances":[{"instance":"ins_…","epoch":3,"job":"job-4"}]}
gateway → device  {"op":"lease_lost","jobs":[{"job":"job-4","instance":"ins_…","reason":"ended"}]}   (≤ 64)
```

hostd stops each named job through its own `host` server (`job_stop` with `instance`: a job id that names another job
by now — ids restart with hostd — is left alone with `LEASE_MISMATCH`) and records it in the audit log (`kind: lease`).

### 3.8 Preview streams: a dev server on the host in the web preview (Cloud #2387, #2246)

One preview mechanism for every host (agreed on Cloud #2246): Control's preview proxy (per-target origin, single-use
enter link, cookie; Cloud `live-preview.ts`) reaches a port through a transport per environment. For a host the
transport is a **stream through `hostd`**, the reverse-dial pattern of wstunnel/chisel reverse tunnels on the link's own
code (no second program, no second public endpoint):

```text
gateway → device  {"op":"port_open","id":"pst_<32 hex>","port":5173,"ticket":"<one-time>"}     (on the link)
device            connect 127.0.0.1:5173 (then [::1]:5173)
device → gateway  WebSocket wss://<gateway>/device/v2/port/<id>, Authorization: Bearer <ticket>; binary both ways
device → gateway  {"op":"port_error","id","code"}   PORT_CLOSED | PORT_DIAL_FAILED | PORT_BUSY | PORT_SANDBOXED
                                                    | PORT_NOT_IN_SANDBOX
```

- One WebSocket per TCP connection: MCP frames on the link never wait behind preview bytes; each stream has its own
  flow control; HTTP keep-alive and WebSocket upgrades (HMR) are plain bytes. At most 64 open per link.
- **Loopback only.** 完全访问 and cloud hosts serve any loopback port. In folder mode the commands' dev server listens
  inside the OS sandbox. **Linux** (device #136): the sandbox has its own network namespace, so the link's `host`
  server — in the same sandbox and namespace — bridges: hostd makes two FIFOs in the host server's scratch folder and
  calls its internal tool `_port_bridge {port, input, output}` (tools named `_…` are hostd's own and never listed to
  Agents); it connects to `127.0.0.1:<port>` (then `[::1]`) there, answers `PORT_CLOSED` when nothing listens, and moves
  the bytes over the FIFOs (no sockets inside the sandbox: seccomp refuses `AF_UNIX`, and the namespace has no route
  out). Only the sandbox's own ports exist there, so the person's own local services are never reachable this way.
  **Windows and macOS** (device #139) share the computer's loopback, so hostd dials the port itself, but only when
  every TCP listener on it (bound to loopback or to every address) is a process of the link's own servers: on Windows
  a pid in their Job objects (#129), on macOS a process whose parent chain, or whose session leader's, reaches a
  server's pid (servers start in sessions of their own, so a dev server whose shell exited still counts). The OS socket
  table comes from the `listeners` crate (MIT). A port the person's own program listens on answers
  `PORT_NOT_IN_SANDBOX`; when the owner cannot be told, `PORT_SANDBOXED`. This keeps a preview from opening the
  person's services by a guessed port; it is not a wall against a command that relays such a port itself, since
  loopback is open to the sandbox's commands there (§6.1).
- The gateway checks the ticket (one use, short-lived, bound to `id` and link) and splices the stream to Control's
  internal request; it never reads the bytes. Evaluated and not used: frp, chisel, wstunnel (separate programs with
  their own servers), yamux over the link (one TCP stream for everything; no maintained JS side).

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
  (`link__server_add` with a custom command), or any server that asks for the display (§6.7): the terminal
  shows the command line and asks `y/N`; no answer in 120 s = no. `--yes-servers` on the command line
  pre-answers it for unattended machines (a local flag, never an environment variable a child or the cloud can
  set). A yes is recorded as a **grant** for that exact definition (§6.7), so the same server is not asked
  about again; a changed definition is.
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
| Network | Allow-list per server from the catalog (`network: [domains]`) plus the package mirrors; custom servers get registries only unless the local `y` included network. **Open web** (owner Q7, 2026-10-20): a pinned catalog preset whose list is `["*"]` (the browser presets `browser`, `browser-firefox`) may reach any public host on any port; loopback/`localhost`, link-local (incl. `169.254.169.254`), multicast, cloud-metadata endpoints, private ranges (`10/8`, `172.16/12`, `192.168/16`, `100.64/10`, `fc00::/7`) and this computer's own addresses stay closed, by name and by IP literal. `*` from a custom server, the project's `.agentrouter/mcp.json` or any other local definition is dropped; no local flag or env var widens it. Mechanism: srt (0.0.81) has no `*` allow entry, so the device starts srt's own CLI through `open-web.mjs`, which passes srt's documented ask callback (`SandboxManager.initialize(config, askCallback)`): it allows public destinations and judges IP literals with srt's resolved-address guard; `network.deniedResolvedAddresses` adds the private ranges to that guard for names. Enforced through the sandbox's egress proxy |
| Local ports (device #139) | A command reaches a dev server it (or another command of the link) started on `localhost`, as in the person's own terminal. Linux: the sandbox's own network namespace and loopback (the person's local services are not there). macOS and Windows: srt's `network.allowLocalBinding` — loopback is shared, so the person's own local services (a database, a local admin page) are reachable to commands too, as on macOS with srt's setting on. On Windows this is a machine-wide mode of srt-win's WFP fence: `sandbox setup` installs it with `--allow-loopback` (an existing install is re-installed once with `--force`, one UAC prompt); every session asks for the installed mode (`srt-win status` → `wfp.allow_loopback`, or the setup's own record when that is unreadable without elevation), and `sandbox status` notes a machine still installed closed. Everything beyond this computer still goes through the allow-list. Preview streams open only the link's own listeners (§3.8). **Known difference from Linux, kept on purpose** (coordinator decision 2026-10-22, Cloud #2383): on macOS and Windows a sandboxed command can reach the person's own loopback services, exactly as the person's own terminal and a local CLI Agent can (PC parity target). What bounds it is who may run commands at all: on Cloud, every call on a person's computer needs input trust `delegated` or higher (the owner, their own Agents, team grants, their schedules; CONNECTIONS §4.5, Cloud #2556 / #2647), so a public or merely known sender can never make an Agent run a command here. A closed loopback would break the dev-server case and is not offered |
| The person's tool configuration (device #138) | The sandbox's home is not the person's (Windows: the sandbox account's own profile; macOS/Linux: `HOME` = scratch), so tools would not find their user configuration — git had no identity. The device points each tool at the person's own file, read-only, with srt-win's list of variables: `GIT_CONFIG_GLOBAL` (`~/.gitconfig`, else the XDG `git/config`), `NPM_CONFIG_USERCONFIG`, `PIP_CONFIG_FILE`, `MAVEN_ARGS`, `RUSTUP_HOME`, `CARGO_HOME`. Windows: what `srt-win profile status` offers (`env`); macOS/Linux: the same names found under the person's home. A file on the read deny list (e.g. `~/.npmrc`) is never named; a variable the server's environment (or, for commands, the person's terminal environment, #95) sets wins. `git config --global` inside the sandbox therefore fails (the file is read-only); repository config is unaffected |
| File watching (device #141) | Watchers inside the sandbox see changes, as in the person's terminal: the host server's change feed (`host__changes`), and the person's own tools (`fs.watch` on a folder, chokidar, a dev server's hot reload). Linux (inotify) and Windows (`ReadDirectoryChangesW`) need nothing. macOS: every watcher goes through the FSEvents service, which srt's Seatbelt profile does not list, so the device adds it (`network.allowMachLookup: ["com.apple.FSEvents"]`); without it a watch starts without error and never reports anything. FSEvents reports changed paths, never contents |
| Terminals (Cloud #2383) | Commands and the host server's persistent terminals open pseudo-terminals, as in the person's terminal. macOS: srt's `allowPty` (`(allow pseudo-tty)`, `/dev/ptmx`, `/dev/ttys*`); without it `openpty` fails with "Operation not permitted". srt's documented setting for a command that allocates a pty of its own. Linux and Windows need nothing |
| Processes | Whole tree in the sandbox; killed with the link (job object / process group) |

### 6.2 Tool choice

| Candidate | Licence | Activity (checked 2026-10-19) | Verdict |
| --- | --- | --- | --- |
| **Anthropic `sandbox-runtime` (`srt`)** | Apache-2.0 | Active: pinned v0.0.81 (2026-10-19; re-vetted 2026-10-21: Apache-2.0, same four dependencies, Windows still alpha) | **Chosen.** Built to sandbox "agents, local MCP servers, bash commands and arbitrary processes"; one policy format on all three OSes: macOS `sandbox-exec` (Seatbelt), Linux bubblewrap + network namespace, Windows a dedicated `srt-sandbox` local account + per-session explicit ACEs on the working tree + WFP egress fence. Used as a process wrapper (`srt --settings <file> -- <cmd>`), so no linking and a narrow contract. Risks: pre-1.0 (pin exact version + integrity like a catalog package, re-vet on each bump); needs Node, which the device provisions itself (§6.5); Windows needs a one-time elevated setup (Q4) |
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
  account and WFP filters). Setup then makes two read grants without elevation (device #66):
  - srt's **profile read grant** (`srt-win profile grant`, srt ≥ 0.0.81). The profile becomes readable to the
    account except srt's built-in credential stores (keys, browser and password-manager data, registry
    hives) and the device data folder (`--exclude`). This is the §6.1 read rule, as on macOS and Linux;
    the per-session deny list still applies. Without it the account cannot read anything under the
    profile, so Node cannot even resolve a script path there (`EPERM lstat …\AppData`).
  - A read+execute ACE for the account on the installed runtime folder, because srt-win starts its
    runner as the account from `srt-win.exe` before any session grant (otherwise "Access is denied").

  What sandboxes read and run lives **beside** the data folder, in `<data>-shared` (srt in `sandbox\`,
  the pinned MCP packages in `mcp-packages\`), like the scratch folders in `<data>-scratch`. srt
  refuses a session that grants a read inside an excluded path, and the data folder stays secret
  as a whole.

  Every start checks both. A later `sandbox setup` refreshes the profile grant, so items created
  since the last run become readable. `agentrouter sandbox remove` (`srt-win uninstall`) removes the
  grant. FAT/exFAT folders and network shares cannot carry ACEs: `SANDBOX_UNAVAILABLE` for them.

  **Stdio (device #112).** srt-win gives a sandboxed child an empty stdin by design (upstream R14: the
  broker uses the runner's stdin for its own launch spec and pumps back only stdout and stderr). A stdio
  MCP server, the host server included, would read end-of-file and exit before `initialize`. So for
  every confined server the device creates a **named pipe** first (`\\.\pipe\agentrouter-<128 random
  bits>`): one instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`), no remote clients, a protected DACL whose
  only entry lets the sandbox account read and write it. The name goes in the launch file. The sandbox
  entry connects to it as the client (the sandbox side holds only client ends, as with srt's own pipes)
  and relays it to the server's stdin and from its stdout. stderr stays on srt's pumped stderr, and the
  aggregator speaks MCP over the pipe. A second client cannot connect. Limit: all confined servers run
  as the one sandbox account, so they are not isolated from **each other** on Windows (one could read
  another's launch file in `<data>-scratch` before the entry deletes it, or open its processes). The
  boundary is between the servers and the person's files, secrets and network, as on the other OSes.
- **macOS:** Seatbelt profile generated per link; no setup step.
- **Linux:** bubblewrap + a network namespace. srt also runs ripgrep (its scan for dangerous files below a folder)
  and socat (the network relay: host side a Unix socket to srt's proxy, inside bubblewrap TCP 3128/1080 to that
  socket). All three are the device's own pinned copies (§6.5, device #119, #122), and srt gets their absolute paths
  (`ripgrep.command`, `socatPath`, `bwrapPath`). The person installs nothing.

  **Ubuntu's user-namespace restriction (#122).** Ubuntu 23.10+ sets `kernel.apparmor_restrict_unprivileged_userns=1`.
  An unconfined process's new user namespace then has no capabilities. The packaged `/usr/bin/bwrap` is exempt
  through Ubuntu's `bwrap-userns-restrict` profile, but that profile stacks every child with `unpriv_bwrap` (`audit
  deny capability`). srt's seccomp helper (`apply-seccomp`) runs inside bubblewrap and makes a nested namespace, so it
  fails with `setgroups: Permission denied`. The coordinator decided on least privilege: no global sysctl, and the
  seccomp layer stays on. The device therefore lifts the restriction for one program only:
  - `sudo agentrouter sandbox userns install` puts a **root-owned** copy of the pinned bubblewrap at
    `/usr/local/libexec/agentrouter/bwrap-<version>/bwrap`. It is downloaded and checked like `sandbox setup` does.
  - It writes `/etc/apparmor.d/agentrouter-bwrap` in the pattern of Ubuntu's own per-application profiles:
    `abi <abi/4.0>`, `flags=(unconfined)`, `userns,`, attached to that exact path, nothing else. Then it loads the
    profile with `apparmor_parser --replace`. The apparmor service loads it again at boot.
  - It is idempotent. `sudo agentrouter sandbox userns remove` unloads and deletes both.
  - Everything bubblewrap starts inherits the profile: srt's relay, `apply-seccomp`, and the command. The command runs
    under the helper's filter, which refuses new user namespaces, so the grant ends at the helper.
  - The grant never names a path the person's processes can write. A profile on a home-folder path would hand
    capability-bearing namespaces to anything that drops a file there, which is what the restriction prevents.
  - With the restriction on, srt runs the root-owned copy. `HOSTD_SANDBOX_NEEDS_ADMIN` (Cloud error catalog) names the
    install line with the running executable's absolute path while that copy or its profile is missing, or while a
    confined start still fails for user namespaces (profile not loaded).
  - With the restriction off (Debian, Fedora, Arch…), srt runs the device's own copy, and nothing needs an admin.
  - A kernel switch that refuses user namespaces (`kernel.unprivileged_userns_clone=0`, `user.max_user_namespaces=0`)
    gets its own one persistent `sysctl.d` line under the same code.
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
sandbox is unavailable (e.g. Ubuntu before `sandbox userns install`).

### 6.5 The device's own runtimes (device #116)

Folder mode never asks the person to install anything (the product is for ordinary people). srt and `npx` servers run
on Node.js, `uvx` servers (serena, §13.2) on uv, so the device provisions both in `sandbox setup` (which the double-click
start and the first `link` run), and `--full` provisions them too:

- One pinned version each (Node.js 22 LTS, uv; Node 24 refuses the Unix socket path srt uses under the macOS data folder, longer than 104 bytes), the project's official portable archive per OS and architecture, the
  SHA-256 in source (`crates/sandbox/src/runtimes.rs`; Node's from the release's signed `SHASUMS256.txt`). Every
  runtime has an official source and one that is fast from mainland China (device #154; there GitHub, nodejs.org,
  releases.astral.sh and registry.npmjs.org gave 8–20 KB/s, the first link took 21 min): Node from npmmirror, uv as
  Astral's own PyPI wheels of the same release from the Tsinghua and Huawei PyPI mirrors (the wheels' SHA-256 pinned
  too), ripgrep from npmmirror, socat and bubblewrap from our npm package `@agentrouter-top/sandbox-tools-linux` (the
  same archives, `.github/workflows/sandbox-tools.yml`). Order: the official host first, left after 8 s below
  256 KiB/s or when it does not answer within seconds; then the mirrors; the last source is never left for being slow.
  Once an official host failed or was left, mirrors go first on that computer (`<runtimes>/.prefer-mirrors`), and
  srt's `npm ci` uses npmmirror first too (npm's own configuration otherwise, npmmirror as the fallback). A file that
  does not match is deleted, never unpacked.
- Downloaded in-process (no curl, PowerShell or installer) into `<data>-shared/runtimes/<name>-<version>/`, never
  through the temp folder; the old version's folder is removed after the new one is in place (docs/AV-HYGIENE.md, the
  one exception to "gateway only"). Windows: the sandbox account gets read+execute on that folder, like on srt's.
- One path: srt, the Windows sandbox entry, `npm ci` (as `node npm-cli.js`, no `npm.cmd`), `npx` and `uvx` use these,
  never the person's own installs; they go first on every local server's PATH. Without uv only `uvx` tools are
  missing (a line in setup, `RUNTIME_MISSING` on start). A cloud placement provisions nothing (its image has them).
- Linux also gets srt's **ripgrep** (the VS Code build `@vscode/ripgrep-linux-<arch>` 1.18.0 = ripgrep 15.0.0, static
  musl, from the npm registry and npmmirror; #154), **socat** (1.8.1.3) and **bubblewrap** (0.13.1). socat and
  bubblewrap are built statically from the official source by `.github/workflows/socat.yml` and `bwrap.yml` and
  published with that source, because upstream has no maintained static build. They come from GitHub or our npm
  package, in `sandbox setup` only (`--full` does not need them). srt gets their paths; they are not put on a server's PATH.
  Where Ubuntu restricts user namespaces, the bubblewrap that runs is the root-owned copy of the same pin (§6.3).

- **Catalog servers install from their locks** (Cloud #2740, `crates/sandbox/src/locks.rs`). `link__server_add` of a
  pinned catalog preset carries `lock`: every package below the pinned one with its registry checksum (npm
  `package-lock.json`; Python `requirements.txt` with `--hash` lines and `.python-version`), the sources in order, the
  program and the server's own arguments. The device checks that the lock names exactly the pinned package and digest,
  installs it once into `<data>-shared/mcp-packages/locks/<key>/` outside the sandbox, with no package code run —
  `npm ci --omit=dev --ignore-scripts` on the device's Node; `uv venv` on a uv-managed Python
  (`<data>-shared/mcp-packages/python`) and `uv pip sync --require-hashes`, wheels only except the entry's `build`
  names — and then runs the installed program (`node <bin script>`, `.venv/{bin,Scripts}/<bin>`) confined, instead of
  `npx`/`uvx` resolving the tree at every start. Sources: the official registry first, then the mirrors, each one whole
  attempt with that source in the environment (an attempt that is not the last gives up after 180 s); where an official
  host already proved slow or unreachable (`.prefer-mirrors`, above) the mirrors go first. The managed Python comes from
  uv's own host first, then npmmirror's copy. A file whose checksum is not the lock's is `MCP_DIGEST_MISMATCH` (nothing
  installed); every source failing is `MCP_INSTALL_FAILED` with each source and its answer. The folder counts only
  with its marker (written last); installs run one at a time. The server's state then is `install_failed` and the
  error stays until the next `link__server_add`. A definition without `lock` (an older cloud) runs as before.

### 6.6 Resource limits per environment (Cloud VISION §4 D10, device #127)

An environment may carry limits on **memory**, **processes** and **CPU** for everything a local server runs. The `host`
server is where the Agent's commands, jobs and terminals run, so its tree is the environment's work. `hostd` enforces
them from **outside** the sandbox, around each server it starts (`agentrouter_device::limits`). Inside, seccomp refuses
`AF_UNIX`, so there is no systemd bus there, and the Agent's own processes could reach whatever enforces them.

| OS | How | Memory | Processes | CPU |
| --- | --- | --- | --- | --- |
| Linux with a user systemd and cgroup v2 (desktops) | a transient scope per server: `systemd-run --user --scope`, which executes the server in place (same pid, process group, stdio); `OOMPolicy=continue` | `MemoryMax`, `MemorySwapMax=0`: the kernel's OOM killer ends the largest process in the scope; the server and the rest go on | `TasksMax` = processes × 32 (the kernel counts threads) | `CPUQuota` (when the user manager has the cpu controller) |
| Windows 完全访问 | a kill-on-close Job object per server (the tree is hostd's own child) | job committed memory: an allocation beyond it fails | active processes: a start beyond it fails | hard CPU-rate cap |
| Windows folder mode | a kill-on-close Job object per server, made **before** the start (device #129): srt-win starts the confined server as the sandbox account through Secondary Logon, in a job of its own that hostd cannot reach, and hostd cannot open that account's processes. So hostd makes a named job (`Local\agentrouter-limits-<128 random bits>`) with the limits already set and a protected DACL that lets only the sandbox account assign processes; the launch file names it, and the sandbox entry (`entry.cjs`) runs `agentrouter sandbox job-join <name> <its pid>` (this program, which the sandbox account already runs as the link's `host` server; only its exact answer `joined` counts) before it starts the server. The job nests under srt's own. When joining fails the server does not start. Only the entry (a small Node.js process) runs outside it for a moment, before any of the server's code; its memory counts toward the limit afterwards | job committed memory: an allocation beyond it fails | active processes (the entry counts as one): a start beyond it fails | hard CPU-rate cap |
| macOS; Linux without a user systemd | sampling every 2 s (sysinfo) | over it, the largest process ends | over it, the newest processes end | not capped |

- **Values.** The platform's (`link__limits {memoryMb, processes, cpuPercent}`, from the environment record), capped
  field by field by the owner's ceiling (`config.json` `limits: {memory_mb, processes, cpu_percent}`). Neither set: no
  limit, as in the person's own terminal. Out of range → `LIMITS_INVALID` naming the field and the range, never clipped.
- **Live.** `link__limits` applies at once to every running server (`systemctl --user set-property --runtime`, the Job
  object's information, the sampler's values): no restart, background jobs keep running. A start whose limits cannot be
  enforced fails with the reason instead of running unlimited.
- **Said.** A limit reached is a line in the server's log, the audit (`server` `limit`), the console feed and
  `link__servers` (`limitEvents`). A call that was running when it happened gets the line in its answer (or in its
  `SERVER_CRASHED` message). `link__servers` and `hello.link` `capacity.limits` show the limits, `enforcedBy`
  (`systemd-scope`, `job-object`, `sampled`), what this OS does not enforce (`unenforced`) and why sampling.
- **Disk** is not limited per folder (a PC has no per-folder quota without an administrator); the output folder's
  `storage_low` event covers it. Wall time is each call's own timeout.

### 6.7 Grants and the display (device #97, Cloud #2396; coordinator decision 2026-10-22)

Owner 2026-10-21: 「就对标你的能力吧。就是普通的电脑操控，可以让用户自己装MCP解决」 — GUI control is the person's own
choice: they add a GUI MCP (e.g. Windows-MCP) on their computer and cloud Agents use it through `hostd`. The platform
builds no screen feature (#51 stays: no screen preset in the catalog).

- **A grant is what the person allowed one tool**, recorded on this computer only (`<data>/grants.json`, read by no
  sandbox): keyed by the tool's name and the hash of its definition (command, arguments, environment, network hosts,
  display, pinned package); environment values are hashed, never stored. The record carries what was granted (the
  display, the network hosts), so a yes covers exactly what the person was shown; a changed definition asks again.
  The cloud can ask for a grant (`link__server_add` with `display: true`), never make one.
- **Display is a per-tool grant, never a folder-mode setting.** Folder mode has no display (Linux: no X11/Wayland/
  D-Bus sockets, seccomp refuses `AF_UNIX`; Windows: the sandbox account; macOS: Seatbelt), and the Agent's own
  commands keep none. A tool granted the display **runs with the person's own rights, outside the folder sandbox**:
  any display access drives every other window (X11 and the D-Bus session bus on Linux — D-Bus can even start units
  outside a sandbox; `SendInput`/UI Automation on the same desktop on Windows; Screen Recording/Accessibility on
  macOS), so a sandbox around such a tool would be nominal. It still runs under the environment's limits (§6.6), the
  argument audit (§7) and the link's lifetime.
- **Asked in plain words** (owner draft 2026-10-22): 「<名称> 要能看到并操作你的屏幕，所以它以你自己的权限运行，不受文件夹
  限制。允许？[y/N]」. A catalog preset that asks for the display is asked too.
- **The person's own tools: `agentrouter mcp add <name> [--display [--yes]] [--title t] [--network a,b] -- <command…>`**
  (#2396 gap 1). Adding a tool is the person's yes for it (a display grant is asked once more in the words above;
  `--yes`, typed on this computer like `--yes-servers`, answers it in advance for an unattended machine); the
  definition is kept in `<data>/tools.json` and started by every `agentrouter link` on this computer, like the
  project's `.agentrouter/mcp.json`, until `agentrouter mcp remove <name>` (which also forgets the grant). `agentrouter
  mcp list` shows them. Environment values are not taken on the command line (workspace rule: no data through command
  lines).
- **Shown:** `link__servers` answers `display: true` for a granted tool (「以你的权限运行」); the audit's server start
  records the policy `{display: true, unconfined: true}`.
- **Parity bench T4** (a screenshot) runs through such a tool: the bench, as the person, adds a screenshot MCP with
  the grant, and T4 asks for the screenshot through it.

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
before a non-read-only call (`host__write`'s and `host__remove`'s `path`; other servers' `path`/`file_path`/`destination`/`target`
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
  沙箱     Windows 隔离账户（srt@0.0.81）
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
| 4 | 2 Link | device · `hostd-link` (formerly `linker`; new crate from `net.rs`) | `/device/v2/link`, `hello v2` with `lnk_`, envelope `{s, m}`, reconnect/heartbeat, `audit_head`; no capabilities |
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
| Identity | device key paired by the person (§4) | device key paired by us (ops), leased to one person's session tree at a time | **no pairing, no device key:** the control plane issues a host token bound to (sandbox id, session tree) at create and revokes it at destroy; it is the `hello` token (no challenge/token exchange). The control plane's signing key is passed in at start (§13.1.1) |
| Boundary | folder mode: `srt` OS sandbox (§6); 完全访问: the person's own rights (§6.4) | 完全访问 inside the lease account (the account and the machine are the boundary); wiped between leases | **the sandbox is the boundary:** servers run without `srt`, like 完全访问 inside the container |
| Folders | named by the person; more only with a local `y` (§5) | the lease's working folders | the session's mounted views (workspace manifest, Cloud PROJECT-SERVICE); `link__folder_add` answers `DENIED_BY_USER` |
| Who decides server installs | the person: catalog presets run, custom ones need `y` (§5) | platform policy (as cloud) | platform policy: catalog presets named by the session's definition or the person's list; the person's own custom entries only in that person's sandboxes (§13.3) |
| Network | catalog allow-list per server (§6.1) | catalog allow-list per server | the sandbox's egress policy + the catalog allow-list per server at the node's egress proxy |
| Undo | checkpoints + journal (§7.2) | checkpoints | off: the project service's per-turn changesets are the undo |
| Audit | §7.1 + `audit_head` | same | same (the chain head is the only copy outside the sandbox that matters) |

- `hello.link` carries `placement: "pc" | "cloud"` (`machine` later; Cloud shows hosts by placement and never
  offers a `cloud` host to anyone outside its session tree).
- **What Daytona keeps:** lifecycle (create / start / stop / snapshot / destroy), starting `hostd` at boot, and the
  port proxy for previews until the gateway gives each sandbox its own addresses. Agent execution and files stop using
  Daytona's toolbox (`/toolbox/process/execute`, `/toolbox/files/*` in Cloud `daytona-provider.ts`); they go to `hostd`
  like on a PC. Cloud PROJECT-SERVICE §10.8 is the Cloud side of this split.
- A cloud host dials out to the same gateway endpoint as a PC (`/device/v2/link`), so the relay, the MCP client and
  the DSH plugin have one path; the sandbox needs no inbound port either.

#### 13.1.1 Running `hostd` in a cloud sandbox (issue #82)

The sandbox entrypoint runs the static Linux build (below) once per sandbox, in the background, for the sandbox's life:

```sh
AGENTROUTER_HOST_TOKEN_FILE=/run/agentrouter/host-token \
AGENTROUTER_CONTROL_KEY=<control plane Ed25519 public key, base64url (or {"kid","publicKey"} JSON)> \
AGENTROUTER_GATEWAY=https://agent-gateway-dev.agentrouter.top \
AGENTROUTER_HOST_ID=<sandbox id> \
AGENTROUTER_DEVICE_HOME=/var/lib/agentrouter \
  hostd link --placement cloud /workspace/<view> [/workspace/<view2> …]
```

| Input | Meaning |
| --- | --- |
| `AGENTROUTER_HOST_TOKEN_FILE` (or `--token-file`) | The platform-issued host token, re-read before every (re)connect, so the platform may rotate it in place. |
| `AGENTROUTER_HOST_TOKEN` | Alternative to the file; removed from `hostd`'s environment at start, so no server or command the Agent runs inherits it. |
| `AGENTROUTER_CONTROL_KEY` | The control plane's signing key (on a PC it is pinned at pairing); every request is verified against it (§3.3). Required. |
| `AGENTROUTER_GATEWAY` (or `--gateway`) | The gateway; default the Dev gateway. Same `/device/v2/link` endpoint as a PC. |
| `HTTPS_PROXY` / `NO_PROXY` | The link's WebSocket goes through an `http://` proxy with CONNECT when one is set (a cloud sandbox's only egress is its node's proxy; PCs behind a proxy too), curl's conventions: `HTTPS_PROXY`/`https_proxy` for `wss://`, `HTTP_PROXY` for `ws://`, `NO_PROXY` wins, credentials in the URL = Basic auth (issue #85, `crates/hostd-link/src/proxy.rs`). |
| `AGENTROUTER_HOST_ID` | The sandbox id; the link id is `link_id("cloud:<id>", first folder)` (stable across `hostd` restarts in the same sandbox). Default: the host name. |
| folders (arguments) | The session's mounted views, absolute; the first is home. Fixed for the session. |

Behaviour: `hello.link` = `{placement: "cloud", app: "hostd", full: true, sandbox: ""}`; the `host` server starts
unconfined at once; no checkpoints or journal; no prompt ever (custom servers and `link__folder_add` answer
`DENIED_BY_USER`); audit and `audit_head` as on a PC (data under `AGENTROUTER_DEVICE_HOME`); the log is one line per
event on stdout; SIGTERM/SIGINT stop it (exit 0), a revoked token or a refused link exits 1, missing inputs exit 2.

**The build:** `.github/workflows/hostd.yml` builds the `agentrouter` binary for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` (static, rustls with bundled web PKI roots — no OpenSSL, no CA bundle needed; the PC build
keeps native-tls). Every merge to `main` that changes the program publishes a GitHub pre-release
`hostd-v<crate version>-<sha7>` with `hostd-x86_64-linux-musl`, `hostd-aarch64-linux-musl` and `SHA256SUMS`:
`https://github.com/Maybank01/agentrouter-device/releases/download/<tag>/hostd-<arch>-linux-musl`. The Cloud sandbox
image pins one tag and checks the sha256. Locally: `cargo build --release -p agentrouter-device-cli --bin agentrouter
--no-default-features --features rustls --target x86_64-unknown-linux-musl` (needs `musl-gcc` for `ring`).

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
| device | ~~`placement: cloud`~~ done (#82, §13.1.1): `hostd` started from a sandbox entrypoint, platform-issued token, no `srt`, no checkpoints, no `y`, `link__folder_add` refused; static musl build per merge |
| device | Lazy start of bundle/catalog servers from recorded tool names (`serena`) |
| device | Credentials in a signed server start: env for that process only, redacted everywhere |
| cloud | Catalog as data + `mcp_catalog` tool; definitions' `mcp` field compiled into installs (AGENT-DEFINITION §3.7) |
| cloud | Execution-host interface split: lifecycle (Daytona) vs host protocol (PROJECT-SERVICE §10.8) |
| cloud | Vault entries for MCP credentials, ask-first in the MCP client, `ASK_UNAVAILABLE` (#2201) |

### 13.6 Names

The program is **`hostd`** (Host Daemon, 宿主守护进程; owner 2026-10-20, §0.3); "link" names only the connection (`lnk_`
ids, `/device/v2/link`). People keep running `agentrouter link`, which starts `hostd`; `agentrouter hostd` is the same command.
Renamed in #80: the link crate is `crates/hostd-link` (`agentrouter-hostd-link`), this file was `docs/LINKER-MCP.md`
(a stub stays there). Unchanged on purpose: everything on the wire or on disk (protocol strings, JSON fields, `link-sig-v3`,
`AGENTROUTER_*` env vars, config/state paths, audit format, `app: "hostd"`), the npm package `@agentrouter-top/cli`
and release artifact names.

### 13.7 Generic MCP integration: what `hostd` adds (Cloud MCP-INTEGRATION, 2026-10-20)

Cloud `docs/development/MCP-INTEGRATION.md` (#2201, docs PR Maybank01/agentrouter-cloud#2238) owns the catalog, actions
and views. The `hostd` side it needs, beyond §13.1–§13.5:

- **Account home.** A credentialed catalog entry (`placement: account_home`, e.g. `bilibili`) runs on exactly one `hostd`
  per (person, credential), whatever environment a session binds; one process per credential (`per_credential: 1`), so
  one IP, one cookie refresher, one poller. Sessions on other hosts (or with no environment) call it through the gateway.
- **Credential tools are host-only.** Tools the catalog lists as `credential_tools` (`bilibili_login`, `…_login_check`,
  `…_refresh_cookie`) are never in `tools/list` for the cloud; Cloud's login card asks the account home to run them and
  shows the QR to the person. A credential is delivered in the signed server start as env or a file in the server's own
  data directory (outside every linked folder), never as an argument (§13.5).
- **Write-back (new frame):** when a server rewrites its credential file (B站 cookie rotation), `hostd` sends a signed
  `credential_update {server, name, sealed value}`; Cloud accepts it only from that account home for that server's
  declared names. Local-only credentials (§13.3) are never sent.
- **Result redaction (new):** `hostd` replaces any value it delivered to a server (exact and URL-encoded) with `[secret]`
  in that server's results before they leave the host.
- **Feed poller (new frames):** for entries that declare feeds, `hostd` calls the read tool on the catalog's interval
  (floor per entry, jitter, idle backoff, unread-count gate), extracts items by the declared paths and sends
  `view_items {feed, items, cursor}`; it advances its cursor only after Cloud's ack (Cloud dedupes by item id). Polls run
  only while Cloud says the feed has subscribers; no model calls.
- **Rate-limit backstop (new):** per-server, per-tool limits arrive in the signed start; over them a call fails with
  `RATE_LIMITED` (added to §3.5) even if Cloud let it through.
- **Lifecycle:** lazy start from recorded tool names, idle stop (default 10 min, kept up while a feed is active), crash
  backoff, per-host cap on running third-party servers (PC 8, cloud 4).

**As built (Cloud #2694): credentials that stay on this computer, and the login on this computer.** Generic for any
server that reads its account from a file; nothing here names a service.
- **Declared in `link__server_add`:** `credentials: [{name, fileEnv}]` (the server reads the credential from the file
  whose path is in `fileEnv`), `hostOnly: [tool…]` (tools only `hostd` runs) and `login: {credential, start, url, key?,
  check?, keyArg?}` (the server's own login tools; `url` / `key` are patterns over the start tool's text, `key`'s first
  group is the key passed to `check` as `{<keyArg>: key}`). The login tools join `hostOnly` by themselves.
- **Kept here only** (`device-core::local_secrets`, Cloud CONNECTIONS §3.2 `local:`): `<data dir>/secrets/`, one value
  per `<server>/<credential>`, sealed with DPAPI on Windows, an owner-only file elsewhere (the device identity's
  protection); the data folder is hidden from every sandbox. The cloud sees only `credentials: [{name, state: ok |
  missing | waiting_login, lastError}]` in `link__servers` — never a value.
- **Handed to that one process as a file:** before each start `hostd` writes the kept value into the server's own
  scratch folder (`credentials/<name>`, owner-only; writable by that server only) and puts the file's **path** in
  `fileEnv` — never the value in an argument or the environment. After every call, every login and every exit, a file
  the server wrote or rewrote (a login, a cookie refresh) is kept again (`credential_saved` on the console).
- **Blanked:** every kept value and each JSON / cookie value of 8+ characters inside it reads `[secret]` in results,
  logs and the audit, like `secretEnv`.
- **Host-only tools:** never in the cloud's `tools/list`; a call is `HOST_ONLY_TOOL`.
- **The login shows by itself:** when a server that declares `login` is ready and this computer keeps no value for
  its credential, `hostd` starts the login at once — installing it is enough for the person to see the QR here.
- **`link__login {name}`** (starts it again): runs `start`, shows the address as a QR on this console (`Feed::Login`; a cloud host logs
  only that a login started), answers `{state: waiting_login, url}` for the web page, then waits up to 180 s for
  `check` in the background → `login_ok` or `login_failed` (its words in `lastError`). Errors: `LOGIN_UNSUPPORTED`,
  `SERVER_NOT_RUNNING`, `LOGIN_IN_PROGRESS`, `LOGIN_FAILED`.
- **`link__logout {name, credential?}`:** forgets the value here and restarts the server without it.

Follow-ups D1–D6 are listed in that doc §8.3 (D1, D2, D6 extend §13.5; D3 write-back + redaction, D4 feed poller, D5
rate backstop are new).
