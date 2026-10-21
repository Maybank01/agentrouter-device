// AgentRouter device: srt's own CLI, with an open-web network decision (docs/HOSTD-MCP.md §6.1; owner 2026-10-20
// 「可以开放到完整的互联网」, browser presets only). Written next to the pinned srt and started instead of its cli.js
// with the same arguments.
//
// srt 0.0.80 has no "any host" entry for allowedDomains; a host no list decides goes to the ask callback, the second
// argument of SandboxManager.initialize (README "Asking about a host no rule decides"), which the CLI leaves empty.
// This file hands the CLI's initialize a callback that allows public destinations only:
// - a hostname is allowed unless it is a loopback name or this computer resolves it to a refused address; srt's
//   resolved-address guard then judges what it really dials (loopback, link-local, multicast, cloud metadata, this
//   host's own addresses, plus the private ranges the device lists in network.deniedResolvedAddresses);
// - an IP literal (also in a shorthand such as 2852039166) is judged here by the same guard, since srt never re-judges
//   a literal it is told to allow.
// If this hook ever fails to take, the CLI runs with no callback and every host off the allow list is refused.
import { lookup } from 'node:dns/promises';
import { isIP } from 'node:net';

const pkg = './node_modules/@anthropic-ai/sandbox-runtime/dist/';
const { SandboxManager } = await import(pkg + 'index.js');
const { createResolvedAddressGuard } = await import(pkg + 'sandbox/resolved-address-guard.js');
const { canonicalizeHost, stripBrackets } = await import(pkg + 'sandbox/parent-proxy.js');

const withReason = SandboxManager.askCallbackDenyReason === true;
const refuse = reason => (withReason ? { allow: false, reason } : false);
// A neutral name: the guard grants loopback to loopback names, which the open web never asks for.
const ANY = 'open-web.invalid';

function decider(config) {
  const guard = createResolvedAddressGuard(config?.network ?? {});
  return async ({ host, port }) => {
    const bare = stripBrackets(String(host)).toLowerCase().replace(/\.$/, '');
    const h = canonicalizeHost(bare) ?? bare;
    if (isIP(h)) {
      return guard.permits(ANY, h, port) ? true : refuse('the open web excludes local and private addresses');
    }
    if (h === 'localhost' || h.endsWith('.localhost')) return refuse('the open web excludes this computer');
    // Through a parent proxy (HTTP_PROXY in the device's environment) srt does not resolve the name itself, so a
    // name this computer resolves into a refused range is refused here too.
    try {
      const all = await lookup(h, { all: true });
      if (all.some(a => !guard.permits(ANY, a.address, port))) {
        return refuse('the open web excludes local and private addresses');
      }
    } catch {
      // Not resolvable here: srt (or the parent proxy) resolves it when it dials.
    }
    return true;
  };
}

const initialize = SandboxManager.initialize;
SandboxManager.initialize = (config, _ask, ...rest) => initialize.call(SandboxManager, config, decider(config), ...rest);
await import(pkg + 'cli.js');
