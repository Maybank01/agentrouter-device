// node --test scripts/ (the Linux job). The decision table of scripts/main-red.mjs and one end-to-end pass on a fake API.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ALERT_URL, decide, failedJobs, main } from "./main-red.mjs";

const job = (name, conclusion, failedStep = "Run cargo test") => ({ name, conclusion, html_url: `https://x/${encodeURIComponent(name)}`, steps: conclusion === "success" ? [] : [{ name: failedStep, conclusion }] });
const red = (attempt, jobs) => ({ run: { conclusion: "failure", run_attempt: attempt }, jobs });
const parentGreen = { run: { status: "completed", conclusion: "success" }, jobs: [job("Sandbox (Windows)", "success"), job("Linux (core + command line)", "success")] };
const pr = { number: 7, node_id: "PR_x", title: "change" };

test("green closes, cancelled is ignored", () => {
  assert.equal(decide({ run: { conclusion: "success" } }).action, "green");
  assert.equal(decide({ run: { conclusion: "cancelled" } }).action, "ignore");
});

test("first red re-runs; an infrastructure failure re-runs up to 3 attempts", () => {
  assert.equal(decide({ ...red(1, [job("Sandbox (Windows)", "failure")]), parent: parentGreen, pr }).action, "rerun");
  const infra = [job("Sandbox (Windows)", "failure", "Set up job")];
  assert.equal(failedJobs(infra)[0].infra, true);
  assert.equal(decide({ ...red(2, infra), parent: parentGreen, pr }).action, "rerun");
  assert.equal(decide({ ...red(3, infra), parent: parentGreen, pr }).action, "report", "runner trouble is never the change's fault");
});

test("red twice after a green parent reverts the merged PR", () => {
  const d = decide({ ...red(2, [job("Sandbox (Windows)", "failure"), job("Linux (core + command line)", "success")]), parent: parentGreen, pr, message: "change (#7)" });
  assert.equal(d.action, "revert");
  assert.deepEqual(d.broken.map((j) => j.name), ["Sandbox (Windows)"]);
});

test("never reverts without proof, a revert, or without a PR", () => {
  const twice = red(2, [job("Sandbox (Windows)", "failure")]);
  assert.equal(decide({ ...twice, parent: { run: null }, pr }).action, "report");
  assert.equal(decide({ ...twice, parent: { run: { status: "in_progress" }, jobs: [] }, pr }).action, "report");
  assert.equal(decide({ ...twice, parent: { run: { status: "completed" }, jobs: [job("Sandbox (Windows)", "failure")] }, pr }).action, "report");
  assert.equal(decide({ ...twice, parent: parentGreen, pr, message: "Revert x\n\nReverted-Commit: abc" }).action, "report");
  assert.equal(decide({ ...twice, parent: parentGreen, pr, message: "Revert \"x\"\n\nReverts Maybank01/agentrouter-device#7" }).action, "report");
  assert.equal(decide({ ...twice, parent: parentGreen, pr: null }).action, "report");
});

function fakeApi(routes) {
  const calls = [];
  const fetchImpl = async (url, init) => {
    const path = url.replace("https://api.github.com", "");
    const body = init.body ? JSON.parse(init.body) : null;
    calls.push({ method: init.method, path, body, auth: init.headers.authorization });
    const hit = routes.find(([m, p]) => m === init.method && (typeof p === "string" ? p === path : p.test(path)));
    const [status, data] = hit ? (typeof hit[2] === "function" ? hit[2](body) : hit[2]) : [404, { message: "no route" }];
    return { ok: status < 300, status, text: async () => JSON.stringify(data) };
  };
  return { calls, fetchImpl };
}

test("end to end: red twice → revert PR with auto-merge, PR comment, main-red issue", async () => {
  const R = "/repos/o/r";
  const { calls, fetchImpl } = fakeApi([
    ["GET", `${R}/actions/runs/9`, [200, { id: 9, event: "push", head_branch: "main", head_sha: "b".repeat(40), conclusion: "failure", run_attempt: 2, workflow_id: 1, html_url: "https://run/9" }]],
    ["GET", `${R}/actions/runs/9/jobs?filter=latest&per_page=100`, [200, { jobs: [job("Sandbox (Windows)", "failure")] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}`, [200, { parents: [{ sha: "a".repeat(40) }], commit: { message: "change (#7)" } }]],
    ["GET", /\/actions\/workflows\/1\/runs\?head_sha=a{40}/u, [200, { workflow_runs: [{ id: 8, status: "completed", conclusion: "success", created_at: "2026-10-22T00:00:00Z", html_url: "https://run/8" }] }]],
    ["GET", `${R}/actions/runs/8/jobs?filter=latest&per_page=100`, [200, { jobs: [job("Sandbox (Windows)", "success")] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}/pulls`, [200, [{ number: 7, node_id: "PR_7", title: "change", merged_at: "x", merge_commit_sha: "b".repeat(40) }]]],
    ["POST", "/graphql", (body) => [200, body.query.includes("revertPullRequest") ? { data: { revertPullRequest: { revertPullRequest: { id: "PR_8", number: 8, url: "u" } } } } : { data: { enablePullRequestAutoMerge: { clientMutationId: null } } }]],
    ["POST", `${R}/labels`, [201, {}]],
    ["GET", `${R}/issues?labels=main-red&state=open&per_page=50`, [200, []]],
    ["POST", `${R}/issues`, [201, { number: 10 }]],
    ["POST", `${R}/issues/7/comments`, [201, {}]],
  ]);
  const lines = [];
  const result = await main({ GITHUB_REPOSITORY: "o/r", RUN_ID: "9", GH_TOKEN: "w", APP_TOKEN: "a" }, fetchImpl, (l) => lines.push(l));
  assert.equal(result, "revert");
  const graphql = calls.filter((c) => c.path === "/graphql");
  assert.equal(graphql.length, 2);
  assert.ok(graphql.every((c) => c.auth === "Bearer a"), "the revert and auto-merge use the App token");
  assert.match(graphql[1].body.variables.body, /Reverted-Commit: b{40}/u);
  assert.ok(calls.some((c) => c.method === "POST" && c.path === `${R}/issues` && c.body.labels[0] === "main-red"));
  assert.ok(calls.some((c) => c.path === `${R}/issues/7/comments` && /#8/u.test(c.body.body)));
  assert.ok(!lines.join("\n").includes("Bearer"), "never prints a token");
});

test("end to end: first red → failed jobs re-run with the workflow token and the issue is filed at once", async () => {
  const R = "/repos/o/r";
  const { calls, fetchImpl } = fakeApi([
    ["GET", `${R}/actions/runs/9`, [200, { id: 9, event: "push", head_branch: "main", head_sha: "b".repeat(40), conclusion: "failure", run_attempt: 1, workflow_id: 1, html_url: "https://run/9" }]],
    ["GET", `${R}/actions/runs/9/jobs?filter=latest&per_page=100`, [200, { jobs: [job("Sandbox (Windows)", "failure")] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}`, [200, { parents: [{ sha: "a".repeat(40) }], commit: { message: "change (#7)" } }]],
    ["GET", /\/actions\/workflows\/1\/runs\?head_sha=a{40}/u, [200, { workflow_runs: [] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}/pulls`, [200, []]],
    ["POST", `${R}/actions/runs/9/rerun-failed-jobs`, [201, {}]],
    ["POST", `${R}/labels`, [422, {}]],
    ["GET", `${R}/issues?labels=main-red&state=open&per_page=50`, [200, []]],
    ["POST", `${R}/issues`, [201, { number: 11 }]],
  ]);
  assert.equal(await main({ GITHUB_REPOSITORY: "o/r", RUN_ID: "9", GH_TOKEN: "w", APP_TOKEN: "a" }, fetchImpl, () => {}), "rerun");
  assert.equal(calls.find((c) => c.path.endsWith("/rerun-failed-jobs")).auth, "Bearer w");
  assert.match(calls.find((c) => c.method === "POST" && c.path === `${R}/issues`).body.body, /Re-running the failed jobs/u);
});

test("owner email (Cloud #2623): confirmed red fires main-red/main, green resolves it, a first red re-run mails nothing", async () => {
  const R = "/repos/o/r";
  const base = (conclusion, attempt) => [
    ["GET", `${R}/actions/runs/9`, [200, { id: 9, event: "push", head_branch: "main", head_sha: "b".repeat(40), conclusion, run_attempt: attempt, workflow_id: 1, html_url: "https://github.com/o/r/actions/runs/9" }]],
    ["GET", `${R}/actions/runs/9/jobs?filter=latest&per_page=100`, [200, { jobs: [job("Sandbox (Windows)", conclusion === "success" ? "success" : "failure")] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}`, [200, { parents: [{ sha: "a".repeat(40) }], commit: { message: "change (#7)" } }]],
    ["GET", /\/actions\/workflows\/1\/runs\?head_sha=a{40}/u, [200, { workflow_runs: [] }]],
    ["GET", `${R}/commits/${"b".repeat(40)}/pulls`, [200, []]],
    ["POST", `${R}/actions/runs/9/rerun-failed-jobs`, [201, {}]],
    ["POST", `${R}/labels`, [422, {}]],
    ["GET", `${R}/issues?labels=main-red&state=open&per_page=50`, [200, []]],
    ["POST", `${R}/issues`, [201, { number: 12 }]],
  ];
  const alerts = [];
  const run = async (conclusion, attempt, env = { OPS_MAIL_ALERT_KEY: "k" }) => {
    const { fetchImpl } = fakeApi(base(conclusion, attempt));
    const lines = [];
    const wrapped = async (url, init) => {
      if (url.startsWith("https://ops-mail.")) { alerts.push({ url, auth: init.headers.authorization, body: JSON.parse(init.body) }); return { ok: true, status: 200, text: async () => JSON.stringify({ ok: true, action: "sent" }) }; }
      return fetchImpl(url, init);
    };
    const result = await main({ GITHUB_REPOSITORY: "o/r", RUN_ID: "9", GH_TOKEN: "w", ...env }, wrapped, (l) => lines.push(l));
    assert.ok(!lines.join("\n").includes("Bearer"), "never prints a key");
    return result;
  };
  assert.equal(await run("failure", 1), "rerun");
  assert.equal(alerts.length, 0, "a first red is only re-run: no email");
  assert.equal(await run("failure", 2), "report");
  assert.equal(alerts.length, 1);
  assert.equal(alerts[0].url, ALERT_URL);
  assert.equal(alerts[0].auth, "Bearer k");
  assert.deepEqual([alerts[0].body.key, alerts[0].body.state], ["main-red/main", "firing"]);
  assert.match(alerts[0].body.title, /^main 红灯：Sandbox \(Windows\) 失败/u);
  assert.equal(alerts[0].body.url, "https://github.com/o/r/issues/12");
  assert.equal(await run("success", 1), "green");
  assert.deepEqual([alerts[1].body.key, alerts[1].body.state], ["main-red/main", "resolved"]);
  assert.equal(await run("failure", 2, {}), "report");
  assert.equal(alerts.length, 2, "without OPS_MAIL_ALERT_KEY nothing is sent");
});
