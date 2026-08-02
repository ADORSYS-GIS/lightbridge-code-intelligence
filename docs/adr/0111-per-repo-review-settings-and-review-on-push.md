# ADR-0111: Per-repo review settings store + review-on-new-commits

- **Status:** Accepted
- **Date:** 2026-08-02
- **Deciders:** @stephane-segning

## Context and Problem Statement

Review behaviour was global and hardcoded. Check-run reporting (#558/#559, fixed #563/#564, richer
summary #565) shipped as all-or-nothing across the fleet: every task snapshotted
`check_runs_enabled = true` with no way to turn it off for one repo. The automatic on-PR-open review
always ran, with no opt-out. And a review was never triggered when new commits landed on an already-open
PR — GitHub's `pull_request.synchronize` was filtered out at the webhook layer, GitLab MR `update` and
Bitbucket `pullrequest:updated` were ignored entirely; `push` events only ever fed re-indexing.

Teams want different behaviour per project — some want check runs off, some want every push reviewed,
some want silence until asked. This ADR is Epic #566's design: a per-repo settings store and the
review-on-new-commits trigger it unlocks.

Two hazards shaped the design:

1. **The repo config file has two readers with different strictness.** The control-plane's
   `RepoPresetConfig` (`services/control-plane/src/preset.rs`) tolerates unknown keys; the
   agent-runner's `RepoReviewConfig` (`services/agent-runner/src/review/repo_config.rs`) is
   `deny_unknown_fields`. Adding a file key to only one side makes the runner reject the entire file,
   silently dropping that repo's `conventions`/`instructions`/`severity`.
2. **ADR-0065 dedup was scoped to one `head_sha`.** Every push is a new SHA, so without a change there
   would be zero suppression across commits — an unfixed finding would re-post at drifted line numbers
   on every push.

## Decision Drivers

- **Per-repo, not per-org or global** — "projects are different" (the owner's framing); a repo owner or
  operator should be able to set this without a fleet-wide rollout.
- **Operator escape hatch, no repo PR required** — an admin needs to be able to flip a setting
  instantly, without waiting on the repo owner to merge a config change.
- **Repo owner keeps the versioned, reviewable default** — the file stays the source of truth when no
  operator override exists.
- **Never regress what already shipped** — `check_run_reporting` and `review_on_pr_open` must default
  to today's live behaviour.
- **Never silently multiply spend** — `review_on_push` must default OFF; enabling it fleet-wide by
  default would multiply every existing customer's LLM bill by their push frequency without them asking.
- **Reuse proven patterns, don't invent new ones** — the resolution shape mirrors
  [ADR-0110](0110-identity-scoped-model-selection-and-acl.md)'s model-override precedence chain and
  [ADR-0103](0103-repo-configurable-opencode-review-presets.md)'s preset resolver.

## Considered Options

- **Option A — DB-only overrides**, no repo file involvement. Simple, but takes control away from repo
  owners entirely; every change becomes an operator ticket.
- **Option B — repo file only**, no DB layer. Keeps the repo owner in control, but removes the
  operator's ability to react instantly (e.g. to a cost incident) without a PR to every affected repo.
- **Option C — three-layer resolution: built-in default → repo config file → DB override, operator
  wins.** Chosen.

## Decision Outcome

Chosen option: **Option C.**

### 1. Settings store

One table, nullable typed columns — `NULL` means "not overridden here, fall through to file/default"
(migration `0036_repo_settings.sql`):

```sql
CREATE TABLE repo_settings (
    repository_id BIGINT PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    check_run_reporting   BOOLEAN,
    review_on_pr_open     BOOLEAN,
    review_on_push        BOOLEAN,
    push_strategy         TEXT CHECK (push_strategy IN ('supersede','debounce','every')),
    push_debounce_seconds INTEGER CHECK (push_debounce_seconds BETWEEN 10 AND 900),
    dedup_scope           TEXT CHECK (dedup_scope IN ('pr','commit')),
    set_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Typed columns over K/V or JSONB: Postgres validates the enums and the debounce range at write time,
values decode straight into `Option<bool>`/`Option<String>`, and it matches the `model_overrides`
precedent ([ADR-0110](0110-identity-scoped-model-selection-and-acl.md)). No org-level table — these are
per-project behavioural choices; an org tier would double the precedence matrix for no stated need and
is purely additive later if ever wanted.

`services/control-plane/src/db/repo_settings.rs` mirrors `db/model_overrides.rs`: pure persistence,
runtime `sqlx::query`, no business logic.

### 2. Resolution

`services/control-plane/src/settings.rs`:

```rust
pub enum PushStrategy { Supersede, Debounce, Every }
pub enum DedupScope { Pr, Commit }
pub enum Layer { Default, File, Db }
pub struct Sourced<T> { pub value: T, pub source: Layer }
pub struct ResolvedSettings { /* one Sourced<_> per setting */ }

/// The entire precedence rule. Pure, no I/O.
pub(crate) fn merge_settings(file: Option<&TriggersConfig>, db: Option<&RepoSettingsRow>) -> ResolvedSettings;

/// Mirrors resolve_model_override / resolve_preset_or_default: does its own I/O, tolerates
/// platform: None (the A2A path holds no forge credentials), degrades with tracing::warn! on every
/// error, returns no Result — must never fail task creation.
pub async fn resolve_repo_settings(pool, platform, repo, ref_, repository_id) -> ResolvedSettings;
```

`Sourced<T>` carries provenance so the admin explain endpoint can report *which layer* produced each
value — in a three-layer system that is the difference between debuggable and not. The same type
serves both the hot path (`.check_run_reporting.value`) and the explain endpoint, so the two cannot
drift apart.

**Built-in defaults:** `check_run_reporting = true` (don't regress the shipped feature),
`review_on_pr_open = true` (the product's core behaviour), **`review_on_push = false`** (see Decision
Drivers), `push_strategy = supersede`, `push_debounce = 90s`, `dedup_scope = pr`.

`resolve_preset_and_settings(...)` combines preset and settings resolution behind **one** forge fetch
of the repo config file, replacing the separate `resolve_preset_or_default` + `resolve_model_override`
pair that used to run at each of the six webhook call sites (GitHub PR opened/synchronize, GitLab MR
open/update, Bitbucket created/updated).

An unrecognised enum string (a typo'd `push_strategy`) warns and falls through to the next layer rather
than failing the whole resolution — consistent with every other config-degradation path in this
codebase; a single misconfigured field must never take down a repo's review.

### 3. File schema — nested `triggers`, both readers updated together

```jsonc
"triggers": {
  "check_runs": true,
  "review_on_open": true,
  "review_on_push": false,
  "push_strategy": "supersede",      // supersede | debounce | every
  "push_debounce_seconds": 90,
  "dedup_scope": "pr"                // pr | commit
}
```

Nested under one `triggers` key, so the strict runner struct gains a single new key instead of six, and
any future control-plane-only setting needs no further runner change. `TriggersConfig` itself is
`#[serde(default)]` **without** `deny_unknown_fields`, added to both `RepoPresetConfig` (permissive) and
`RepoReviewConfig` (strict) — a later control-plane-only key inside `triggers` can never brick a repo's
runner config. `push_strategy`/`dedup_scope` are `Option<String>` in the file struct (not typed enums)
so a typo there degrades to a warning inside `merge_settings`, not a failed parse of the whole
`RepoPresetConfig` (which would also cost the repo its *preset*).

### 4. Admin API

- `GET /admin/repositories/{id}/settings` (scope `repo:read`) — per field
  `{"value": …, "source": "default"|"file"|"db"}`.
- `POST /admin/repositories/{id}/settings/override` (scope `repo:configure`) — `Option<Option<T>>` per
  field: absent = leave, explicit `null` = clear, value = set. Validates enums/range before writing —
  a typo is a self-explanatory 400, not a CHECK-constraint 500 surfaced from Postgres.

The `/override` path segment matters: `POST .../preset` writes the repo **file**; this writes the
**DB** row that beats it — two different admin surfaces, deliberately not conflated.

### 5. Check runs become per-repo

`tasks.check_runs_enabled BOOLEAN NOT NULL DEFAULT true` (migration `0037`), **snapshotted at task
creation** rather than re-read at use. The check's start and resolve happen minutes apart on different
outbox deliveries and must agree — a fresh read at resolve time would let an operator's mid-run toggle
flip strand an in-progress check on the PR forever. All four check-run touchpoints
(`dispatcher::start_check_run_signal`, `internal::finalize_review`, `internal::handle_review_failure`,
the reaper) already load task context, so gating cost one line each.

### 6. Review on new commits

The `opened` webhook arm is refactored into a shared `create_review_task` helper called from both the
open and sync arms. GitHub allows `"synchronize"`; GitLab allows MR `"update"` **gated on `oldrev` being
present** (GitLab fires `update` for label/title/description edits too, which carry no `oldrev`);
Bitbucket allows `"pullrequest:updated"` — no reliable per-field "did the head move" signal exists there,
so the task idempotency index is the correctness backstop.

`create_task` (content-idempotent, `run_epoch: 0`) is used, not `create_explicit_task` — the natural key
`(repo, pull_request, "review", head_sha, 0)` makes a redelivery or a Bitbucket metadata-only `updated`
collapse to `Ok(None)` for free: "same head ⇒ no re-review". Documented cost: a force-push back to an
already-reviewed SHA won't re-review (the same head has already produced a row).

New `EntryPoint::PrSync` → preset `"pr_sync"`, defaulting to the platform's cheap tier (`fast`, same as
`pr_open`) rather than `deep` — sync fires once per push, and a PR under active development can push many
times. Bot-author and draft skips apply to the sync path exactly as they do to the open path.

**PR-wide finding suppression.** `posted_findings_for_head` (scoped to one `head_sha`) is replaced by
`posted_findings_for_target` (scoped to the whole PR, across every commit). Two-tier dedup key in
`review.rs`: same head still uses the exact `dedup_key(file, line, title)`; a different head uses the
new `pr_dedup_key(file, title)` with the line dropped, since line numbers drift between commits.
Suppression is bounded by a **multiset of occurrence counts**
(`HashMap<Key, usize>`), not a `HashSet` — at most `count` prior postings suppress `count` current
findings per key, so a genuinely new Nth occurrence of a recurring issue still surfaces. A plain
`HashSet` would silently and permanently swallow any new occurrence of an already-seen `(file, title)`
pair. `dedup_scope: commit` is the kill switch, restoring the pre-epic same-commit-only behaviour.

### 7. Push-storm strategies

Read from the already-resolved `ResolvedSettings` at the point a new push's task is created — no second
fetch:

- **`supersede` (default).** After the new push's task is created, `cancel_superseded_pr_reviews`
  cancels the PR's other automatic (`pr_open`/`pr_sync`) reviews — including one parked
  `waiting_for_index` (ADR-0055), which the pre-existing `cancel_active_tasks_for_pr` (used on PR close)
  deliberately does not touch — while sparing an explicit `@mention` run. Each cancelled task's check
  run resolves to `Cancelled` so it doesn't hang "in progress" on the PR forever.
- **`debounce`.** The claim query already honours `run_after`
  (`WHERE run_after <= now()`), so this needed no new scheduler: `NewTask.run_after_secs` is stamped
  from the repo's configured quiet period before the row is inserted. Required a fix to
  `release_reviews_waiting_on_index`, which previously set `run_after = now()` unconditionally on
  release — clobbering a still-pending debounce delay if an unrelated index task happened to finish
  first; changed to `run_after = GREATEST(run_after, now())`.
- **`every`** — the default path from the slice that first enabled sync review; no extra code.

**Known simplification, not fixed here:** `debounce` delays each new push's own task from its own
creation time; it does not cancel-and-reschedule an earlier still-pending debounced task when a newer
push lands inside the same window. A rapid burst can therefore still produce more than one run under
`debounce`. `supersede` is the strategy that guarantees single-active-review semantics; `debounce` only
spaces runs out. If true reset-timer debounce is wanted, it is a follow-up, not a silent gap in this
ADR.

### Consequences

- **Good:** every setting this epic introduced defaults to today's live behaviour or to the safest
  option (`review_on_push: false`) — merging the epic changes nothing for a repo that configures
  nothing. Operators get an instant kill switch; repo owners keep the versioned default. PR-wide dedup
  is a standalone win even for repos that never enable push review (it also fixes re-review-via-mention
  dedup, which was already scoped to one `head_sha`).
- **Bad / accepted trade-off:** three layers (default/file/DB) is more to reason about than one; the
  admin `GET .../settings` provenance response exists specifically to keep that debuggable.
  `debounce`'s known simplification (above) means it does not fully prevent a push storm by itself.
- **Neutral / to watch:** the ADR-0055 index-readiness gate is repo-wide — a merge to the default branch
  parks every open PR's sync review until the index completes, which now interacts with `debounce`'s
  `run_after` (fixed to not fast-forward) but is otherwise unchanged. A separate, pre-existing gap
  (unrelated to this epic, noticed in passing): the manual "Cancel run" admin action
  (`db::cancel_task_by_id`) still does not resolve a check run on cancellation — only the automatic
  `supersede` path does.

## Pros and Cons of the Options

### Option A — DB-only overrides
- Good: one code path, no repo-file schema to maintain.
- Bad: removes repo-owner self-service entirely; every change is an operator ticket.

### Option B — repo file only
- Good: repo owner stays in full control, versioned and reviewable.
- Bad: no instant operator override — a cost incident or abuse case needs a PR merged to every affected
  repo before it can be contained.

### Option C — three-layer, DB wins (chosen)
- Good: repo owner keeps the default; operator gets an instant, auditable (`set_by`) override; per-field
  precedence (not whole-row) means a repo can override one setting in the file and another via the DB
  without either clobbering the other.
- Bad: most moving parts of the three; mitigated by `Sourced<T>` provenance and the admin explain
  endpoint.

## More Information

- Check-run reporting (the feature this epic makes per-repo): #558/#559, fixed #563 (`details_url: null`
  → GitHub 422), #564 (self-healing fallback), richer summary #565.
- Precedence-chain pattern reused from: [ADR-0110](0110-identity-scoped-model-selection-and-acl.md)
  (`repo_model_overrides`/`org_model_overrides`) and
  [ADR-0103](0103-repo-configurable-opencode-review-presets.md) (`preset.rs`'s entry-point resolver).
- ADR-0065's original same-`head_sha` dedup scope (superseded for the `pr` scope by this ADR;
  `dedup_scope: commit` restores it exactly).
- ADR-0055's index-readiness gate, unchanged but now interacting with `debounce`'s `run_after`.
- Epic: #566. Ten independently-mergeable slices, in order: settings store → file schema → admin API →
  per-repo check runs → `review_on_pr_open` gate → PR-wide dedup → `EntryPoint::PrSync` plumbing →
  sync enabled on all three platforms → `supersede`/`debounce` strategies → this ADR.
