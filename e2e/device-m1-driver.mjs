// Device M1 end-to-end: the conversation side, run on the ops host under the shared-session lock:
//   E2E_SHA=<commit of the e2e run> /root/agentrouter-ops/session-lock.sh dev node device-m1-driver.mjs
// Takes the newest pairing code the Windows runner published as the commit status
// `device-e2e/pairing-code` (the runner issues a fresh one every 10 minutes, so waiting for the
// lock does not matter), confirms it, waits for the device, starts a Codex
// conversation, grants it the device, asks it to fix the sample repo and run its tests, waits for the
// turn, prints the outcome, and removes the device (which ends the runner's job). Never prints cookies
// or tokens; writes the rotated session back after every refresh (e2e/README.md).
import { readFileSync, renameSync, writeFileSync } from "node:fs";

const ORIGIN = "https://edge-d53hmn9blslvhcuff8rjsdvt.agentrouter.top";
const HOST = new URL(ORIGIN).hostname;
const SESSION_FILE = process.env.SESSION_FILE;
const SHA = (process.env.E2E_SHA || "").trim();
const REPO = process.env.E2E_REPO || "Maybank01/agentrouter-device";
const MODEL = process.env.MODEL || "gpt-5.5";
const KERNEL = process.env.KERNEL || "codex";
const NAME = process.env.DEVICE_NAME || "CI-e2e";
const KEEP_DEVICE = process.env.KEEP_DEVICE === "1";
if (!SESSION_FILE || process.env.SESSION_LOCKED !== SESSION_FILE) throw new Error("run under session-lock.sh dev");
if (!/^[0-9a-f]{40}$/u.test(SHA)) throw new Error("E2E_SHA missing");

const jar = new Map();
let origins = [];
let access = null;
let authSession = null;
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const clip = (s, n = 300) => String(s ?? "").replace(/\s+/gu, " ").slice(0, n);

function load() {
  const saved = JSON.parse(readFileSync(SESSION_FILE, "utf8"));
  origins = Array.isArray(saved.origins) ? saved.origins : [];
  for (const c of saved.cookies ?? []) {
    if (String(c.domain ?? "").replace(/^\./u, "") !== HOST) continue;
    const path = typeof c.path === "string" && c.path.startsWith("/") ? c.path : "/";
    jar.set(`${c.name}\u0000${path}`, { ...c, path });
  }
  if (!jar.size) throw new Error("no saved Dev session");
}
function save() {
  const cookies = [...jar.values()].map((c) => ({ name: c.name, value: c.value, domain: HOST, path: c.path, expires: c.expires ?? -1,
    httpOnly: c.httpOnly ?? false, secure: true, sameSite: c.sameSite ?? "Lax" }));
  const tmp = `${SESSION_FILE}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify({ cookies, origins }), { mode: 0o600 });
  renameSync(tmp, SESSION_FILE);
}
function store(response) {
  let changed = false;
  for (const raw of response.headers.getSetCookie?.() ?? []) {
    const [pair, ...attributes] = raw.split(";").map((p) => p.trim());
    const eq = pair.indexOf("=");
    if (eq < 1) continue;
    const name = pair.slice(0, eq);
    const value = pair.slice(eq + 1);
    let path = "/"; let expired = value === ""; let expires = -1; let httpOnly = false; let sameSite = "Lax";
    for (const attribute of attributes) {
      const [key, ...rest] = attribute.split("=");
      const k = key.toLowerCase(); const v = rest.join("=");
      if (k === "path" && v.startsWith("/")) path = v;
      if (k === "max-age") { if (Number(v) <= 0) expired = true; else expires = Math.floor(Date.now() / 1000) + Number(v); }
      if (k === "expires" && expires < 0) { const at = Date.parse(v); if (at <= Date.now()) expired = true; else expires = Math.floor(at / 1000); }
      if (k === "httponly") httpOnly = true;
      if (k === "samesite") sameSite = v[0]?.toUpperCase() + v.slice(1).toLowerCase();
    }
    const key = `${name}\u0000${path}`;
    if (expired) jar.delete(key); else jar.set(key, { name, value, path, expires, httpOnly, sameSite });
    changed = true;
  }
  if (changed) save();
}
const matches = (p, c) => p === c || (p.startsWith(c) && (c.endsWith("/") || p[c.length] === "/"));
async function http(method, path, { json, bearer = false, timeoutMs = 30_000 } = {}) {
  const url = new URL(path, ORIGIN);
  const h = new Headers({ accept: "application/json", "user-agent": "agentrouter-device-e2e/1" });
  if (method !== "GET") { h.set("origin", ORIGIN); h.set("x-agentrouter-csrf", "1"); }
  const cookie = [...jar.values()].filter((c) => matches(url.pathname, c.path)).map((c) => `${c.name}=${c.value}`).join("; ");
  if (cookie) h.set("cookie", cookie);
  if (bearer) h.set("authorization", `Bearer ${await token()}`);
  if (authSession && path.startsWith("/api/newapi/")) h.set("x-auth-session", authSession);
  let body;
  if (json !== undefined) { h.set("content-type", "application/json"); body = JSON.stringify(json); }
  const response = await fetch(url, { method, headers: h, body, redirect: "manual", signal: AbortSignal.timeout(timeoutMs) });
  store(response);
  const text = await response.text();
  let data = null; try { data = JSON.parse(text); } catch { /* not json */ }
  return { status: response.status, data };
}
let accessAt = 0;
async function token() {
  if (access && Date.now() - accessAt < 4 * 60_000) return access;
  const r = await http("POST", "/api/newapi/api/user/auth/refresh");
  if (r.status !== 200 || typeof r.data?.data?.access_token !== "string") throw new Error(`refresh: HTTP ${r.status} ${clip(r.data?.message ?? r.data?.error?.code, 80)}`);
  access = r.data.data.access_token; accessAt = Date.now();
  authSession = r.data.data.session?.id ?? authSession;
  return access;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const list = (d) => (Array.isArray(d) ? d : Array.isArray(d?.devices) ? d.devices : Array.isArray(d?.items) ? d.items : []);

async function status(ags) {
  const r = await http("GET", `/api/control/agent/sessions/${ags}`);
  return r.data?.data?.status ?? `http ${r.status}`;
}
async function turn(ags, maxMs) {
  const start = Date.now();
  let seenRunning = false;
  let last = "";
  while (Date.now() - start < maxMs) {
    const s = await status(ags);
    if (s !== last) { log(`conversation ${s}`); last = s; }
    if (s === "running" || s === "starting" || s === "queued") seenRunning = true;
    if ((seenRunning || Date.now() - start > 30_000) && !["running", "starting", "queued"].includes(s)) return s;
    await sleep(5000);
  }
  return "timeout";
}

load();
await token();
log("session ok");
/** The newest code the runner published (fresh = issued under 9 minutes ago). */
async function newestCode() {
  const response = await fetch(`https://api.github.com/repos/${REPO}/commits/${SHA}/statuses?per_page=100`,
    { headers: { accept: "application/vnd.github+json", "user-agent": "agentrouter-device-e2e" }, signal: AbortSignal.timeout(15_000) });
  if (!response.ok) return null;
  const s = (await response.json()).filter((x) => x.context === "device-e2e/pairing-code" && /^[A-Z2-9]{4}-[A-Z2-9]{4}$/u.test(x.description ?? ""))
    .sort((a, b) => b.created_at.localeCompare(a.created_at))[0];
  return s && Date.now() - Date.parse(s.created_at) < 9 * 60_000 ? s.description : null;
}
let r = null;
const tried = new Set();
const until = Date.now() + 25 * 60_000;
while (Date.now() < until) {
  const code = await newestCode().catch(() => null);
  if (code && !tried.has(code)) {
    tried.add(code);
    r = await http("POST", "/api/control/personal/devices/pair", { json: { code, name: NAME } });
    log(`pair ${code}: HTTP ${r.status} ${r.status === 200 ? "" : clip(JSON.stringify(r.data?.error?.code ?? r.data), 120)}`);
    if (r.status === 200) break;
  }
  await sleep(20_000);
}
if (r?.status !== 200) { log("no pairing"); process.exit(1); }
const paired = r.data?.data?.device?.id;
let device = null;
for (let i = 0; i < 60 && !device; i++) {
  await sleep(3000);
  r = await http("GET", "/api/control/personal/devices");
  const all = list(r.data?.data);
  device = all.find((d) => d.id === paired && d.online === true) ?? null;
  if (i === 59 && !device) log(`devices: ${clip(JSON.stringify(all.map((d) => ({ id: d.id, name: d.name, online: d.online, state: d.state }))), 400)}`);
}
if (!device) { log("device did not come online"); process.exit(1); }
log(`device ${device.id} online`);

r = await http("POST", "/api/control/agent/sessions", { bearer: true, json: { prompt: "你好。先不用做任何事，回复“好的”就行。", kernel: KERNEL, model: MODEL, surface: "chat" }, timeoutMs: 120_000 });
const ags = r.data?.data?.id;
log(`conversation: HTTP ${r.status} ${ags ?? clip(JSON.stringify(r.data), 300)}`);
if (!ags) process.exit(1);
log(`first turn: ${await turn(ags, 5 * 60_000)}`);
r = await http("PUT", `/api/control/personal/devices/${device.id}/sessions/${ags}`, { json: { actions: ["exec", "read", "write"], granted: true } });
log(`grant: HTTP ${r.status}`);
const prompt = `我链接了一台电脑（设备名 ${NAME}），它链接的文件夹是一个小的 Node 项目（calc.js 和 calc.test.js），现在 npm test 是失败的。`
  + "请在那台设备上：先看看这两个文件，用 edit_file 或 apply_patch 修好 calc.js 里的错误（不要改测试），然后在那个文件夹里运行 npm test，告诉我结果。";
r = await http("POST", `/api/control/agent/sessions/${ags}/messages`, { bearer: true, json: { content: prompt }, timeoutMs: 120_000 });
log(`task message: HTTP ${r.status}`);
const end = await turn(ags, 15 * 60_000);
log(`task turn: ${end}`);
r = await http("GET", `/api/control/agent/sessions/${ags}/ui-messages`);
const msgs = Array.isArray(r.data?.data) ? r.data.data : Array.isArray(r.data?.data?.messages) ? r.data.data.messages : [];
const texts = msgs.filter((m) => m.role === "assistant").map((m) => (Array.isArray(m.parts) ? m.parts.filter((p) => p.type === "text").map((p) => p.text).join(" ") : m.content ?? ""));
log(`assistant (last): ${clip(texts.at(-1), 700)}`);
const tools = msgs.flatMap((m) => (Array.isArray(m.parts) ? m.parts : [])).filter((p) => String(p.type ?? "").startsWith("tool") || p.toolName)
  .map((p) => `${p.toolName ?? p.type}${p.input?.action ? `:${p.input.action}` : ""}${p.state ? `(${p.state})` : ""}`);
log(`tool calls: ${clip(tools.join(", "), 900)}`);
log(`conversation id ${ags}`);
if (!KEEP_DEVICE) {
  r = await http("DELETE", `/api/control/personal/devices/${device.id}`);
  log(`device removed: HTTP ${r.status}`);
}
save();
