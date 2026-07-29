# ADR-0038: Per-repository review model, selected in the admin UI from an operator allowlist

- **Status:** Accepted — repo-scope tier only. The identity/ACL scope expansion below is superseded by
  [ADR-0110](0110-identity-scoped-model-selection-and-acl.md) (repo/org scope; per-user tier still open).
- **Date:** 2026-06-22
- **Deciders:** @stephane-segning

## Context and Problem Statement

The review model is a single global setting (`LLM_MODEL`, from the ai-helm chart / env) that the
control plane injects into every agent Job — every repository is reviewed by the same model. But
repositories differ: a security-sensitive service may warrant the strongest (most expensive) model, a
high-traffic low-risk repo a cheaper/faster one, and some languages or domains are served better by a
particular model. There is no way to choose.

Should an **admin** be able to pick the review model **per repository**, from the web console?

## Decision Drivers

- **Per-repo cost/quality control:** match model spend to a repo's risk and value.
- **Right owner:** model choice has cost implications, so it belongs to the **admin** (who already owns
  the approval gate, epic #75), not the PR author — this distinguishes it from
  [ADR-0030](0030-repo-review-config.md)'s in-repo, author-owned config.
- **Safe selection:** the UI must not let someone pick a model the gateway doesn't serve (a typo
  breaks every review) — choose from an **allowlist**, not free text.
- **Reuse the existing path:** the control plane already injects `LLM_MODEL` into each Job
  ([ADR-0017](0017-agent-runner-control-plane-bootstrap.md)); per-repo selection should just override
  that value.
- **Backward compatible:** a repo with no choice set uses the global default.
- **In scope** ([ADR-0029](0029-focused-review-not-generic-runner.md)): selecting *which model reviews*
  is configuring understanding, not arbitrary execution.

## Considered Options

- **Option A — Global model only** (status quo).
- **Option B — Per-repo model in the repo file** (`.lightbridge-code-review.jsonc`,
  [ADR-0030](0030-repo-review-config.md)).
- **Option C — Per-repo model in the admin UI**, stored in the control-plane DB, chosen from an
  operator-defined allowlist.

## Decision Outcome

Chosen option: **Option C.** An admin picks, per repository, the review model from a **dropdown** in
that repo's settings in the web console (the governance surface, epic #75 Milestone C). The choice is
stored on the repository (control-plane DB). When the dispatcher builds a review Job, it overrides
`LLM_MODEL` with the repo's chosen model, falling back to the global default when none is set.

- **Allowlist, not free text.** The operator configures the set of models the gateway actually serves
  (chart config), which the control plane exposes via an API endpoint (such as GET /admin/models); the UI offers exactly those, plus "Default". This keeps a misconfiguration from
  ever reaching a Job and gives the UI a clean dropdown.
- **Admin-gated.** Setting the model requires a dedicated permission (such as repo:write or reusing repo:approve under ADR-0023); it is **not** exposed in the author-owned repo file
  ([ADR-0030](0030-repo-review-config.md)) — a PR author must not be able to select the most expensive
  model on the org's budget.
- **Just the model first.** Generation params (temperature/top_p/max_tokens) and a per-run/per-kind
  override are natural extensions of the same mechanism, deferred until asked for.

### Consequences

- **Good:** per-repo cost/quality control owned by the right party (admin); safe by construction
  (allowlist); reuses the `LLM_MODEL` injection unchanged downstream; fully backward compatible
  (unset → global default). The agent prompt is already operator config ([ADR-0037](0037-agent-acts-via-mediated-tools.md));
  this extends "review behaviour is operational config" to the model.
- **Bad / accepted trade-off:** a new per-repo settings field + migration, a settings UI surface, and
  an operator allowlist to maintain — the allowlist must track what the gateway serves, or a stale
  entry offers a model that 404s (mitigated: the runner already surfaces an LLM misconfig as a failed
  task, and the allowlist lives in operator config next to the gateway wiring).
- **Neutral / to watch:** this is distinct from [ADR-0030](0030-repo-review-config.md) (author-owned,
  in-repo) and [ADR-0031](0031-review-skills-commands.md) (named skills) — three different config
  planes (operator allowlist, admin per-repo, author in-repo) that the UI/docs must keep legible.

## Pros and Cons of the Options

### Option A — global model only
- Good: simplest; one place to set the model.
- Bad: no per-repo control; every repo pays for one model's cost/quality point.

### Option B — per-repo model in the repo file
- Good: lives with the repo; no UI needed; versioned.
- Bad: **wrong owner** — a PR author could select the most expensive model on the org's budget; cost
  is an admin decision. Also couples to the ADR-0030 file-read path and offers no validation against
  what the gateway serves.

### Option C — admin UI + DB + operator allowlist (chosen)
- Good: right owner (admin), safe (allowlist), reuses `LLM_MODEL` injection, back-compat.
- Bad: DB field + UI + allowlist maintenance.

## More Information

- Reuses the Job env injection ([ADR-0017](0017-agent-runner-control-plane-bootstrap.md),
  `LLM_MODEL`) and the OpenAI-compatible model contract ([ADR-0018](0018-openai-compatible-embeddings.md)).
- Admin surface + gating: epic #75 (admin console / approval), [ADR-0023](0023-db-backed-rbac.md)
  (permission-based authz).
- Sibling config planes: [ADR-0030](0030-repo-review-config.md) (author, in-repo),
  [ADR-0031](0031-review-skills-commands.md) (skills), [ADR-0037](0037-agent-acts-via-mediated-tools.md)
  (prompt as operator config).
- Source of truth: this ADR + the review-agent epic (#137).

## Amendment (2026-06-28) — scope expands to per-identity models + ACL

Per-**repo** model selection (this ADR as written) is too narrow. The real requirement is **per-identity
model resolution across org / user / repo**: e.g. **Stephane and Mark reviewing on the same repo should get
different models**, each per their own configuration. That introduces **ACL** — who may set a model at which
scope (org admin vs repo maintainer vs individual), and how the scopes resolve/override (precedence order).

This ADR does not cover identity-scoped selection or the authz model, so it will be **superseded by a
follow-up ADR** that designs: (a) the config store keyed on (org, user, repo); (b) the resolution/precedence
order; (c) the ACL governing who sets what at which scope; (d) how the resolved model reaches the runner's
per-task config (the control plane already owns that hand-off). Tracked in epic #241.

**Follow-up ADR written 2026-07-29:** [ADR-0110](0110-identity-scoped-model-selection-and-acl.md)
answers (a)-(d) above for **repo and org** scope. It deliberately does not resolve the per-**user** tier
this amendment's own headline example asked for (two people reviewing the same repo getting different
models) — see ADR-0110's Decision Outcome for why that's cut to a further follow-up rather than silently
built. Tracked in epic #491, story #501.
