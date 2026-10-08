// Test vectors for the share-code protocol (DEVICE-PROTOCOL.md §11.3, §13.1, §13.2): the four strings a
// device signs with its own key, password normalisation, and request v2 / session_open signed by the
// control plane. Ed25519 is deterministic, so the output is stable:
//   node services/control-plane/src/linked-devices/make-share-vectors.mjs > services/control-plane/src/linked-devices/share-vectors.json
// Standalone on purpose (no import of protocol.ts): the protocol test checks protocol.ts reproduces it,
// and the device repository checks its Rust implementation against the same file.
import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map((item) => canonicalJson(item === undefined ? null : item)).join(",")}]`;
  const entries = Object.entries(value).filter(([, v]) => v !== undefined).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(",")}}`;
}
const sha256 = (text) => createHash("sha256").update(text, "utf8").digest("hex");
const keyFrom = (byte) => {
  const seed = Buffer.alloc(32, byte);
  const key = createPrivateKey({ key: Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]), format: "der", type: "pkcs8" });
  return { seedHex: seed.toString("hex"), key, pub: String(createPublicKey(key).export({ format: "jwk" }).x) };
};
const signText = (key, text) => sign(null, Buffer.from(text, "utf8"), key).toString("base64url");

const ALPHABET = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const normalisePassword = (input) => {
  const value = input.toUpperCase().replace(/[\s-]/g, "");
  return value.length === 8 && [...value].every((c) => ALPHABET.includes(c)) ? value : null;
};

const device = keyFrom(9);
const control = keyFrom(7);
const kid = createHash("sha256").update(Buffer.from(control.pub, "base64url")).digest("hex").slice(0, 16);
const deviceId = "lnd_" + "a".repeat(32);
const shareId = "shr_" + "b".repeat(32);
const session = "rms_" + "c".repeat(32);
const at = 1760000000000;
const password = normalisePassword("abcd-efgh");

const share = {
  share: shareId, access: "confirm", folders: ["D:\\mods", "C:\\Users\\张三\\Desktop"], expiresAt: 1760003600000, approval: "every_session",
  modes: ["remote", "delegate"], delegateCapPoints: 100, password, at,
};
const modes = [...new Set(share.modes)].sort().join(",");
const shareText = ["agentrouter-device-share/v1", deviceId, shareId, share.access, sha256(canonicalJson(share.folders)), share.expiresAt, share.approval, modes,
  share.delegateCapPoints, sha256(password), at].join("\n");
const passwordText = ["agentrouter-device-share-password/v1", deviceId, shareId, sha256("MNPQ2345"), at + 1].join("\n");
const stopText = ["agentrouter-device-share-stop/v1", deviceId, shareId, "person", at + 2].join("\n");
const capText = ["agentrouter-device-share-cap/v1", deviceId, shareId, 100, at + 3].join("\n");

const requestV2 = (input) => {
  const digest = sha256(canonicalJson(input.args));
  const text = ["agentrouter-device-request/v2", deviceId, session, shareId, input.client, input.action, digest, at + 60_000, input.nonce].join("\n");
  return { text, request: { v: 2, kid, device: deviceId, session, share: shareId, client: input.client, action: input.action, digest, exp: at + 60_000, nonce: input.nonce, sig: signText(control.key, text) }, args: input.args };
};

const sessionOpenArgs = {
  session, share: shareId, mode: "remote",
  who: { account: "prn_" + "2".repeat(32), name: "林小满", email: "li***@qq.com", registeredDays: 2, newAccount: true, reported: false },
  via: { kind: "client", device: "lnd_" + "d".repeat(32), deviceName: "林小满的笔记本", client: "AgentRouter" },
  owner: { account: "prn_" + "2".repeat(32), name: "林小满" }, instructors: ["prn_" + "2".repeat(32)],
  limits: { access: "confirm", maxMinutes: 30 }, expiresAt: 1760001800000,
};

console.log(JSON.stringify({
  device: { seedHex: device.seedHex, publicKey: device.pub, id: deviceId },
  control: { seedHex: control.seedHex, publicKey: control.pub, kid },
  passwords: [["abcd-efgh", "ABCDEFGH"], [" ABCD EFGH ", "ABCDEFGH"], ["mnpq2345", "MNPQ2345"], ["ABCD-EFG1", null], ["ABCDEFGHJ", null], ["ABCD-EFGI", null], ["ABC", null]]
    .map(([input, expected]) => { if (normalisePassword(input) !== expected) throw new Error(`password vector ${input}`); return { input, normalised: expected }; }),
  share: { body: share, text: shareText, signature: signText(device.key, shareText) },
  password: { body: { password: "MNPQ2345", at: at + 1 }, text: passwordText, signature: signText(device.key, passwordText) },
  stop: { body: { reason: "person", at: at + 2 }, text: stopText, signature: signText(device.key, stopText) },
  cap: { body: { addPoints: 100, at: at + 3 }, text: capText, signature: signText(device.key, capText) },
  requestV2: requestV2({ action: "exec", client: "Claude Code", nonce: "0123456789abcdef0123456789abcdef", args: { command: "dotnet build", cwd: "D:\\mods\\MyMod", timeout: 30 } }),
  sessionOpen: requestV2({ action: "session_open", client: "", nonce: "fedcba9876543210fedcba9876543210", args: sessionOpenArgs }),
}, null, 1));
