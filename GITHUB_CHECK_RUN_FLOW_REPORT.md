# How the "Lightbridge Review" GitHub check gets started and resolved

Scope: the exact code path from a GitHub event (PR opened/synced, or an `@lightbridge-assistant`
mention) to the "Lightbridge Review" check appearing `in_progress` on the PR, through to it
resolving `success`/`failure`/`timed_out`/`cancelled` — covering both the **fast** (automatic,
on-open) and **deep** (mention-triggered) presets. All paths/line numbers are in
`services/control-plane/src`.

---

## 1. Where "fast" vs "deep" gets decided

Two entry points create a review task, each resolving to a different **default** preset (an
operator can override this per repo via `.lightbridge-code-review.jsonc`):

**`preset.rs:39-67`** — `EntryPoint::platform_default_preset()`:

```rust
pub(crate) fn platform_default_preset(self) -> &'static str {
    match self {
        // PrSync fires once per push — same cheap tier as the initial open, deliberately not
        // deep: at N pushes per PR, deep would multiply spend by N for no proportional benefit.
        EntryPoint::PrOpen | EntryPoint::PrSync => "fast",
        EntryPoint::Mention | EntryPoint::A2a | EntryPoint::Mcp => "deep",
    }
}
```

| Trigger | GitHub event | `EntryPoint` | Default preset | Handler |
|---|---|---|---|---|
| PR opened | `pull_request` (`action: opened`) | `PrOpen` | `fast` | `webhook.rs:1502` `handle_pull_request` |
| New commits pushed | `pull_request` (`action: synchronize`) | `PrSync` | `fast` | same handler |
| `@lightbridge-assistant …` comment | `issue_comment` (`action: created`) | `Mention` | `deep` | `webhook.rs:1755` `handle_issue_comment` |

The actual resolution (repo config vs. platform default) happens in
`crate::settings::resolve_preset_and_settings` (called from both handlers), which fetches
`.lightbridge-code-review.jsonc` at the PR's **base** ref (fork-safe — a fork PR can't rewrite its
own preset by editing the file on its own branch) and falls back to the table above when the repo
declares nothing.

---

## 2. Webhook → task row (no check-run yet)

Both handlers end by inserting a `tasks` row — **not** by touching GitHub. Nothing is posted to the
PR at this point.

- **PR open/sync** — `webhook.rs:1636-1661`, `create_review_task` → `crate::db::create_task`.
- **Mention** — `webhook.rs:1879-1894`, → `crate::db::create_explicit_task`.

Each task row carries `check_runs_enabled` (a per-repo opt-out, epic #566) snapshotted at creation
time, so the open-gate and the resolve-gate later can never disagree about whether a check exists
for this task.

---

## 3. Dispatcher claims the task and **opens** the check run

**`queue/dispatcher.rs`** — the dispatcher polls for claimable tasks, launches the Kubernetes Job,
and only *after* the Job is confirmed launched does it open the check:

```rust
// dispatcher.rs:330-352
match db::set_task_job(pool, task.id, &job_name).await {
    Ok(()) => tracing::info!(task_id = %task.id, job_name, "dispatched task to a Job"),
    ...
}
// ADR-0068: 👀 means "seen AND work started" — enqueue it now the agent Job is launched
react_work_started(pool, task, review).await;
// Cosmetic runner-status reporting: open a check/status on the PR's head SHA now the Job is launched
start_check_run_signal(pool, task).await;
```

`start_check_run_signal` (`dispatcher.rs:429-461`) gates on the task actually being a PR task with
a known `head_sha` and `check_runs_enabled`, then calls:

```rust
crate::outbox::enqueue_check_run_start(pool, &t, context.target_id, head_sha).await
```

This is the same code path for **both** fast and deep — the check-run mechanics don't know or care
which preset the task is running; they only care that it's a PR-scoped task.

---

## 4. The egress outbox (ADR-0059) — why this isn't a direct API call

`enqueue_check_run_start` (`outbox.rs:230-254`) does **not** call GitHub. It writes a row to an
outbox table (`enqueue_outbox_post`, intent `"check_run_start"`, deduped on
`"<task_id>:check_run:start"`). The **reconciler** is the sole consumer that actually posts:

```rust
// queue/reconciler.rs:319-320
"check_run_start" => deliver_check_run_start(pool, platform, repo, row).await,
"check_run_resolve" => deliver_check_run_resolve(pool, platform, repo, row).await,
```

The reconciler drains the outbox on a Postgres `LISTEN`/`NOTIFY` trigger (near-instant), with a
polling fallback (`run_outbox_drain`, `reconciler.rs:80`) in case the notify connection drops.

`deliver_check_run_start` (`reconciler.rs:393-420`) first runs a **staleness guard**
(`check_run_task_is_stale`, `reconciler.rs:367-385`) — if a *newer*, non-cancelled task already
exists for the same PR head SHA, this delivery is skipped (epic #571's supersede/debounce
push-storm handling; stops an old run's check from clobbering a newer one). Otherwise it calls the
platform trait:

```rust
let external_id = platform.start_check_run(repo, CheckRunStart { head_sha: &p.head_sha, details_url: None }).await?;
```

---

## 5. The actual GitHub API call

**`integrations/github.rs:318-351`** — `create_check_run_in_progress`:

```rust
pub async fn create_check_run_in_progress(&self, token: &str, owner: &str, repo: &str,
    head_sha: &str, details_url: Option<&str>) -> anyhow::Result<i64> {
    let response = self.http
        .post(format!("https://api.github.com/repos/{owner}/{repo}/check-runs"))
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&Self::check_run_start_payload(head_sha, details_url))
        .send().await.context("opening check run")?;
    ...
}
```

The request body (`check_run_start_payload`, `github.rs:252-260`):

```rust
{ "name": CHECK_RUN_NAME, "head_sha": head_sha, "status": "in_progress" }
```

`CHECK_RUN_NAME` is defined once, in **`integrations/platform.rs:186`**:

```rust
pub const CHECK_RUN_NAME: &str = "Lightbridge Review";
```

— this is the literal string that shows up in the PR's checks list ("Lightbridge Review — In
progress"). The returned check-run `id` is persisted back onto the task
(`db::set_check_run_external_id`, `reconciler.rs:414-418`) so the resolve step can PATCH the *same*
check rather than opening a second one.

**End-to-end so far:** webhook → task row → dispatcher launches Job → outbox row →
reconciler → `POST /check-runs` (`status: in_progress`) → GitHub shows the check running.

---

## 6. Resolution — three different paths reach the same resolve code

All three funnel into `enqueue_check_run_resolve` (`outbox.rs:273-303`), which enqueues a
`"check_run_resolve"` row the reconciler later delivers via `deliver_check_run_resolve`
(`reconciler.rs:423-…`) → `platform.resolve_check_run` → GitHub's
`integrations/github.rs:1110-1146`:

```rust
match req.external_id {
    Some(id) => self.update_check_run_completed(...).await,   // PATCH the SAME check
    None => self.create_check_run_completed(...).await,       // self-healing: create one already-completed
}
```

The `None` branch (`create_check_run_completed`, `github.rs:393-426`) is a deliberate self-heal:
if the `start` intent never got an id recorded (e.g. it dead-lettered on a permissions issue), the
resolve still gives the PR *some* check on that SHA rather than none.

### 6a. Normal finish (review posted, or run genuinely completed)

`http/internal.rs` — three call sites at lines **1929**, **2151**, **2189** — cover the
`finished`/`exhausted`/`aborted` review outcomes reaching `client.finalize_review(...)`. This is
the same code regardless of fast/deep; the *content* of the check (`render_check_output`,
`review.rs`) differs based on findings count and conclusion, not the preset.

### 6b. Explicit runner failure (`status: "failed"` / `"timed_out"`)

`http/internal.rs`'s `set_status` handler — only these two runner-reported statuses trigger
`handle_review_failure`, which reacts 😕, posts the ADR-0056 fallback PR comment, **and** resolves
the check to `Failure`/`TimedOut`:

```rust
let check_conclusion = match update.status.as_str() {
    "failed" => Some(CheckConclusion::Failure),
    "timed_out" => Some(CheckConclusion::TimedOut),
    _ => None,
};
```

### 6c. Superseded or reaper-recovered runs

- **Superseded** (`webhook.rs:1972-1996`, `supersede_older_reviews`): when a new push cancels an
  older automatic run for the same PR, each cancelled task's check is resolved to `Cancelled` via
  `internal.rs::resolve_cancelled_check_run` — so a killed run never hangs "in progress" forever.
- **Stuck/uncatchably-killed Jobs** (`queue/reaper.rs:239`): a task whose Job died without ever
  reporting a status (pod OOM-killed, node died, etc.) gets marked `failed` by the reaper after
  `MAX_ATTEMPTS` (`= 5`) retries, and the same `enqueue_check_run_resolve` call resolves its check.

---

## 7. One known gap in this flow (found earlier this session)

`set_status`'s `check_conclusion` match (§6b) only fires on literal `"failed"`/`"timed_out"`
runner statuses. A task that reports **`"succeeded"`** while carrying a review-failure detail (the
runner's own review loop crashed, but *indexing* — which the runner tracks as the task's overall
success/failure — completed) does **not** go through §6b at all: no 😕, no fallback comment, and
critically **no check-run resolution**. That's the mechanism behind the "CI still shows running,
but the dashboard says succeeded" behavior investigated earlier — the check genuinely never gets a
terminal PATCH, because nothing in this flow is wired to call `enqueue_check_run_resolve` for that
specific state.

---

## 8. Summary diagram

```
GitHub event (pull_request / issue_comment)
        │
        ▼
webhook.rs: handle_pull_request / handle_issue_comment
   - EntryPoint::{PrOpen,PrSync} → default "fast"
   - EntryPoint::Mention         → default "deep"
   - resolve_preset_and_settings() may override from .lightbridge-code-review.jsonc
        │
        ▼
crate::db::create_task / create_explicit_task   (tasks row only — nothing posted yet)
        │
        ▼
queue/dispatcher.rs: claims task → launches K8s Job → set_task_job() succeeds
        │
        ├─ react_work_started()        (👀 reaction)
        └─ start_check_run_signal()    (PR task + head_sha + check_runs_enabled only)
                │
                ▼
        outbox::enqueue_check_run_start()   → outbox row, intent "check_run_start"
                │
                ▼ (Postgres LISTEN/NOTIFY, near-instant)
        queue/reconciler.rs: deliver_check_run_start()
                │  (staleness guard: skip if a newer task for the same PR/SHA exists)
                ▼
        integrations/github.rs: create_check_run_in_progress()
                │
                ▼
        POST /repos/{owner}/{repo}/check-runs   { name: "Lightbridge Review", status: "in_progress" }
                │
                ▼
        ══════ GitHub shows "Lightbridge Review — In progress" on the PR ══════

   ... agent-runner Job does the actual review/index work ...

        │
        ├─ normal finish → client.finalize_review() → internal.rs (3 call sites)
        ├─ runner reports "failed"/"timed_out" → set_status → handle_review_failure()
        ├─ newer push supersedes this run → supersede_older_reviews() → Cancelled
        └─ Job dies silently → reaper.rs (after MAX_ATTEMPTS=5) → Failed
                │
                ▼ (all four paths converge here)
        outbox::enqueue_check_run_resolve()   → outbox row, intent "check_run_resolve"
                │
                ▼
        queue/reconciler.rs: deliver_check_run_resolve() → resolve_check_run()
                │
                ▼
        PATCH /repos/{owner}/{repo}/check-runs/{id}   { status: "completed", conclusion: ... }
        (or POST .../check-runs already-completed, self-healing, if no id was ever recorded)
                │
                ▼
        ══════ GitHub shows the final ✓ / ✗ / ⊘ on the PR ══════
```
