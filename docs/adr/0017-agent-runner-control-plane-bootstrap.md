# ADR-0017: Agent runner bootstraps from the control plane (no App key in the Job)

- **Status:** Accepted
- **Date:** 2026-06-19

## Context and Problem Statement

The dispatcher launches one Kubernetes Job per task (ADR-0004); that Job runs the **agent runner**,
which must clone the target repository, index it, run the OpenCode agent, and surface findings. To
clone a private repo and (eventually) post results it needs a GitHub credential. The question is
where that credential lives and how the runner — an isolated, per-task pod that may process
untrusted fork content — obtains its work and reports back.

The control plane is the system's trust boundary (ADR-0002): it already holds the GitHub App private
key and mints short-lived installation tokens (`github.rs`). We do not want to spread the App key
into every ephemeral Job.

## Decision Drivers

- Keep the GitHub App private key in exactly one place (the control plane).
- Per-task, short-lived credentials (a fork's Job should never hold a long-lived or broadly-scoped
  secret).
- A runner that holds no standing authority: it asks for what it needs, does the work, reports a
  result; the control plane decides what actually gets written to GitHub.
- A contract both sides can test without a cluster.

## Considered Options

- **Runner bootstraps from the control plane (chosen).** The Job carries only its task id and the
  control-plane callback wiring. The runner calls an internal API to fetch its task context plus a
  freshly-minted installation token, then reports status transitions back. The App key never leaves
  the control plane.
- **Give each Job the GitHub App key (or a broad token).** Simplest wiring, but spreads the most
  sensitive secret into every untrusted-content pod and widens the blast radius of a compromised
  runner. Rejected.
- **Mount a per-task Kubernetes Secret with a pre-minted token.** Avoids a callback, but couples the
  dispatcher to token lifetime, leaves the token at rest in the namespace for the Job's lifetime,
  and still needs a channel for the runner to report results. Rejected for v1; revisit if the
  callback becomes a bottleneck.

## Decision Outcome

Chosen option: **the runner bootstraps from the control plane over an internal API.**

### The contract

Two routes on the control plane, mounted under `/internal/tasks/{id}` and authenticated with a
shared bearer (`AGENT_RUNNER_TOKEN`) — **not** the OIDC path used by the dashboard, because the
caller is a pod, not a user:

- `GET /internal/tasks/{id}` → repo coordinates (`owner`, `name`, `clone_url`, `default_branch`),
  a freshly-minted installation `token`, and the task parameters (`command`, `target_*`,
  `base_sha`, `head_sha`).
- `POST /internal/tasks/{id}/status` → a status transition (`running` | `succeeded` | `failed` | …).
  Terminal states stamp `completed_at` and clear the dispatcher lease.

The dispatcher injects `CONTROL_PLANE_URL` and `AGENT_RUNNER_TOKEN` into each Job env (alongside the
task id) so the runner can reach back. Absent `AGENT_RUNNER_TOKEN` in the control-plane process, the
internal routes fail closed (503) rather than serving unauthenticated callers.

### Consequences

- Good, because the GitHub App key stays solely in the control plane; the runner holds only a
  short-lived, installation-scoped token for the duration of one task.
- Good, because the control plane keeps decision authority: the runner proposes findings; validation
  and GitHub write-back remain on the trusted side (a later slice of epic #5).
- Good, because the contract is testable on both sides without a cluster (wiremock on the runner,
  `#[sqlx::test]` on the control plane).
- Bad, because the shared bearer is a symmetric secret distributed to every Job. It is injected as a
  literal env value in the Job spec today; hardening (per-task tokens, or a `secretKeyRef` to a
  managed Secret, or mTLS/SA-token auth) is a follow-up and noted in the runner ticket.
- Neutral, because pre-minted per-task Secrets remain a possible optimization if the callback round
  trip ever matters.

**⚠️ Amended 2026-07-14 ([ADR-0092](0092-per-task-runner-tokens.md)):** the "Bad" line above is
resolved — `AGENT_RUNNER_TOKEN` is still the Job env var name, but its value is now a fresh,
short-lived, per-task signed token minted at dispatch, not one long-lived value shared across every
Job. The bootstrap contract itself (this ADR) is unchanged; only the bearer's scope is.
