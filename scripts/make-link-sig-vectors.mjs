// Generates the shared test vectors of the link request signature v3 (docs/LINKER-MCP.md §3.3): the device
// aggregator (crates/aggregator/tests/sig_vectors.rs) and the Cloud devices module (protocol.ts) both check
// against the same file. Deterministic: a fixed Ed25519 seed (Ed25519 signatures are deterministic) and fixed
// nonces/expiries. Run: node scripts/make-link-sig-vectors.mjs > crates/aggregator/tests/link-sig-v3-vectors.json
import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";

const seed = Buffer.alloc(32, 7);
const key = createPrivateKey({ key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]), format: "der", type: "pkcs8" });
const publicKey = String(createPublicKey(key).export({ format: "jwk" }).x);
const kid = createHash("sha256").update(Buffer.from(publicKey, "base64url")).digest("hex").slice(0, 16);

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map((item) => canonicalJson(item === undefined ? null : item)).join(",")}]`;
  const entries = Object.entries(value).filter(([, v]) => v !== undefined).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(",")}}`;
}
const sha256 = (text) => createHash("sha256").update(text, "utf8").digest("hex");
const withoutMeta = (params) => Object.fromEntries(Object.entries(params ?? {}).filter(([k]) => k !== "_meta"));
const linkString = (s) => ["agentrouter-link-request/v3", s.link, s.session, s.method, s.digest, s.exp, s.nonce].join("\n");

const LINK = "lnk_0123456789abcdef0123456789abcdef";
const NOW = 1760000000000;

function signed(method, params, over = {}) {
  const body = { link: LINK, session: "ags_conv1", method, digest: sha256(canonicalJson(withoutMeta(params))), exp: NOW + 60_000, nonce: "0".repeat(31) + "1", ...over };
  const sig = sign(null, Buffer.from(linkString(body), "utf8"), key).toString("base64url");
  return { params: { ...params, _meta: { ...(params._meta ?? {}), "agentrouter.top/sig": { v: 3, kid, ...body, sig } } }, string: linkString(body), canonical: canonicalJson(withoutMeta(params)) };
}

const cases = [];
const add = (name, method, built, expect) => cases.push({ name, method, params: built.params, canonical: built.canonical, string: built.string, expect });

add("initialize binds the session", "initialize", signed("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "agentrouter-control-plane", version: "1" } }), "ok");
add("tools/call with unicode keys, numbers and nested arrays", "tools/call", signed("tools/call", { name: "files__write_file", arguments: { path: "src/模组.txt", content: "a b \"q\"", n: [1, 0.1, 1e21, -0, 1e-7], "": 1, "\u{1F600}": 2, B: true, b: null } }, { nonce: "0".repeat(31) + "2" }), "ok");
add("other _meta keys are not signed", "tools/call", (() => { const b = signed("tools/call", { name: "git__git_status", arguments: { repo_path: "." } }, { nonce: "0".repeat(31) + "3" }); b.params._meta.progressToken = 42; b.params._meta["agentrouter.top/turn"] = "t1"; return b; })(), "ok");
add("arguments changed after signing", "tools/call", (() => { const b = signed("tools/call", { name: "files__write_file", arguments: { path: "a.txt", content: "x" } }, { nonce: "0".repeat(31) + "4" }); b.params.arguments.path = "../../.ssh/config"; return b; })(), "SIGNATURE_INVALID");
add("signed for another link", "tools/call", signed("tools/call", { name: "files__read_file", arguments: { path: "a" } }, { link: "lnk_ffffffffffffffffffffffffffffffff", nonce: "0".repeat(31) + "5" }), "WRONG_LINK");
add("signed for another method", "tools/call", signed("initialize", { name: "files__read_file", arguments: { path: "a" } }, { nonce: "0".repeat(31) + "6" }), "SIGNATURE_INVALID");
add("expired", "tools/call", signed("tools/call", { name: "files__read_file", arguments: {} }, { exp: NOW - 1, nonce: "0".repeat(31) + "7" }), "REQUEST_EXPIRED");
add("lives too long", "tools/call", signed("tools/call", { name: "files__read_file", arguments: {} }, { exp: NOW + 66_000, nonce: "0".repeat(31) + "8" }), "REQUEST_EXPIRED");
add("replayed nonce (same as case 2)", "tools/call", signed("tools/call", { name: "files__read_file", arguments: {} }, { nonce: "0".repeat(31) + "2" }), "REPLAYED");
add("missing signature", "tools/call", { params: { name: "files__read_file", arguments: {} }, string: "", canonical: "" }, "SIGNATURE_INVALID");

process.stdout.write(`${JSON.stringify({ about: "Link request signature v3 vectors (docs/LINKER-MCP.md §3.3). Checked in order with one replay cache; now = nowMs.", publicKey, kid, link: LINK, nowMs: NOW, cases }, null, 2)}\n`);
