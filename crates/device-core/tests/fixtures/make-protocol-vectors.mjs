import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";
// Same canonical JSON as agentrouter-cloud services/control-plane/src/linked-devices/protocol.ts
function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map((item) => canonicalJson(item === undefined ? null : item)).join(",")}]`;
  const entries = Object.entries(value).filter(([, v]) => v !== undefined).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(",")}}`;
}
const seed = Buffer.alloc(32, 7);
const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]);
const key = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
const pub = String(createPublicKey(key).export({ format: "jwk" }).x);
const kid = createHash("sha256").update(Buffer.from(pub, "base64url")).digest("hex").slice(0, 16);
const argsObj = {
  timeout: 30,
  command: 'Get-ChildItem "D:\\x" | % { $_.Name } # 中文 😀 \u0001\t\u007f',
  cwd: "D:\\mods",
  z: [1, 2.5, 1e21, -0, null, true, { b: 1, a: "\u2028<>&" }],
  "\uE000": 1,
  "\uD83D\uDE00": 2,
  Zed: 3,
  big: 12345678901234567890,
  small: 1e-7,
  f: 0.1,
  neg: -3.25e-10,
  mid: 123456.789,
};
const argsText = JSON.stringify(argsObj);
const canon = canonicalJson(JSON.parse(argsText));
const digest = createHash("sha256").update(canon, "utf8").digest("hex");
const r = { device: "lnd_" + "a".repeat(32), session: "ags_test", action: "exec", digest, exp: 1760000060000, nonce: "0123456789abcdef0123456789abcdef" };
const text = `agentrouter-device-request/v1\n${r.device}\n${r.session}\n${r.action}\n${r.digest}\n${r.exp}\n${r.nonce}`;
const sig = sign(null, Buffer.from(text, "utf8"), key).toString("base64url");
const nums = [0, -0, 1, -1, 0.1, 1.5, 100, 1e20, 1e21, 1.5e21, 123456789012345680000, 1e-6, 1e-7, 1.234e-7, 2 ** 53, 5e-324, 1.7976931348623157e308, 123456.789, -3.25e-10];
console.log(JSON.stringify({
  seedHex: seed.toString("hex"), pub, kid, argsText, canon, digest, request: { v: 1, kid, ...r, sig },
  numbers: nums.map((n) => [JSON.stringify(n), canonicalJson(JSON.parse(JSON.stringify(n)))]),
}, null, 1));
