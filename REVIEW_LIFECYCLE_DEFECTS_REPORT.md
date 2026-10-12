# Review lifecycle defects: cancel, MCP diff scoping, and the a328be78 stall

**Date:** 2026-09-17
**Scope:** three reported problems in the review lifecycle, traced to root cause in the source.
**Evidence:** `services/control-plane/src`, `services/agent-runner/src`, `services/review-agent/src`,
and `logs/Logs for a328be78-0b46-44b0-a3d6-743c4582be4c-logs-2026-09-17 17_03_42.txt` (144 Loki lines,
PR `ADORSYS-GIS/lightbridge-authz#755`).

---

## Summary

| # | Defect | Where | Impact |
|---|---|---|---|
| 1 | Manual cancel never resolves the GitHub check run | `queue/tasks.rs:117` | Check hangs "in progress" forever; PR looks blocked |
| 2 | `check_run_start` staleness guard ignores the task's own status | `db/tasks.rs:692` | A cancel racing the outbox drain opens an unresolvable check |
| 3 | PR-closed cancel path also never resolves the check | `http/webhook.rs:1664` | Same as #1, quieter |
| 4 | MCP `start_review` hardcodes `base_sha: None` | `mcp/tools.rs:117` | Every MCP review runs **unscoped** — no diff, agent has no subject |
| 5 | MCP bypasses repo preset config | `mcp/tools.rs:105-121` | `.lightbridge-code-review.jsonc` preset override ignored |
| 6 | Self-cancel exits 143 → Kubernetes retries the Job | `run.rs:156` + `k8s.rs:589` | A cancelled deep review **restarts from scratch** |
| 7 | `set_task_status` has no terminal-state guard | `db/tasks.rs:771` | The retry resurrects `cancelled` → `running`; cancel must be issued twice |
| 8 | No timeout on an ACP prompt cycle | `opencode/drive.rs:38` | A hung sub-agent burns up to the full 1h Job deadline in silence |

Defects 6 and 7 are the pair that wasted ~50 minutes of runtime and a full duplicate deep review in the
attached log. They are also the cheapest to fix.

---

## 0. System context (for anyone reading this cold)

Lightbridge is a forge App for code review and repo Q&A. A Rust control plane runs in three roles —
`serve` (HTTP API + webhooks), `dispatcher` (queue consumer, launches one ephemeral Kubernetes Job per
task) and `reconciler` (drains the GitHub-egress outbox; ADR-0059 makes it the **sole writer** to the
forge). The Job (`agent-runner`) clones, indexes into Neo4j (structure) + pgvector (semantics), and
drives the review.

Reviews are two-tier (ADR-0062): `fast` — diff-only, no retrieval — on PR open/sync; `deep` — full
retrieval, multi-turn — on `@mention`, A2A and MCP. As of ADR-0097 slice 5 the review agent is
**OpenCode driven over ACP** with mediated tools, not the retired native Rust loop.

The "Lightbridge Review" check run is opened by the dispatcher after the Job launches
(`dispatcher.rs::start_check_run_signal` → `outbox::enqueue_check_run_start`) and resolved later by a
`check_run_resolve` outbox row. Both are delivered by the reconciler. Nothing else may talk to GitHub.

---

## 1. Cancelling from the dashboard leaves the check "in progress"

### Root cause

`POST /tasks/{id}/cancel` ([`queue/tasks.rs:117`](services/control-plane/src/queue/tasks.rs)) does
exactly one thing and then returns `204`:

```rust
match crate::db::cancel_task_by_id(pool, id).await {
    Ok(true) => {
        tracing::info!(task_id = %id, by = %caller.claims.identity(), "task cancelled (manual)");
        StatusCode::NO_CONTENT.into_response()
    }
    ...
}
```

`db::cancel_task_by_id` ([`db/tasks.rs:357`](services/control-plane/src/db/tasks.rs)) is a bare
`UPDATE tasks SET status = 'cancelled' …`. **No `check_run_resolve` row is ever enqueued**, so the
check opened at dispatch never receives its terminal `PATCH` and GitHub keeps rendering
"Lightbridge Review — In progress" indefinitely.

The dashboard is confirmed to hit this endpoint: `apps/web/lib/server/api.ts:131-136`.

### The helper already exists and is already generic

`internal::resolve_cancelled_check_run(pool, task_id)`
([`http/internal.rs:2324`](services/control-plane/src/http/internal.rs)) takes a task id, looks up its
context, and enqueues a `check_run_resolve` with `CheckConclusion::Cancelled`. It is called from
exactly one place — `supersede_older_reviews`
([`http/webhook.rs:1988`](services/control-plane/src/http/webhook.rs)), the push-storm path from epic
#566 — which is why a *superseded* run resolves correctly and a *manually cancelled* one does not.

### Every cancel path, audited

| Path | Code | Resolves the check? |
|---|---|---|
| Superseded by a newer push | `webhook.rs:1988` | ✅ |
| **Manual cancel (dashboard / API)** | `queue/tasks.rs:117` | ❌ |
| PR/MR closed → `cancel_active_tasks_for_pr` | `webhook.rs:1664` (GitHub), `:802` (GitLab), `:1237` (Bitbucket) | ❌ |
| Reaper deleting a cancelled task's Job | `queue/reaper.rs:155` | ❌ |

The PR-closed case is less visible (the PR is closed anyway) but leaves the same dangling check if the
PR is later reopened. The reaper case is a backstop that never fires because nothing upstream of it
resolves either.

### Second defect: a race that survives fixing the above

Before delivering **either** check-run intent, the reconciler consults
`check_run_task_is_stale` ([`queue/reconciler.rs:367`](services/control-plane/src/queue/reconciler.rs)),
which delegates to `db::should_report_check_run`
([`db/tasks.rs:692`](services/control-plane/src/db/tasks.rs)):

```sql
SELECT NOT EXISTS (
    SELECT 1 FROM tasks newer
    JOIN tasks self ON self.id = $1
    WHERE newer.repository_id = self.repository_id
      AND newer.target_type   = self.target_type
      AND newer.target_id     = self.target_id
      AND newer.head_sha IS NOT DISTINCT FROM self.head_sha
      AND newer.status <> 'cancelled'
      AND (newer.created_at, newer.id) > (self.created_at, self.id)
)
```

It asks only *"does a newer non-cancelled task exist for this head SHA?"* — it **never inspects the
task's own status**. So a cancel landing in the window between dispatch and the reconciler draining the
outbox still lets `check_run_start` fire, opening an `in_progress` check for an already-dead task. A
resolve that was enqueued earlier cannot retract it, because the resolve is delivered first and the
start second (`(created_at, id)` ordering).

### Fix

1. Call `resolve_cancelled_check_run` from `tasks::cancel` and from each
   `cancel_active_tasks_for_pr` loop (the function already returns the cancelled ids).
2. Parameterise its currently-hardcoded title/summary — `"Review superseded" / "A newer commit was
   pushed…"` is wrong wording for a user-initiated cancel.
3. Add a self-status clause to `should_report_check_run` so a `check_run_start` is skipped when the
   task is already terminal:
   `AND self.status NOT IN ('succeeded','failed','timed_out','cancelled')` guarding the start
   specifically (the resolve must still be delivered for a terminal task).

### Related, out of scope for the cancel fix

`GITHUB_CHECK_RUN_FLOW_REPORT.md` §7 already documents a sibling gap: a task reporting `"succeeded"`
while carrying a review-failure detail never reaches `handle_review_failure`, so its check is never
resolved either. Same symptom ("CI still running, dashboard says done"), different trigger. Worth
fixing in the same pass since both want a single "every terminal task resolves its check" invariant.

---

## 2. An MCP-triggered review has no diff

### Symptom

The review agent's own transcript on an MCP-started run:

> No diff available. I need to review "against the working tree" but with no diff there's no subject
> of review. […] Env says "Is directory a git repo: no". Without a diff, I genuinely cannot review.

### Root cause: `base_sha` is hardcoded `None`

`mcp::tools::start_review` ([`mcp/tools.rs:117`](services/control-plane/src/mcp/tools.rs)) builds its
task row with:

```rust
let new_task = db::NewTask {
    ...
    base_sha: None,                        // ← every MCP task, unconditionally
    head_sha: Some(head_sha.to_string()),
    ...
};
```

The runner computes the review's scope in `clone::pr_diff`
([`agent-runner/src/clone.rs:85`](services/agent-runner/src/clone.rs)):

```rust
pub async fn pr_diff(checkout: &Path, ctx: &TaskContext, filter: Option<&DiffFilter>) -> Option<PrDiff> {
    let base = ctx.base_sha.as_deref()?;   // ← short-circuits to None for every MCP task
    let head = ctx.head_sha.as_deref()?;
    ...
}
```

`None` propagates into the prompt builder, which takes the fallback branch at
[`review-agent/src/prompt.rs:338`](services/review-agent/src/prompt.rs):

```rust
None => user.push_str(
    "\n\nNo diff is available for this run; answer or review against the working tree and \
     keep every claim grounded in the tools.",
),
```

That sentence is verbatim what the agent was reacting to.

### Why the webhook paths *do* have a diff

They resolve both SHAs from the forge before creating the task:

- **Mention (GitLab)** — `webhook.rs:967-994` calls `CodePlatform::pr_shas(...)` and stores the
  `(base, head)` pair; there is even an extra API fallback at `webhook.rs:692-707` for when the
  webhook payload's `diff_refs` comes back empty.
- **PR open/sync (GitHub)** — base/head come straight off the `pull_request` payload.
- **A2A** — accepts `baseSha` from the caller and documents the exact consequence of omitting it in
  its agent card ([`a2a/card.rs:42`](services/control-plane/src/a2a/card.rs)):

  > OPTIONAL but STRONGLY RECOMMENDED base commit SHA […] When ABSENT, no diff can be computed and the
  > review FALLS BACK to the WHOLE WORKING TREE at headSha — a broader, unfocused audit of the entire
  > repo snapshot, NOT the PR's delta.

MCP does none of this. Worse, `StartReviewArgs`
([`mcp/handler.rs:48`](services/control-plane/src/mcp/handler.rs)) has **no `base_sha` field at all**,
so an MCP client cannot supply what the server refuses to resolve:

```rust
pub struct StartReviewArgs {
    pub platform: String,
    pub org: String,
    pub repo: String,
    pub pr_number: i64,
    /// The PR's head commit SHA. This server has no tool to look it up — get it from the
    /// platform's own API/MCP server (e.g. GitHub's) first.
    pub head_sha: String,
    pub prompt: Option<String>,
}
```

This is a pure omission, not a design constraint. `start_review` already holds the repository row and
`installation_id`, and `pr_shas` is implemented for all three platforms
([`integrations/github.rs:977`](services/control-plane/src/integrations/github.rs), GitLab at
`gitlab.rs:627`).

### The compounding factor: OpenCode's cwd is not the checkout

This part is **by design** (ADR-0099 config isolation,
[`review/opencode.rs:255-275`](services/agent-runner/src/review/opencode.rs)):

```rust
let workdir = std::env::temp_dir().join(format!("lci-opencode-review-{task_id}"));
// ⚠️ CONFIG ISOLATION (security): opencode MERGES config from its cwd's `opencode.json` and from
// the global HOME/XDG config over our `OPENCODE_CONFIG`. The checkout is UNTRUSTED (a PR from a
// fork could ship an `opencode.json` that re-enables built-in tools / bash, injects an MCP server
// that runs commands, or swaps the model …). So opencode runs with a NEUTRAL cwd (this workdir,
// never the checkout) and an EMPTY HOME/XDG — its ONLY config is ours. File reads still reach the
// checkout via `lci-review-mcp` (LCI_MCP_CHECKOUT) …
```

The attached log confirms it on every session event:
`"directory":"/tmp/lci-opencode-review-a328be78-…","worktree":"/"`.

So on an MCP run the agent is handed, simultaneously: **no diff**, a prompt telling it to *"review
against the working tree"*, and a cwd that is an empty non-git directory. There is genuinely no subject
of review. The checkout is reachable — but only through the mediated `read_file` / search tools, which
the fallback wording does not say.

### Fix

1. In `mcp::tools::start_review`, resolve `(base_sha, head_sha)` via `CodePlatform::pr_shas` the way
   the mention handler does. This also lets `head_sha` become **optional** in `StartReviewArgs`,
   removing the awkward "go ask GitHub's own MCP server first" instruction.
2. Optionally accept an explicit `base_sha` override for parity with A2A.
3. Reword the no-diff fallback in `prompt.rs:338`: post-ADR-0099 the working tree is *not* the agent's
   cwd, and the prompt should name the mediated tools as the only route to it.
4. Consider making `start_review` refuse (or loudly degrade) when it cannot scope a diff, rather than
   silently spending a deep-tier run that cannot produce a grounded review.

### Bonus defect found on the way: MCP ignores repo config

`start_review` calls `resolve_repo_settings_db_only(pool, repo.id)` and takes the preset straight from
`EntryPoint::Mcp.platform_default_preset()` (`mcp/tools.rs:105-121`). The webhook paths instead call
`settings::resolve_preset_and_settings`, which fetches `.lightbridge-code-review.jsonc` at the PR's
**base** ref (fork-safe) and lets the repo override the preset. **An MCP-triggered review therefore
ignores the repo's declared preset**, always running the platform default (`deep`).

---

## 3. Incident analysis: task `a328be78-0b46-44b0-a3d6-743c4582be4c`

`@lightbridge-assistant deep review` on `ADORSYS-GIS/lightbridge-authz#755`, head
`89806e86f8f80768dfb9cde9c40f86faa7e3630b`.

### Timeline

```
14:59:32.244  created explicit review task (webhook, issue_comment, pr=755)
14:59:32.263  dispatched to Job lightbridge-agent-a328be78-…
14:59:33.182  fetched task context
14:59:35.653  repo already indexed — reusing base index
14:59:36.585  prompt built: diff IS present — 8210 bytes, 6 files (4 rendered, 2 low-signal)
14:59:36.586  ⚠ 4× ADR-0099 floor-breach warnings (see (d) below)
14:59:36.810  22 tools offered, incl. 10 discovered external-knowledge tools
14:59:45.880  opencode spawn+initialize complete (9069 ms) — github_mcp_offered: false
14:59:46.570  session ses_f501e7f0… created  agent=build  model={id:reviewer, variant:low}
              directory=/tmp/lci-opencode-review-a328be78-…  worktree=/
15:00–15:02   normal cycle: agent.reasoning → tool.done → session.diff [] (×4)
15:03:00.257  SECOND session ses_f501b8a6… created, carries parentID  ← `task` sub-agent spawned
15:03:00–15:04:04  three sub-agent tool cycles
15:04:04.869  last event of any kind
              ████████████  45 MINUTES OF TOTAL SILENCE  ████████████
15:49:02.544  task cancelled (manual) — christian.leghadjeu@adorsys.com
15:49:03.144  runner: "task no longer active upstream (cancelled); aborting promptly"
15:49:13.964  *** fetched task context ***  ← THE ENTIRE REVIEW RESTARTS
15:49:16.366  repo already indexed — reusing base index
15:49:17.328  prompt rebuilt — byte-identical 8210-byte diff; same 4 floor-breach warnings
15:49:29.484  opencode re-spawned (11929 ms), session ses_f4ff0f7d…
15:50:38.310  agent working normally
15:50:44.342  task cancelled (manual) — SECOND cancel required
15:50:53.995  runner aborts again
15:52:25.016  reaper: stopped cancelled task's Job
```

Total: ~53 minutes wall clock, one hung deep review plus one **entirely duplicated** deep review, two
manual cancels, and no check-run resolution at any point (defect #1).

### (a) Cancelling caused a full restart — `backoffLimit: 1` vs exit code 143

The runner's self-cancel arm exits with `143`
([`agent-runner/src/run.rs:136-157`](services/agent-runner/src/run.rs)):

```rust
enum RunOnceOutcome { Ran(anyhow::Result<RunResult>), Terminated, CancelledUpstream }
...
match outcome {
    // The control plane already owns a cancelled row in both cases; reporting here would clobber
    // it with a status it doesn't have …
    RunOnceOutcome::Terminated | RunOnceOutcome::CancelledUpstream => {
        std::process::ExitCode::from(143)
    }
```

143 (`128 + SIGTERM`) is deliberate and correct for the *SIGTERM* arm. But the Job manifest
([`integrations/k8s.rs:589`](services/control-plane/src/integrations/k8s.rs)) is:

```json
"backoffLimit": 1,
"activeDeadlineSeconds": <AGENT_JOB_DEADLINE_SECONDS, default 3600>,
"ttlSecondsAfterFinished": 900,
"restartPolicy": "Never"
```

With `restartPolicy: Never`, Kubernetes reads any non-zero container exit as a Job failure and — with
`backoffLimit: 1` — schedules **one more pod**. That second pod re-cloned, re-prompted and re-ran the
entire deep review for a task the user had explicitly killed 10 seconds earlier. This is the single
largest waste in the incident.

**Fix:** exit `0` on `CancelledUpstream`. The code's own comment already establishes that the control
plane owns the terminal row, so a zero exit loses nothing. (The SIGTERM arm can keep 143 — when the
reaper deletes the Job there is no Job left to retry.)

### (b) The retry resurrected a cancelled task — which is why two cancels were needed

The second pod's first action is to report `running`
([`run.rs:253`](services/agent-runner/src/run.rs)), and `db::set_task_status`
([`db/tasks.rs:771`](services/control-plane/src/db/tasks.rs)) applies it with **no terminal-state
guard**:

```sql
UPDATE tasks SET
    status = $2,
    started_at   = CASE WHEN $2 = 'running' THEN COALESCE(started_at, now()) ELSE started_at END,
    completed_at = CASE WHEN $3 THEN now() ELSE completed_at END,
    lease_owner  = CASE WHEN $3 THEN NULL ELSE lease_owner END,
    lease_expires_at = CASE WHEN $3 THEN NULL ELSE lease_expires_at END,
    error_detail = CASE WHEN $2 = 'running' THEN NULL ELSE COALESCE($4, error_detail) END
WHERE id = $1
```

`cancelled → running` therefore succeeded silently, leaving an incoherent row (status `running`,
`completed_at` still stamped from the cancel). Three consequences, all visible in the log:

- The runner's own self-cancel poll (`cancelled_upstream`, `run.rs:208`, every 10 s, terminal-status
  check at `run.rs:224`) saw `running` and kept going — so the pod that should have died within 10 s
  ran for another 90 s until the *second* manual cancel.
- The reaper's `list_cancelled_with_job` (`db/tasks.rs:371`, `WHERE status = 'cancelled'`) no longer
  matched the row, so the Job was not deleted at 15:49:1x. It was only reaped at 15:52:25, after the
  second cancel.
- The dashboard showed a run the user had already cancelled as alive again.

**Fix:** guard non-terminal transitions —
`AND status NOT IN ('succeeded','failed','timed_out','cancelled')` — so a terminal row is immutable.
`set_task_status` already returns `bool`; a `false` here is the correct signal to the caller that the
task is gone.

### (c) A 45-minute stall with no backstop below the 1-hour Job deadline

`run_review_loop` ([`review-agent/src/opencode/drive.rs:31-44`](services/review-agent/src/opencode/drive.rs)):

```rust
pub async fn run_review_loop<S: ReviewSession>(
    session: &mut S, driver: &mut ReviewDriver, first_prompt: &str,
) -> anyhow::Result<ReviewResolution> {
    let mut prompt = first_prompt.to_string();
    loop {
        let events = session.prompt(&prompt).await?;   // ← no timeout, ever
        let outcome = cycle_turn_outcome(&events);
        match driver.on_cycle(&outcome) {
            DriveAction::Prompt(next) => prompt = next,
            DriveAction::Finalize(resolution) => return Ok(resolution),
        }
    }
}
```

`review.max_cycles` bounds how many prompt *cycles* run (deliberately, because
`agent.build.maxSteps` is proven not to cap anything over ACP — see the e2e canary at
`review/opencode.rs:1175-1315`). It does nothing when a **single** `session/prompt` never returns. The
last event before the silence was inside the `task` sub-agent session created at 15:03:00, so the
sub-agent's cycle is what hung.

The only backstop is the Job's `activeDeadlineSeconds` — default **3600** (`deploy/config-reference.md:107`,
`local-dev-guide.md:666`). So a hang costs up to a full hour of pod time and provider spend, in total
silence, with the check run still showing "in progress" the whole way.

**Fix:** a per-cycle timeout on `ReviewSession::prompt` (host-side, since `drive.rs` is
transport-agnostic), and/or an idle watchdog on the recorder event stream that aborts after N seconds
without an event. Either should resolve as `Aborted(reason)` so `finalize_review_outcome` still posts a
visible artifact — the net invariant `finalize_review_outcome` documents ("every review run leaves a
VISIBLE artifact unless the gateway was unreachable") is violated today by a hang.

### (d) The operator overlay re-enabled the sub-agent that hung

Four ADR-0099 floor-breach warnings fired identically on both runs
(`review/opencode.rs:225-231` emits them from `rendered.floor_breaches`):

```
built-in tool `task` re-enabled (coverage may go blind)
agent `explore` re-enabled built-in tool `bash` (coverage may go blind for that agent)
agent `explore` re-enabled built-in tool `webfetch` (coverage may go blind for that agent)
agent `explore` opened permission `bash` (no longer "deny")
agent `explore` opened permission `webfetch` (no longer "deny")
```

The re-enabled `task` tool is what spawned the sub-session at 15:03:00 that then hung. Separately,
**re-enabling `bash` for `explore` is near-useless by construction**: per §2, that bash runs in the
empty `/tmp/lci-opencode-review-*` workdir, not the checkout. The overlay is paying coverage blindness
(the gates cannot see inside a sub-agent's tool calls) for a tool that cannot see the repository.

Recommend reviewing the `review.opencode` operator overlay in ai-helm-values: at minimum drop the
`bash` re-enable, and weigh whether `task` earns its cost given it is the proximate cause of the hang.

### (e) Two smaller observations

- **`github_mcp_offered: false`.** ADR-0105's read-only GitHub MCP attaches only when the preset's
  `review.tools` explicitly lists an `mcp__github__…` selector *and* the platform is GitHub
  (`review/opencode.rs:241`). The `deep` preset lists none, so the agent had no way to look at the PR
  itself — it saw only the 22 tools in the allowlist.
- **Model.** The deep review ran on `glm-5.3-flash` with `model: {id: reviewer, providerID: eaig,
  variant: low}`. A fast/cheap model at the low variant on the expensive tier is a plausible
  contributor to the sub-agent thrash and the eventual stall; worth checking against the intended
  deep-tier model mapping.

---

## 4. Recommended fix order

Ordered by (cost of the bug) ÷ (size of the fix).

1. **`set_task_status` terminal-state guard** (`db/tasks.rs:771`) — one SQL clause. Stops cancelled
   runs being resurrected, restores the runner's 10-second self-cancel and the reaper's ability to see
   the row. *Needs a regression test: cancelled → running must be rejected.*
2. **Exit `0` on `CancelledUpstream`** (`run.rs:155-157`) — one line. Stops Kubernetes duplicating a
   cancelled deep review. Together with (1) this is what burned the 50 minutes.
3. **Resolve the check run on every cancel** — call `resolve_cancelled_check_run` from
   `tasks::cancel` and the `cancel_active_tasks_for_pr` loops; parameterise its title/summary. Fixes
   the reported "CI still shows running".
4. **Self-status clause in `should_report_check_run`** (`db/tasks.rs:692`) — closes the
   cancel-races-the-outbox window that (3) alone cannot.
5. **MCP resolves `(base_sha, head_sha)` via `pr_shas`** (`mcp/tools.rs:117`) — makes MCP reviews
   diff-scoped like every other entry point, and lets `head_sha` become optional.
6. **Per-cycle timeout / idle watchdog on the ACP prompt** (`opencode/drive.rs:38` + the agent-runner
   host's `ReviewSession::prompt`) — bounds a hang to minutes instead of the 1-hour Job deadline.
7. **Prompt wording + MCP preset resolution** — reword the no-diff fallback (`prompt.rs:338`) for the
   post-ADR-0099 cwd model; switch MCP to `resolve_preset_and_settings`.
8. **Operator overlay review** (ai-helm-values `review.opencode`) — drop the `bash` re-enable for
   `explore`; reconsider `task`.

Per this repo's refactor discipline, items 1–2 are behaviour fixes and belong in their own PR,
separate from 3–4 (check-run lifecycle) and 5/7 (MCP entry-point parity).

---

## Appendix: how this was verified

- Control-plane and runner sources read at the commit checked out on 2026-09-17; every line reference
  above was confirmed by direct grep against the working tree, not from memory.
- The incident timeline was reconstructed by sorting the Loki export chronologically and decoding both
  log shapes it interleaves: the Rust `tracing` JSON (`target`/`fields.message`) and the OpenCode
  plugin's own JSON (`service: "lci-opencode"`, with `properties.sessionID` / `properties.info`).
  The `directory`, `worktree`, `agent`, `model` and `parentID` facts in §2 and §3 come from the
  `session.created` / `session.updated` payloads.
- No code was changed as part of this analysis.
