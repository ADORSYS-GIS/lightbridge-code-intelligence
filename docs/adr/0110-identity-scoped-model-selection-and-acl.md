# ADR-0110: Repo/org-scoped model selection + ACL (ADR-0038's promised follow-up, scoped down)

- **Status:** Accepted — repo/org scope only; per-user resolution explicitly deferred (see Decision
  Outcome's first scope cut).
- **Date:** 2026-07-29
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0038](0038-per-repo-review-model.md) shipped per-repo model selection, then its 2026-06-28
amendment flagged that per-repo alone is too narrow — the real need is per-**identity** resolution
across org/user/repo (its own example: two people reviewing the same repo should be able to get
different models), with an ACL governing who may set what at which scope. That amendment promised "a
follow-up ADR" designing the config store, precedence order, ACL model, and runner hand-off. It was
never written; epic #491's story #501 is that follow-up.

This codebase has no identity concept above `repo` today. `repositories.installation_id` is a plain
`BIGINT` column (the GitHub installation ID / GitLab project ID) — it is not a first-class org entity
with its own table, and there is no `users` table or per-actor identity resolution anywhere. Building
the full (org, user, repo) precedence chain ADR-0038's amendment describes would first require
resolving a webhook's acting human (a GitHub/GitLab login) to a stable internal user identity — a
prerequisite neither ADR-0038 nor story #501 named as its own dependency.

## Decision Drivers

- **Ship something real now, without inventing a user-identity system as a silent prerequisite.** A
  design that quietly requires "first, build user accounts" is not actually answering what story #501
  asked for on the timeline it asked for it.
- **Reuse the resolution pattern already proven for presets** ([ADR-0103](0103-repo-configurable-opencode-review-presets.md)'s
  entry-point resolver, `services/control-plane/src/preset.rs`) rather than inventing a new
  fallback-chain shape.
- **Allowlist, not free text**, still holds ([ADR-0038](0038-per-repo-review-model.md)'s original
  concern) — a typo'd model name must not be reachable at write time.
- **Reuse the existing flat RBAC claim model** ([ADR-0023](0023-db-backed-rbac.md)) rather than building
  a second, parallel authorization system just for this.
- **Say plainly what's cut, not silently ship a smaller thing than what was asked for.**

## Considered Options

- **Option A — Build the full (org, user, repo) chain now**, including a new user-identity table
  resolved from webhook actor logins.
- **Option B — Repo scope only** (ADR-0038 as originally shipped, unchanged).
- **Option C — Repo + org scope, admin-gated, no per-user tier.** Per-user resolution named as an
  explicit, separate follow-on once a real user-identity concept exists.

## Decision Outcome

Chosen option: **Option C**, with two scope cuts from ADR-0038's amendment's literal ask — both called
out here for the record rather than silently decided:

**Scope cut 1 — no per-user tier.** This ADR resolves `repo` and `org` scope only. Per-individual-user
model selection (the amendment's own headline example — two people on the same repo getting different
models) is explicitly **out of scope**, deferred until a real user-identity table exists to resolve a
webhook actor's login against. Building that identity-resolution layer as a hidden prerequisite here
would silently balloon this ADR past what it can actually decide about model/ACL config; it is its own
future ADR.

**Scope cut 2 — a single flat `model:configure` permission, no delegated per-scope grants.** Any caller
holding `model:configure` may set *any* repo's or org's override — there is no "org admin can only touch
their own org" delegation model in this ADR. Building scope-qualified permission delegation is its own
ACL system; starting from the flat-claim model ADR-0023 already has keeps this ADR's actual surface
small enough to review and ship.

**The rest of the design:**

- **New tables** (`services/control-plane/migrations/`):
  ```sql
  CREATE TABLE repo_model_overrides (
      repository_id BIGINT PRIMARY KEY REFERENCES repositories(id),
      model TEXT NOT NULL,
      set_by TEXT NOT NULL,
      updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
  );
  CREATE TABLE org_model_overrides (
      installation_id BIGINT PRIMARY KEY,
      model TEXT NOT NULL,
      set_by TEXT NOT NULL,
      updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
  );
  ```
  `installation_id` is reused as the org key — it's already the closest thing to an org identity this
  schema has (one GitHub App installation is scoped to one org/user account), so no new org-entity table
  is introduced; a row keyed by an installation ID that happens to cover only one repo is still a valid,
  if trivial, "org" override.
- **Precedence**: `repo_model_overrides` (if a row exists for the task's repo) → `org_model_overrides`
  (if a row exists for that repo's `installation_id`) → the existing global `LLM_MODEL` default. This is
  the same three-tier "specific override → scoped default → global default" shape
  [`preset.rs`](0103-repo-configurable-opencode-review-presets.md) already resolves presets with —
  deliberately reused, not reinvented.
- **Allowlist validation at write time**: a `GET /admin/models` endpoint (ADR-0038 assumed this would
  exist; it doesn't yet — building it is part of story #501, not deferred further) backed by an
  operator-curated list (chart/env config, mirroring how the review-preset tool allowlist is
  operator-owned). Both new write endpoints validate against it before the row is written; naming the
  allowlist in the error on rejection.
- **ACL**: a new permission `model:configure` ([ADR-0023](0023-db-backed-rbac.md)'s claim model),
  required by both new write endpoints, in addition to the repo already being approved for a repo-scope
  write.
- **Runner hand-off**: control plane resolves the override at task-creation time (same call sites
  `preset.rs`'s `resolve_preset_or_default` is already threaded through — `webhook.rs`,
  `a2a/handler/lifecycle.rs`), stores it on the new nullable `tasks.model_override` column.
  `agent-runner`'s `ReviewConfig` resolution applies it as the **final** step, after `for_preset`
  resolves the preset's complete base config — an override changes *which model* runs, never a preset's
  tools/gates/budgets/prompt. This keeps [ADR-0103](0103-repo-configurable-opencode-review-presets.md)'s
  "every preset renders through the identical structural path" guarantee intact; a model override is a
  value substitution on top of that structure, not a new axis of structural variance between presets.

### Consequences

- **Good:** ships a real, usable repo/org model override with a validated allowlist and an honest
  precedence chain, reusing proven resolution/RBAC patterns rather than inventing new ones; unblocks
  story #501 without silently requiring a user-identity system first.
- **Bad / accepted trade-off:** does not deliver the amendment's headline per-user example (two reviewers
  on one repo, two different models) — that remains unsolved until a follow-up ADR adds user identity.
  Any caller with `model:configure` has global reach across every org/repo — a real gap if this console
  is ever opened to more than a small trusted admin set; tightening it is its own follow-up, not silently
  built here.
- **Neutral / to watch:** this is now a fourth config plane alongside ADR-0030 (author, in-repo),
  ADR-0038/this ADR (admin, DB, repo/org-scoped), and ADR-0103 (preset resolution, a parallel but
  independent axis) — docs and the admin UI must keep these legible as separate, not conflate "preset"
  and "model override" as the same setting.

## Pros and Cons of the Options

### Option A — full (org, user, repo) chain now
- Good: actually delivers the amendment's stated need.
- Bad: requires inventing user-identity resolution from webhook actors as an unstated prerequisite —
  a materially larger, riskier scope than what story #501 or this ADR can respons­ibly design and ship
  in one pass; the identity-resolution question (which login counts as "the" user for an auto-triggered
  `pr_open` task with no specific human actor?) doesn't even have an obvious answer yet.

### Option B — repo scope only (status quo, ADR-0038 unchanged)
- Good: nothing new to build.
- Bad: doesn't answer the amendment at all; leaves story #501 entirely unaddressed.

### Option C — repo + org, admin-gated, no per-user (chosen)
- Good: real, shippable progress on the amendment's org/repo half; reuses existing resolution/RBAC
  patterns; the two cuts are each independently addressable later without redesigning this ADR.
- Bad: doesn't fully close the amendment (see Consequences); a single flat permission is coarser than
  ideal ACL granularity.

## More Information

- Precedence-chain pattern reused from:
  [ADR-0103](0103-repo-configurable-opencode-review-presets.md) (`services/control-plane/src/preset.rs`).
- RBAC claim model: [ADR-0023](0023-db-backed-rbac.md).
- Supersedes the identity/ACL portion of [ADR-0038](0038-per-repo-review-model.md)'s amendment; ADR-0038
  itself (the original per-repo-only decision) is unchanged and remains the repo-scope tier's direct
  ancestor.
- Sibling write capability: [ADR-0109](0109-control-plane-forge-write-for-repo-review-config.md) (a
  different admin-write surface, targeting the author-owned repo file rather than control-plane DB
  state — the two should not be confused: this ADR's overrides live in Postgres, never touch the repo's
  own git history).
- Consuming story: #501 (epic #491).
- Explicitly named follow-on: a future ADR adding user identity (webhook-actor → internal user
  resolution) to enable the per-user tier this ADR defers.
