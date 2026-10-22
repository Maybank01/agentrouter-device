#!/usr/bin/env node
// A red `main` is never silent (issue #125). Run by .github/workflows/main-red.yml when a CI run of a push to main
// completes. Reads only the GitHub API: it never checks out or runs pull request code.
//
//   red, first attempt (an infrastructure failure: up to 3)  re-run the failed jobs once (flake check: srt's own
//                                                            `wfp verify` timed out once on a hosted Windows runner)
//   red again, and a failed job was green on the parent      revert the merged PR: GitHub's own revert PR
//                                                            (GraphQL revertPullRequest, made with the App token so its
//                                                            checks run) with auto-merge, so the revert is tested on its
//                                                            way in and its merge runs CI on main again; a comment on the
//                                                            merged PR; a `main-red` issue
//   red again, no proof it was this merge (parent red or     a `main-red` issue only: never revert an innocent change;
//   unfinished), a revert of a revert, no PR, or GitHub      fix forward
//   refused the revert
//   green                                                    close the open `main-red` issues
//
// The owner's email (Cloud #2623): the one CI alert path, the ops-mail Cloudflare Worker's `POST /alert` (send_email to
// the owner's verified address; agentteam-deployments infrastructure/ops-messaging/cf-mail-worker). Key `main-red/main`:
// one email when main is confirmed red (revert or report, never the first red that is only re-run), none while it stays
// red, one when main is green again; the Worker keeps the streak and a storm guard. Skipped without OPS_MAIL_ALERT_KEY.
//
// Environment: GITHUB_REPOSITORY, RUN_ID (the CI run), GH_TOKEN (the workflow token: actions, issues, pull requests),
// APP_TOKEN (optional: the merge bot App's installation token; without it the issue asks for the revert by hand),
// OPS_MAIL_ALERT_KEY (optional: this repository's key for the ops-mail Worker's /alert),
// DRY_RUN=1 (print the decision, change nothing), DRILL_RERUN=1 (see below). Never prints a token.

export const LABEL = "main-red";
export const ALERT_URL = "https://ops-mail.z1316296346.workers.dev/alert";
export const ALERT_KEY = "main-red/main";
export const INFRA_ATTEMPTS = 3;
const TRAILER = "Reverted-Commit:";
// This guard's trailer, git's `git revert` line and GitHub's revert-button line.
const REVERT_LINE = /^(Reverted-Commit: |This reverts commit |Reverts [\w.-]+\/[\w.-]+#\d+)/u;

/** Failed jobs of one attempt, with their failed steps; `infra` = only the runner failed (`Set up job`). */
export function failedJobs(jobs) {
  return (jobs ?? [])
    .filter((job) => job.conclusion === "failure" || job.conclusion === "timed_out")
    .map((job) => {
      const steps = (job.steps ?? []).filter((step) => step.conclusion === "failure" || step.conclusion === "timed_out").map((step) => step.name);
      return { name: job.name, url: job.html_url, conclusion: job.conclusion, steps, infra: steps.length > 0 && steps.every((name) => name === "Set up job") };
    });
}

/**
 * What to do about one completed CI run of a push to main.
 * `parent`: { run, jobs } of the first parent's CI run (run null when there is none); `message`: the head commit's
 * message; `pr`: the merged PR whose merge commit is the head (null when none).
 */
export function decide({ run, jobs, parent, message, pr }) {
  if (run.conclusion === "success") return { action: "green" };
  if (run.conclusion !== "failure" && run.conclusion !== "timed_out") return { action: "ignore", reason: `the run ended ${run.conclusion}` };
  const failed = failedJobs(jobs);
  if (!failed.length) return { action: "report", failed, reason: "the run failed but no job did (workflow or startup error)" };
  const attempt = run.run_attempt ?? 1;
  if (failed.every((job) => job.infra)) {
    return attempt < INFRA_ATTEMPTS
      ? { action: "rerun", failed, reason: "the runner failed to start the job (infrastructure)" }
      : { action: "report", failed, reason: `the runner failed to start the job on ${attempt} attempts: GitHub-hosted runner trouble, not the change` };
  }
  if (attempt < 2) return { action: "rerun", failed, reason: "flake check: the failed jobs run once more" };
  if (message?.split("\n").some((line) => REVERT_LINE.test(line.trim()))) return { action: "report", failed, reason: "this commit is itself a revert; reverting it would bring back the change it removed" };
  if (!parent?.run) return { action: "report", failed, reason: "the parent commit has no CI run on main, so nothing proves this merge broke it" };
  if (parent.run.status !== "completed") return { action: "report", failed, reason: `the parent commit's CI run is still ${parent.run.status}, so nothing proves this merge broke it yet` };
  const before = new Map((parent.jobs ?? []).map((job) => [job.name, job.conclusion]));
  const broken = failed.filter((job) => before.get(job.name) === "success");
  if (!broken.length) return { action: "report", failed, reason: `already red before this merge: ${failed.map((job) => `${job.name} (${before.get(job.name) ?? "not run"} on the parent)`).join(", ")}` };
  if (!pr) return { action: "report", failed, broken, reason: "this commit came in without a merged pull request, so there is no PR to revert" };
  return { action: "revert", failed, broken, reason: `${broken.map((job) => job.name).join(", ")} passed on the parent and failed here twice` };
}

export function describeFailures(failed) {
  return failed.map((job) => `- [${job.name}](${job.url}) ${job.conclusion}${job.steps.length ? `: ${job.steps.map((s) => `\`${s}\``).join(", ")}` : ""}`).join("\n");
}

/**
 * One condition to the ops-mail Worker's /alert (it dedupes: one email per red streak, one on recovery). Never throws and
 * never fails the guard: an undelivered alert is a log line naming the cause and whether the next call retries it.
 */
export async function ownerAlert(alert, { env, fetchImpl, out }) {
  const key = env.OPS_MAIL_ALERT_KEY?.trim();
  if (!key) { out(`owner alert ${alert.state}: skipped (OPS_MAIL_ALERT_KEY is not set)`); return "skipped"; }
  try {
    const response = await fetchImpl(env.OPS_MAIL_ALERT_URL?.trim() || ALERT_URL, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
      body: JSON.stringify({ key: ALERT_KEY, ...alert, ...(alert.title ? { title: alert.title.replace(/\s+/gu, " ").trim().slice(0, 160) } : {}) }),
      signal: AbortSignal.timeout(15_000),
    });
    const text = await response.text();
    let data = null;
    try { data = JSON.parse(text); } catch { data = null; }
    if (!response.ok) {
      out(`owner alert ${alert.state}: ops-mail /alert answered HTTP ${response.status} (${String(data?.error ?? text).slice(0, 200)}); not delivered${response.status >= 500 ? ", the next call of this condition sends again" : ""}`);
      return "failed";
    }
    out(`owner alert ${alert.state}: ${data?.action ?? "sent"}${data?.reason ? ` (${data.reason})` : ""}`);
    return data?.action ?? "sent";
  } catch (error) {
    out(`owner alert ${alert.state}: ops-mail /alert unreachable (${String(error?.message ?? error).slice(0, 160)}); not delivered, the next call of this condition sends again`);
    return "failed";
  }
}

function client(token, fetchImpl) {
  return async function api(method, path, body) {
    const url = path.startsWith("https://") ? path : `https://api.github.com${path}`;
    const response = await fetchImpl(url, {
      method,
      headers: { accept: "application/vnd.github+json", authorization: `Bearer ${token}`, "x-github-api-version": "2022-11-28", ...(body ? { "content-type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(20_000),
    });
    const text = await response.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch { data = text; }
    return { ok: response.ok, status: response.status, data };
  };
}

async function must(result, what) {
  const r = await result;
  if (!r.ok) throw new Error(`main-red: GitHub API ${what} answered HTTP ${r.status}: ${JSON.stringify(r.data)?.slice(0, 400)}`);
  return r.data;
}

export async function main(env = process.env, fetchImpl = fetch, out = console.log) {
  const repo = env.GITHUB_REPOSITORY;
  const runId = env.RUN_ID;
  if (!repo || !runId || !env.GH_TOKEN) throw new Error("main-red: GITHUB_REPOSITORY, RUN_ID and GH_TOKEN are required");
  const dry = env.DRY_RUN === "1";
  const gh = client(env.GH_TOKEN, fetchImpl);
  const app = env.APP_TOKEN ? client(env.APP_TOKEN, fetchImpl) : null;

  const run = await must(gh("GET", `/repos/${repo}/actions/runs/${runId}`), `run ${runId}`);
  // Drill: re-run a green CI run of main with the workflow token, as the flake check does; its completion must run this
  // guard again (workflow_run, attempt 2). Never on a red run.
  if (env.DRILL_RERUN === "1") {
    if (run.conclusion !== "success" || run.event !== "push" || run.head_branch !== "main") throw new Error(`main-red drill: run ${runId} is ${run.event}/${run.head_branch}/${run.conclusion}; the drill re-runs only a green CI run of a push to main`);
    const rerun = await must(gh("POST", `/repos/${repo}/actions/runs/${runId}/rerun`), `re-run of ${runId}`);
    out(`drill: run ${runId} re-run with the workflow token (${JSON.stringify(rerun)}); its completion must start this guard for attempt ${run.run_attempt + 1}`);
    return "drill";
  }
  if (run.event !== "push" || run.head_branch !== "main") { out(`run ${runId}: ${run.event} on ${run.head_branch}, not a push to main`); return "ignore"; }
  const jobsOf = async (id) => (await must(gh("GET", `/repos/${repo}/actions/runs/${id}/jobs?filter=latest&per_page=100`), `jobs of run ${id}`)).jobs;
  const jobs = await jobsOf(run.id);
  const sha = run.head_sha;
  const commit = await must(gh("GET", `/repos/${repo}/commits/${sha}`), `commit ${sha}`);
  const parentSha = commit.parents?.[0]?.sha;
  let parent = { run: null, jobs: [] };
  if (parentSha) {
    const runs = await must(gh("GET", `/repos/${repo}/actions/workflows/${run.workflow_id}/runs?head_sha=${parentSha}&event=push&branch=main&per_page=20`), `CI runs of ${parentSha}`);
    const newest = (runs.workflow_runs ?? []).sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at))[0];
    if (newest) parent = { run: newest, jobs: newest.status === "completed" ? await jobsOf(newest.id) : [] };
  }
  const pulls = await gh("GET", `/repos/${repo}/commits/${sha}/pulls`);
  const pr = (pulls.ok ? pulls.data : []).find((p) => p.merged_at && p.merge_commit_sha === sha) ?? null;
  const decision = decide({ run, jobs, parent, message: commit.commit?.message, pr });
  const short = sha.slice(0, 12);
  out(`run ${run.id} attempt ${run.run_attempt} at ${short}: ${decision.action}${decision.reason ? ` (${decision.reason})` : ""}`);
  if (dry) return decision.action;

  async function openIssues() {
    const list = await gh("GET", `/repos/${repo}/issues?labels=${LABEL}&state=open&per_page=50`);
    return list.ok ? list.data.filter((issue) => !issue.pull_request) : [];
  }
  async function report(title, body) {
    await gh("POST", `/repos/${repo}/labels`, { name: LABEL, color: "B60205", description: "main went red after a merge (scripts/main-red.mjs)" });
    const existing = (await openIssues()).find((issue) => issue.title.includes(short));
    if (existing) { await must(gh("POST", `/repos/${repo}/issues/${existing.number}/comments`, { body }), `comment on #${existing.number}`); return existing.number; }
    const created = await must(gh("POST", `/repos/${repo}/issues`, { title, body, labels: [LABEL] }), "issue create");
    return created.number;
  }

  if (decision.action === "ignore") return "ignore";
  if (decision.action === "green") {
    for (const issue of await openIssues()) {
      await gh("POST", `/repos/${repo}/issues/${issue.number}/comments`, { body: `main is green again at \`${short}\` (${run.html_url}).` });
      await gh("PATCH", `/repos/${repo}/issues/${issue.number}`, { state: "closed", state_reason: "completed" });
      out(`closed #${issue.number}`);
    }
    await ownerAlert({ state: "resolved", url: run.html_url, text: `main 在 ${short} 转绿（${run.html_url}）。` }, { env, fetchImpl, out });
    return "green";
  }
  const failures = describeFailures(decision.failed ?? []);
  const where = `CI run ${run.html_url} (attempt ${run.run_attempt}) on \`${short}\`${pr ? ` (#${pr.number} ${pr.title})` : ""}`;
  const parentLine = parent.run ? `Parent \`${parentSha.slice(0, 12)}\`: ${parent.run.html_url} (${parent.run.status}${parent.run.conclusion ? `, ${parent.run.conclusion}` : ""}).` : `Parent \`${parentSha?.slice(0, 12) ?? "none"}\`: no CI run on main.`;

  // The re-run's completion runs this guard again (the re-run keeps the push event); the issue is filed now anyway, so
  // a red main is on record from its first red run.
  if (decision.action === "rerun") {
    const rerun = await gh("POST", `/repos/${repo}/actions/runs/${run.id}/rerun-failed-jobs`);
    if (rerun.ok) {
      const issue = await report(`main red at ${short}: ${decision.failed.map((job) => job.name).join(", ")}`, `${where}\n\n${failures}\n\n${parentLine}\n\n**Re-running the failed jobs** (${decision.reason}). Red again → this guard reverts the merge when the parent was green, or reports here; green → this issue closes.`);
      out(`re-ran the failed jobs of run ${run.id}; issue #${issue}`);
      return "rerun";
    }
    decision.reason = `${decision.reason}; the re-run was refused (HTTP ${rerun.status}: ${JSON.stringify(rerun.data)?.slice(0, 200)})`;
  }

  if (decision.action === "revert" && app) {
    const reverted = await app("POST", "/graphql", {
      query: "mutation($id: ID!, $title: String!, $body: String!) { revertPullRequest(input: {pullRequestId: $id, title: $title, body: $body}) { revertPullRequest { id number url } } }",
      variables: {
        id: pr.node_id,
        title: `Revert "${pr.title}" (main went red)`,
        body: `main went red after #${pr.number} and stayed red on a re-run: ${decision.reason}.\n\n${failures}\n\n${where}\n${parentLine}\n\nOpened by scripts/main-red.mjs (#125). It merges by itself once the required checks pass; to keep the change instead, close this PR and fix forward.`,
      },
    });
    const revertPr = reverted.ok && !reverted.data?.errors ? reverted.data?.data?.revertPullRequest?.revertPullRequest : null;
    if (revertPr) {
      const merge = await app("POST", "/graphql", {
        query: "mutation($id: ID!, $body: String!) { enablePullRequestAutoMerge(input: {pullRequestId: $id, mergeMethod: SQUASH, commitBody: $body}) { clientMutationId } }",
        variables: { id: revertPr.id, body: `main went red on ${decision.broken.map((job) => job.name).join(", ")} after this merge.\n\n${TRAILER} ${sha}\nRed-Run: ${run.html_url}` },
      });
      const auto = merge.ok && !merge.data?.errors;
      const autoNote = auto ? "It merges by itself once the required checks pass." : `Auto-merge was refused (${JSON.stringify(merge.data?.errors ?? merge.status)?.slice(0, 300)}): merge it by hand once its checks pass.`;
      await gh("POST", `/repos/${repo}/issues/${pr.number}/comments`, { body: `main went red after this merge and stayed red on a re-run (${decision.reason}). Revert: #${revertPr.number}. ${autoNote} To keep this change, close the revert and fix forward.\n\n${failures}` });
      const issue = await report(`main red at ${short}: ${decision.broken.map((job) => job.name).join(", ")} (reverting #${pr.number})`, `${where}\n\n${failures}\n\n${parentLine}\n\n**Revert:** #${revertPr.number}. ${autoNote} The next green run of main closes this issue.`);
      out(`revert PR #${revertPr.number}${auto ? " (auto-merge on)" : ""}; issue #${issue}`);
      await ownerAlert({ state: "firing", url: `https://github.com/${repo}/issues/${issue}`,
        title: `main 红灯：已开回滚 PR #${revertPr.number}（撤下 #${pr.number} ${pr.title}）`,
        text: `${decision.broken.map((job) => job.name).join(", ")} 在 #${pr.number} 合并后失败，重跑仍失败，而父提交是绿的。已开回滚 PR #${revertPr.number}，${auto ? "检查通过后自动合并" : "自动合并被拒绝，需要手动合并"}。CI：${run.html_url}；issue #${issue}。` }, { env, fetchImpl, out });
      return "revert";
    }
    decision.reason = `${decision.reason}; GitHub refused the revert PR (${JSON.stringify(reverted.data?.errors ?? reverted.data ?? reverted.status)?.slice(0, 300)}), e.g. it conflicts with a later merge`;
  } else if (decision.action === "revert") {
    decision.reason = `${decision.reason}; no App token here (secrets MERGE_BOT_APP_ID / MERGE_BOT_APP_PRIVATE_KEY), so revert #${pr.number} by hand`;
  }

  const issue = await report(`main red at ${short}: ${(decision.failed ?? []).map((job) => job.name).join(", ") || "the CI run"}`, `${where}\n\n${failures || "No job failed; see the run."}\n\n${parentLine}\n\n**Not reverted:** ${decision.reason}. Fix forward; the next green run of main closes this issue.`);
  if (pr && decision.action !== "rerun") await gh("POST", `/repos/${repo}/issues/${pr.number}/comments`, { body: `main is red after this merge; not reverted (${decision.reason}). Issue #${issue}.` });
  out(`issue #${issue}`);
  await ownerAlert({ state: "firing", url: `https://github.com/${repo}/issues/${issue}`,
    title: `main 红灯：${(decision.failed ?? []).map((job) => job.name).join(", ") || "CI"} 失败，没有自动回滚`,
    text: `CI ${run.html_url}（第 ${run.run_attempt} 次）在 ${short}${pr ? `（#${pr.number} ${pr.title}）` : ""} 上失败。没有回滚：${decision.reason}。需要修复（fix forward），下一次 main 转绿会关闭 issue #${issue}。` }, { env, fetchImpl, out });
  return "report";
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((error) => { console.error(String(error?.stack ?? error)); process.exit(1); });
}
