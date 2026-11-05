// Confirms a ZITADEL device login (RFC 8628) in a browser, as a person does: the client's confirmation address,
// login name, password, Allow. Dev Login v2 (login-dev.agentrouter.page/ui/v2/login/device), English UI.
//
//   node approve.mjs <file with the confirmation address>
//
// PERSON_FILE names a file with `username=` and `password=` lines (never argv or a printed value, R21/R38);
// BROWSER the browser executable (playwright-core drives an installed Chrome); SHOT where to save the last page.
import fs from "node:fs";
import { chromium } from "playwright-core";

const url = fs.readFileSync(process.argv[2], "utf8").trim();
const person = Object.fromEntries(
  fs.readFileSync(process.env.PERSON_FILE, "utf8").trim().split(/\r?\n/).map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]),
);
if (!person.username || !person.password) throw new Error("approve: PERSON_FILE needs username= and password= lines");

const browser = await chromium.launch({ executablePath: process.env.BROWSER, headless: true });
const page = await browser.newPage({ locale: "en-US" });
const step = async (what, fn) => {
  try {
    await fn();
    console.log(`ok   ${what}`);
  } catch (error) {
    const text = (await page.locator("body").innerText().catch(() => "")).replace(/\s+/g, " ").slice(0, 300);
    throw new Error(`approve: ${what} failed at ${new URL(page.url()).pathname}: ${error.message.split("\n")[0]} (page: ${text})`);
  }
};
try {
  const continueButton = () => page.locator("button[type=submit]", { hasText: "Continue" });
  await step("device page with the code filled in", async () => {
    await page.goto(url, { waitUntil: "networkidle" });
    await continueButton().click();
  });
  await step("login name", async () => {
    await page.locator("input[name=loginName]").fill(person.username);
    await continueButton().click();
  });
  await step("password", async () => {
    await page.locator("input[type=password]").fill(person.password);
    await continueButton().click();
  });
  await step("consent: Allow", async () => {
    const allow = page.getByRole("button", { name: "Allow" });
    await allow.click();
    await allow.waitFor({ state: "detached", timeout: 20_000 });
  });
  const text = (await page.locator("body").innerText()).replace(/\s+/g, " ");
  if (process.env.SHOT) await page.screenshot({ path: process.env.SHOT });
  console.log(`page after Allow: ${new URL(page.url()).pathname}: ${text.slice(0, 160)}`);
} finally {
  await browser.close();
}
