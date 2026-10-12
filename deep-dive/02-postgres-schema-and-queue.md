# 2. Postgres: the work queue and its tables

Postgres is the control plane's system of record (ADR-0002, file `01`). This chapter covers the
`tasks` table as a work queue, the `SELECT ... FOR UPDATE SKIP LOCKED` claim pattern that makes
concurrent dispatchers safe, the outbox pattern that makes GitHub/GitLab/Bitbucket egress
single-writer, webhook dedup, and the supporting tables around reviews and feedback.

**A naming note before anything else**, because it's the first thing that will confuse you if you
cross-reference `docs/components-and-data-models.md`: that doc (and the original migration
`0020_github_outbox.sql`) names two tables `github_deliveries` and `github_outbox`. Migration
`0024_platform_abstraction.sql` **renamed both** — `github_deliveries` → `webhook_deliveries`,
`github_outbox` → `outbox` — as part of ADR-0072's GitHub/GitLab/Bitbucket platform abstraction, and
added a `platform` column to both (default `'github'`, so existing rows kept their meaning). This
chapter uses the *current* names, verified directly against
`services/control-plane/migrations/0024_platform_abstraction.sql:28-36` and the live query text in
`services/control-plane/src/db/outbox.rs`. If you read the older docs or ADR-0059's own prose
(written before the rename), mentally substitute `outbox` for `github_outbox` and
`webhook_deliveries` for `github_deliveries`.

## 2.1 Migrations are the schema — read them, not a diagram

`services/control-plane/migrations/` is 41 files as of this writing, applied in order by
`sqlx::migrate!` on every startup (`docs/components-and-data-models.md:60-67`, cross-checked against
the actual directory listing). The rule, stated in that doc and worth repeating because it's a hard
invariant: **migrations are checksum-verified — never edit an applied one; add a new numbered one.**
Every schema claim in this chapter is quoted from the migration that introduced or changed it, not
from a mental model of "the schema" — because with 41 sequential ALTERs, the only ground truth is
the SQL itself.

## 2.2 `tasks` — the queue

### The columns, by the migration that added them

The table starts in `0001_init.sql:36-51`:

```sql
CREATE TABLE IF NOT EXISTS tasks (
    id                 UUID PRIMARY KEY,
    repository_id      BIGINT NOT NULL REFERENCES repositories (id),
    installation_id    BIGINT NOT NULL,
    github_delivery_id TEXT   NOT NULL REFERENCES github_deliveries (delivery_id),
    target_type        TEXT   NOT NULL,
    target_id          BIGINT NOT NULL,
    command_text       TEXT   NOT NULL,
    base_sha           TEXT,
    head_sha           TEXT,
    status             TEXT   NOT NULL,
    priority           INT    NOT NULL DEFAULT 100,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at         TIMESTAMPTZ,
    completed_at       TIMESTAMPTZ
);
```

At this point `tasks` is just a log of received work — nothing here makes it a *queue* yet. That
comes two migrations later. `github_delivery_id` is later renamed `webhook_delivery_id`
(`0024_platform_abstraction.sql:26`) and made nullable (`docs/components-and-data-models.md:156` —
"admin-initiated index tasks have no webhook"); `github_repo_id`-style naming throughout the schema
gets the same platform-neutral treatment in that same migration.

`0003_task_queue.sql` is the migration that turns `tasks` into a queue (file header, quoted in
full): "RFC-0001 Phase 1: make the `tasks` table a work queue consumed by the dispatcher via `SELECT
... FOR UPDATE SKIP LOCKED`, and make webhook task creation idempotent." It adds exactly the columns
a lease-based queue needs:

```sql
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS attempts         INT         NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS run_after        TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS run_epoch        INT         NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS lease_owner      TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS job_name         TEXT;
```

Read each column as a specific queue mechanic:

- **`run_after`** — a task isn't claimable until `now() >= run_after`; this is what lets a task be
  enqueued but deliberately delayed (e.g. a push-storm debounce — `NewTask::run_after_secs`,
  `services/control-plane/src/db/tasks.rs:103-108`).
- **`run_epoch`** — a re-run discriminator. The same `(repository_id, target_type, target_id,
  command_text, head_sha)` tuple can legitimately run more than once (an explicit `@mention`
  re-review); `run_epoch` is the column that lets a second run coexist with the first without
  tripping the idempotency index below.
- **`lease_owner` / `lease_expires_at`** — the dispatcher's claim lease. Whichever dispatcher pod
  claims a task stamps its own identity and a short expiry; the reaper (file `01`, under
  `dispatcher`) later uses an expired lease as a signal to reconcile, not as proof the task is dead.
- **`job_name`** — the Kubernetes Job this task was dispatched to, recorded so the console can stream
  its logs and the reaper can delete it.
- **`priority` / `attempts`** — already present from `0001`; `priority DESC` governs claim order,
  `attempts` increments on every claim.

`priority` and `status` (TEXT, snake_case) mirror the `TaskStatus` enum in
`services/control-plane/src/types.rs` (`docs/components-and-data-models.md:76-80`):

```rust
pub enum TaskStatus {
    Received, WaitingForIndex, Queued, Running, PostingResult,
    Succeeded, Failed, TimedOut, Cancelled,
}
```

Two more columns matter for understanding *why* a task runs the way it does, both added well after
`0003`: `preset` (added as `tier` in `0021_task_tier.sql`, renamed in `0033_task_preset.sql` — "Hard
cutover, no compat column (this repo's convention)") and `entry_point` (added in the same `0033`
migration). `0033`'s own comment is the clearest statement of what changed: `tier` was a hardcoded
`"fast"`/`"deep"` literal; `preset` is "an arbitrary operator-configured name resolved per repo
config," and `entry_point` (`"pr_open"` / `"mention"` / `"a2a"`) was split out as a *separate* column
specifically because "preset names are now arbitrary/operator-defined, so a decision like 'post the
quick-pass banner vs the full truncation note' can no longer key off a preset name."

### The idempotency index

Quoted verbatim, `0003_task_queue.sql`:

```sql
-- At most one task per normalized command + target + head SHA (per run epoch). NULLS NOT DISTINCT
-- (Postgres 15+) so a null head_sha cannot bypass the guard; run_epoch lets an explicit re-run
-- create a new version later without colliding (RFC-0001).
CREATE UNIQUE INDEX IF NOT EXISTS tasks_idempotency_idx
    ON tasks (repository_id, target_type, target_id, command_text, head_sha, run_epoch)
    NULLS NOT DISTINCT;
```

`NULLS NOT DISTINCT` is the detail easiest to skip past and most load-bearing: ordinary SQL unique
indexes treat every `NULL` as distinct from every other `NULL`, which would let an unbounded number
of rows share `(repo, target, command, NULL head_sha, 0 epoch)` and defeat the whole guard for any
task whose `head_sha` is absent (e.g. a repository-level `index` task). `NULLS NOT DISTINCT`
(Postgres 15+) makes two `NULL`s collide like any other equal value, closing that gap.

This index is what makes `create_task` (`services/control-plane/src/db/tasks.rs:164-211`) safe to
call on every GitHub `opened`/`synchronize` event without producing duplicate work — the `INSERT ...
ON CONFLICT (repository_id, target_type, target_id, command_text, head_sha, run_epoch) DO NOTHING`
simply no-ops on a redelivery. The *explicit* command path (`create_explicit_task`,
`tasks.rs:228-276`) deliberately bypasses this for human `@mention`s — "an explicit mention always
lands a task" — by computing a fresh `run_epoch` via `COALESCE(MAX(run_epoch), -1) + 1` inside the
same `INSERT`, retrying on a `23505` unique-violation race (up to 5 attempts) rather than silently
deduping a second human request.

### The claim query — `FOR UPDATE SKIP LOCKED`, verbatim

This is the mechanism the whole chapter builds up to. From
`services/control-plane/src/db/tasks.rs:397-420`:

```rust
/// Atomically claim the next due `queued` task and take a short dispatch lease. `FOR UPDATE SKIP
/// LOCKED` guarantees that concurrent dispatcher replicas never claim the same row. Returns `None`
/// when nothing is due. (Lease expiry is reaped by the scheduler in RFC-0001 Phase 2.)
pub async fn claim_next_task(
    pool: &PgPool,
    owner: &str,
    lease: Duration,
) -> Result<Option<ClaimedTask>, sqlx::Error> {
    sqlx::query_as::<_, ClaimedTask>(
        "UPDATE tasks \
         SET status = 'running', attempts = attempts + 1, started_at = now(), \
             lease_owner = $1, lease_expires_at = now() + ($2 * interval '1 second') \
         WHERE id = ( \
           SELECT id FROM tasks \
           WHERE status = 'queued' AND run_after <= now() \
           ORDER BY priority DESC, created_at, id \
           FOR UPDATE SKIP LOCKED \
           LIMIT 1 \
         ) \
         RETURNING id, repository_id, installation_id, target_type, target_id, command_text, \
                   base_sha, head_sha, attempts, trace_context",
    )
    .bind(owner)
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await
}
```

Walk the mechanics once, slowly, because the pattern recurs (the outbox drain in §2.5 uses the exact
same clause):

- The `SELECT id FROM tasks WHERE status = 'queued' AND run_after <= now() ORDER BY ... LIMIT 1 FOR
  UPDATE SKIP LOCKED` subquery picks exactly one candidate row and takes a row-level lock on it.
- **`SKIP LOCKED`** is the whole point: if another transaction (another dispatcher replica) already
  holds a lock on some `queued` row — because it's mid-claim on it right now — this query simply
  skips that row and looks at the next one, instead of blocking until the lock releases. Two
  dispatcher pods running this query at the same instant will, with overwhelming probability, each
  walk away with a *different* row — no explicit coordination, no distributed lock service, just
  Postgres's own row-lock semantics.
- The outer `UPDATE ... WHERE id = (subquery)` is what actually claims it: in the same statement, the
  one row the subquery found gets flipped to `status = 'running'`, stamped with this caller's
  `lease_owner` and a `lease_expires_at` a configurable number of seconds out, and `attempts`
  incremented. Subquery and update run as one atomic statement — there's no window between "I found
  a candidate" and "I claimed it" where a second caller could grab the same row.
- `ORDER BY priority DESC, created_at, id` is FIFO-within-priority: higher `priority` first, then
  oldest `created_at`, then `id` as a final tiebreak for rows created in the same transaction (where
  `now()` — and therefore `created_at` — is identical).

The supporting index that keeps this off a sequential scan, from the same migration
(`0003_task_queue.sql`):

```sql
CREATE INDEX IF NOT EXISTS tasks_queue_idx
    ON tasks (priority DESC, created_at)
    WHERE status = 'queued';
```

A **partial** index — it only indexes rows where `status = 'queued'`, so the index stays small
regardless of how many millions of `succeeded`/`failed` rows accumulate over the table's lifetime.
`docs/components-and-data-models.md:191` names its sibling, `tasks_reapable_idx` (partial on `WHERE
status IN ('running','posting_result')`), used by the reaper's own scan
(`services/control-plane/src/db/tasks.rs:464-477`, `list_reapable_tasks`) for the exact same reason.

### The `run_epoch` column lets explicit re-runs coexist with idempotency

Worth connecting explicitly: the idempotency index in §2.2 keys on `run_epoch`, and
`create_explicit_task` is the *only* caller that varies it per call (computed inline via the
`COALESCE(MAX(run_epoch), -1) + 1` subquery quoted above). Every automatic path (`create_task`) just
passes a fixed `run_epoch: 0` from its `NewTask` struct. So the idempotency guard and the "a human
can always get a fresh run" escape hatch are the same unique index, read two different ways by two
different call sites — not two separate mechanisms.

## 2.3 The outbox pattern — one writer to the forge

### Why: ADR-0059

Before the outbox, two roles wrote to GitHub — `serve` posted synchronously inside the `finalize`
request, and the poller posted failure notices separately — and that split forced an accidental
"settle buffer" hack to stop the two writers racing each other (ADR-0059's Context section,
`docs/adr/0059-reconciler-owns-all-github-egress.md:17-28`). The fix: **every outbound GitHub/GitLab/
Bitbucket *content* write becomes an intent row in a table; exactly one role (the reconciler, file
`01`) ever posts.**

### The table

Current schema, reconstructed from `0020_github_outbox.sql` (original) plus the renames and
additions in `0024_platform_abstraction.sql`, `0029_open_pr_egress.sql`, `0031_trace_context.sql`,
and `0035_check_run_reporting.sql` — this is the schema `db/outbox.rs`'s queries actually run
against today:

```sql
CREATE TABLE outbox (
    id              BIGSERIAL PRIMARY KEY,
    task_id         UUID REFERENCES tasks (id) ON DELETE CASCADE,
    platform        TEXT   NOT NULL DEFAULT 'github',
    installation_id BIGINT NOT NULL,
    owner           TEXT   NOT NULL,
    repo            TEXT   NOT NULL,
    kind            TEXT   NOT NULL CHECK (kind IN (
                        'review', 'reply', 'reaction', 'label', 'failure_notice',
                        'pr_open', 'check_run_start', 'check_run_resolve')),
    payload         JSONB  NOT NULL,
    dedup_key       TEXT   NOT NULL UNIQUE,
    status          TEXT   NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'posted', 'failed')),
    attempts        INT    NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    posted_at       TIMESTAMPTZ,
    platform_ref_id BIGINT,
    trace_context   TEXT
);
```

(`platform_ref_id` was `github_id` originally — renamed in `0024`; `trace_context` is from `0031`;
the `pr_open`/`check_run_*` kinds and their constraint widening are from `0029` and `0035`.)

Every producer-side column choice is explained by one goal: **the reconciler must never join to
post.** `owner`/`repo`/`installation_id` are carried flat on the row rather than resolved from
`task_id` at drain time — `db/outbox.rs:1-13`'s module doc states it directly: "Producers write
intent — fully shaped — in the same transaction as the domain change ... all the shaping happens at
produce time: the PR-diff fetch ... and the inline/deferred/out-of-scope validation + body rendering
... run in the producer and are baked into the `payload`." The reconciler, in other words, is kept
a **dumb poster**: it never parses a diff, it just ships bytes that were already fully rendered by
whoever enqueued the row.

### Enqueue: idempotent on `dedup_key`

From `services/control-plane/src/db/outbox.rs:48-85`:

```rust
pub async fn enqueue_outbox_post(
    pool: &PgPool,
    platform: Platform,
    task_id: Option<Uuid>,
    installation_id: i64,
    owner: &str,
    repo: &str,
    kind: &str,
    payload: &Value,
    dedup_key: &str,
) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO outbox (platform, task_id, installation_id, owner, repo, kind, payload, dedup_key, \
         trace_context) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, (SELECT trace_context FROM tasks WHERE id = $2)) \
         ON CONFLICT (dedup_key) DO NOTHING",
    )
    .bind(platform).bind(task_id).bind(installation_id).bind(owner).bind(repo)
    .bind(kind).bind(payload).bind(dedup_key)
    .execute(pool)
    .await?
    .rows_affected()
        > 0;
    if inserted {
        let _ = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(OUTBOX_CHANNEL)
            .bind(dedup_key)
            .execute(pool)
            .await;
    }
    Ok(inserted)
}
```

`ON CONFLICT (dedup_key) DO NOTHING` means a re-finalize or a webhook redelivery never double-enqueues
— `dedup_key` is a stable string per logical post (`enqueue_review` in `src/outbox.rs:71-91` keys it
`<task_id>:review`; `enqueue_reaction` keys it `<task_id>:reaction:<content>` so the 👀/😕 lifecycle
reactions don't collide with each other). `pg_notify(OUTBOX_CHANNEL, ...)` only fires when a row was
genuinely inserted, waking the reconciler's `LISTEN` immediately rather than making it wait for its
timer fallback.

### The reconciler's claim query — the same lock pattern, with a caveat

From `services/control-plane/src/db/outbox.rs:96-109`:

```rust
pub async fn claim_outbox_batch(pool: &PgPool, limit: i64) -> Result<Vec<OutboxRow>, sqlx::Error> {
    sqlx::query_as::<_, OutboxRow>(
        "SELECT id, task_id, installation_id, owner, repo, kind, payload, attempts, platform, \
                trace_context \
         FROM outbox \
         WHERE status = 'pending' AND next_attempt_at <= now() \
         ORDER BY created_at, id \
         LIMIT $1 \
         FOR UPDATE SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}
```

Same `FOR UPDATE SKIP LOCKED` idiom as `claim_next_task` in §2.2 — but read the doc comment
immediately above it in the source carefully, because it states something the tasks-queue version
doesn't have to worry about: "`FOR UPDATE SKIP LOCKED` only holds the row lock for the duration of
*this* statement (no enclosing transaction), so it does **not** protect the claim→post→mark gap —
two replicas could each claim a distinct disjoint set, then both deliver. Correctness here rests on
the **single-replica** invariant (ADR-0058/0059), not the lock." This is exactly why the reconciler,
unlike the dispatcher, is specified as a hard singleton rather than "any number of replicas, the lock
sorts it out" — the lock here only prevents a *self*-collision (an overlapping claim within the same
process), not a cross-replica one.

`ORDER BY created_at, id` — ascending, not `priority DESC` like the task queue — because outbox rows
don't carry a priority; ordering here exists purely to preserve **per-task posting order** (post the
review before the verdict reaction that references it), and `id` breaks `created_at` ties for rows
enqueued inside the same producing transaction, where Postgres's `now()` is transaction-stable and
therefore identical across every row that transaction inserts.

### Retry backoff and dead-lettering

From `services/control-plane/src/db/outbox.rs:129-144`:

```rust
pub async fn mark_outbox_failed(pool: &PgPool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE outbox SET \
             attempts = attempts + 1, \
             last_error = $2, \
             status = CASE WHEN attempts + 1 >= $3 THEN 'failed' ELSE 'pending' END, \
             next_attempt_at = now() + make_interval(mins => (attempts + 1) * (attempts + 1)) \
         WHERE id = $1",
    )
    .bind(id).bind(error).bind(OUTBOX_MAX_ATTEMPTS)
    .execute(pool)
    .await
    .map(|_| ())
}
```

Quadratic backoff (`attempts² minutes`) up to `OUTBOX_MAX_ATTEMPTS = 6` (`outbox.rs:22`), after which
the row is parked `failed` for post-mortem inspection rather than retried forever — "a courtesy post
isn't worth retrying forever."

## 2.4 Webhook dedup — `webhook_deliveries`

GitHub's `X-GitHub-Delivery` header is the natural idempotency key for inbound webhook events. The
table (`0001_init.sql:15-22`, renamed by `0024_platform_abstraction.sql:29-30`):

```sql
CREATE TABLE IF NOT EXISTS webhook_deliveries (
    delivery_id     TEXT PRIMARY KEY,
    event_name      TEXT        NOT NULL,
    installation_id BIGINT,
    repository_id   BIGINT,
    received_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload_json    JSONB       NOT NULL,
    platform        TEXT        NOT NULL DEFAULT 'github'
);
```

`docs/components-and-data-models.md:124` names the mechanism precisely: "The PK + `ON CONFLICT` gives
exactly-once delivery handling with no in-process dedup set" — the dedup is structural (a primary-key
collision), not a cache that could be lost on restart or diverge across `serve` replicas.

This table has a second life beyond webhook dedup: `reserve_mcp_run_slot`
(`services/control-plane/src/db/tasks.rs:827-872`) reuses it as the MCP role's per-identity rate-limit
ledger, inserting synthetic `event_name = 'mcp.review'` rows and counting them in a window. That's
why the payload-compaction sweep (`services/control-plane/src/db/webhook_deliveries.rs:29-32`)
explicitly excludes `event_name = 'mcp.review'` rows from ever having their payload blanked to `{}` —
compacting one mid-window would silently shrink its own caller's quota count.

## 2.5 The ADR-0055 index-readiness gate — `waiting_for_index`

One more piece of queue mechanics worth calling out by name, because it's the reason `TaskStatus` has
a `WaitingForIndex` variant at all: a review/ask task that lands while an `index` task is already
in flight for the same repo starts in `waiting_for_index` instead of `queued`, so the dispatcher
never claims it against a half-written snapshot. The SQL fragment that computes this at insert time
(`services/control-plane/src/db/tasks.rs:134-137`):

```rust
const INITIAL_TASK_STATUS_SQL: &str = "CASE WHEN $7 <> 'index' AND EXISTS ( \
        SELECT 1 FROM tasks ix WHERE ix.repository_id = $2 AND ix.command_text = 'index' \
          AND ix.status IN ('queued', 'running', 'posting_result', 'waiting_for_index')) \
     THEN 'waiting_for_index' ELSE 'queued' END";
```

When the blocking `index` task later reaches *any* terminal status,
`release_reviews_waiting_on_index` (`tasks.rs:724-769`) flips every task the repo has parked back to
`queued` and fires one `pg_notify` to wake the dispatcher — this runs on success *and* failure/
cancellation "so a failed index never strands reviews forever."

## 2.6 Supporting tables, briefly

**`reviews`** (`0009_reviews.sql`) — one row per task, the durable copy of what got posted:

```sql
CREATE TABLE IF NOT EXISTS reviews (
    task_id            uuid PRIMARY KEY REFERENCES tasks (id) ON DELETE CASCADE,
    summary            text        NOT NULL,
    body               text        NOT NULL,
    inline_count       int         NOT NULL DEFAULT 0,
    deferred_count     int         NOT NULL DEFAULT 0,
    out_of_scope_count int         NOT NULL DEFAULT 0,
    findings           jsonb       NOT NULL DEFAULT '[]'::jsonb,
    created_at         timestamptz NOT NULL DEFAULT now()
);
```

It exists so "the admin console can show what the agent said for a run (and as an AI-governance
audit record)" — GitHub/GitLab only holds the posted copy; this is the control plane's own. `findings`
is the agent's full findings array (file/line/severity/title/body) verbatim, later joined against
rejected reactions for feedback memory.

**`review_comments`** and **`review_feedback`** (`0015_review_feedback.sql`) implement ADR-0035's
feedback-polling loop — GitHub does not webhook reactions, so the reconciler's feedback poll (its
other half, alongside the outbox drain) periodically reads the reactions REST API for comments the
bot itself created and reconciles the result into these tables:

```sql
CREATE TABLE IF NOT EXISTS review_comments (
    id                UUID        PRIMARY KEY,
    task_id           UUID        NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    github_comment_id BIGINT      NOT NULL,
    kind              TEXT        NOT NULL CHECK (kind IN ('inline', 'reply')),
    file              TEXT,
    line              INT,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (kind, github_comment_id)
);

CREATE TABLE IF NOT EXISTS review_feedback (
    id                UUID        PRIMARY KEY,
    task_id           UUID        NOT NULL REFERENCES tasks (id) ON DELETE CASCADE,
    github_comment_id BIGINT      NOT NULL,
    comment_kind      TEXT        NOT NULL,
    reactor           TEXT        NOT NULL,
    reaction          TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (github_comment_id, comment_kind, reactor, reaction)
);
```

(`github_comment_id` on both was renamed `platform_comment_id` by `0024`.) A reaction that
disappears — the reactor un-reacted — is *deleted* on the next reconcile pass, which is how a removal
is observed without GitHub ever webhooking it. ADR-0044's feedback memory (M1) is built on top:
`rejected_findings_for_repo` (`services/control-plane/src/db/feedback.rs`) joins `review_feedback`
(`reaction = '-1'`) → `review_comments (file, line)` → the matching entry in `reviews.findings`, to
recover the *title* of each previously-rejected finding and feed it back into future review prompts
as "previously rejected here — don't repeat."

**`pending_review_actions`** (`0012_pending_review_actions.sql`) is the ADR-0037 mediated-write
buffer: the agent's `add_review_comment` / `add_comment` / `set_summary` mediated tool calls land
here *during* a run, and the control plane flushes the whole buffer once, on clean completion, as one
grouped review — "so a mid-run crash posts nothing." Three `action` kinds, each with its own
dedup rule: `inline` findings are deduped by `(task_id, file, line)` (last write wins — not a content
hash, because a non-deterministic LLM re-run rewords the same finding differently each time); the
`summary` is single-valued per task; plain `comment`s are append-only and consolidated into one reply
at flush.

**`a2a_tasks`** (`0025_a2a_tasks.sql`) maps a server-generated A2A `taskId` onto the underlying
`tasks` row that actually does the work, and carries its own **optimistic-concurrency `version`
column** — the migration's comment explains why this is a genuinely separate layer from `run_epoch`:
the A2A SDK's `TaskStore::update(task)` is a blind upsert with no expected-version check, so under
horizontal `a2a` scaling, two replicas could each read-mutate-write and silently lose an update; every
update here is `... WHERE version = <expected>`, so a lost update fails loudly instead of clobbering.

**`a2a_task_events`** (`0026_a2a_task_events.sql`) is the append-only per-A2A-task streaming log
behind ADR-0077: an ordered, gap-free `(a2a_task_id, seq)` sequence of status/artifact updates,
appended inside the *same transaction* that flips the underlying task's status
(`set_task_status`, §2.2's caller) — "so an event exists for every transition a poller could observe
— streaming and polling can never disagree."

**`durable_step`** (`0028_durable_step.sql`) is the ADR-0087 crash-recovery journal for the agent
loop's opt-in `CheckpointRuntime`: keyed on `(task_id, run_epoch, step_name)`, it lets a pod death
requeue the same `run_epoch` and replay already-completed steps from storage instead of re-paying
them. It's explicitly scoped as *execution state*, not domain data — self-purging on success, with
the `replay` role (file `01`) as its TTL backstop for runs that never reached success.

## 2.7 What to read next

- `03-neo4j-structural-graph.md` — the other store the agent never gets direct credentials to; same
  trust-boundary reasoning as the outbox's single-writer rule, applied to a different kind of write.
- `04-pgvector-and-dual-retrieval.md` — `code_chunks`, and how its own snapshot concept
  (`latest_indexed_commit`) rhymes with this chapter's `(repository_id, commit_sha)` scoping idiom.
- `05-glossary.md` — lease, epoch, outbox, dedup key, and every other term this chapter introduced.
