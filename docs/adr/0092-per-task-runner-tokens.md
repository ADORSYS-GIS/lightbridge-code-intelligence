# ADR-0092: Per-task runner tokens (hardening the ADR-0017 bootstrap contract)

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0017](0017-agent-runner-control-plane-bootstrap.md) authenticates the runner↔control-plane
internal API with one shared bearer, `AGENT_RUNNER_TOKEN`: a single value, read once from env at
control-plane startup, injected as a plaintext literal into **every** agent Job's env
(`services/control-plane/src/integrations/k8s.rs`), and compared with a plain constant-time equality
check against that one process-wide secret (`RunnerAuth` in `services/control-plane/src/http/internal.rs`).

That ADR flagged this as an accepted-for-v1 gap: "the shared bearer is a symmetric secret distributed
to every Job… hardening (per-task tokens, or a `secretKeyRef` to a managed Secret, or mTLS/SA-token
auth) is a follow-up." A Job is not otherwise credential-free (`README.md`'s "Secrets a Job holds"
table already says so) — but this secret specifically was flagged as the widest-blast-radius one: it
is long-lived (good until the process restarts or the value is rotated) and identical across every
task, so a single leaked Job env (`kubectl get pod -o yaml`, a logging sidecar, a core dump) yields a
credential valid against the internal API for every task, past and future, indefinitely. Issue
[#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243) tracks closing that gap.

## Decision Drivers

- Match the credential-scoping precedent already established for the GitHub token
  (`GithubApp::installation_token`, `services/control-plane/src/integrations/github.rs`): minted
  on-demand, expires on its own, never a standing secret.
- No new standing secret at rest: a per-task Kubernetes `Secret` per Job (one option ADR-0017 already
  considered and rejected for v1) still leaves a credential sitting in the namespace for the Job's
  lifetime and couples the dispatcher to Secret lifecycle.
- Stateless verification: the control plane already re-derives everything else about a task from the
  database, never from the caller (ADR-0002) — the token scheme should follow that shape rather than
  add a new table + round trip to check "is this token still current."
- Must not weaken the existing check: the shared-bearer comparison was already constant-time
  (`subtle::ConstantTimeEq`); the replacement must be at least as resistant to forgery/timing attacks.

## Considered Options

- **A self-verifying, per-task signed token (chosen).** Mint an HS256 JWT at Job-dispatch time, scoped
  by a `tid` (task id) claim and an `exp` no longer than the Job's own `activeDeadlineSeconds`. The
  control plane holds one signing key (`RUNNER_TOKEN_SIGNING_KEY`, never injected into a Job) and
  verifies both the signature and the `tid` claim against the request's own `{id}` path parameter — no
  new table, no DB round trip, and HMAC verification (via `jsonwebtoken`, already a workspace
  dependency) is constant-time internally, same property as the old `ConstantTimeEq` check.
- **A per-task Kubernetes `Secret`, pre-minted at dispatch.** ADR-0017 already rejected this for v1 —
  it's a standing secret at rest for the Job's lifetime, one more k8s object per task, and still needs
  its own cleanup path. Revisiting it here doesn't remove those costs; the signed-token approach gets
  the same "no shared value" property without them. Rejected again.
- **mTLS or a projected Kubernetes ServiceAccount token.** Stronger in the abstract, but a much larger
  change to the runner↔control-plane transport (cert issuance/rotation, or SA-token audience wiring)
  for a problem the signed-token approach already solves. Deferred — worth revisiting if the internal
  API ever needs to authenticate a caller that isn't a same-cluster Job.

## Decision Outcome

Chosen option: **a self-verifying, per-task signed token**, minted at dispatch and verified against the
request's own task id.

### The contract

- `RunnerTokenSigner` (`services/control-plane/src/runner_token.rs`) is the one piece of code on both
  sides of the trust boundary: `KubeLauncher::launch` (`integrations/k8s.rs`) mints with it, `RunnerAuth`
  (`http/internal.rs`) verifies with it. Both read the same process-local `RUNNER_TOKEN_SIGNING_KEY` —
  it is never injected into a Job; only the tokens it mints are (into the same `AGENT_RUNNER_TOKEN` env
  var name as before, so the runner side is unchanged).
- **Mint** (`RunnerTokenSigner::mint`): one HS256 JWT per Job, claims `{ tid: <task uuid>, exp }`, where
  `exp` is `now + activeDeadlineSeconds + 300s` — the Job's own hard runtime cap plus a grace window so
  an in-flight callback right at the deadline boundary isn't rejected as expired moments before
  Kubernetes would have killed the Job anyway.
- **Verify** (`RunnerAuth::from_request_parts`): every internal route is `/internal/tasks/{id}/...`, so
  the extractor reads that `{id}` before the handler does, checks the presented bearer's signature and
  expiry, and rejects if its `tid` claim doesn't match the route's own `{id}` — a validly-signed token
  minted for a *different* task is a **403** (`ForeignTask`), distinct from a missing/expired/malformed
  token (**401**) and an unconfigured signer (**503**, the same fail-closed degrade as before).

### Local dev

`docs/local-setup.md`'s manual runner invocation no longer has a fixed shared string to set on both
sides. `control-plane mint-runner-token <task-id>` (a small CLI role, `main.rs`) mints one token from
`RUNNER_TOKEN_SIGNING_KEY` and prints it to stdout — useful for the manual local run and for
operators debugging a stuck task in prod.

### Consequences

- **Good:** a leaked Job env now authenticates exactly one task, for at most its own runtime window —
  not every task, indefinitely. This closes the gap ADR-0017 flagged and README's "Secrets a Job holds"
  tracked as a follow-up.
- **Good:** stateless — verification is signature + expiry + a claim comparison, no new table, no DB
  round trip on the hot callback path (`get_context`, `/chunks`, `/graph`, the mediated review-write
  routes all stay as fast as before).
- **Good:** the forgery-resistance property is preserved, not weakened — HMAC verification is
  constant-time (the same guarantee `subtle::ConstantTimeEq` gave the old equality check), and forging
  a token now additionally requires the signing key, not just guessing/replaying one static string.
- **Bad, accepted:** this is a breaking operator-facing config change — `AGENT_RUNNER_TOKEN` is no
  longer read by the control plane as its own secret (only as the Job env var name the *minted* token
  travels in); every deployment must set `RUNNER_TOKEN_SIGNING_KEY` instead. No dual-read/back-compat
  path is provided (a hard cutover, matching how the shared bearer's failure mode was already "closed,
  not open" — running with the old var alone now closes the internal API rather than silently keeping
  it on the weaker scheme).
- **Neutral:** the signing key itself is still one process-wide secret (rotating it invalidates every
  Job's in-flight token, same operational shape as rotating the old `AGENT_RUNNER_TOKEN`) — that's
  inherent to symmetric HMAC and out of scope here; the property this ADR buys is *per-task* scope, not
  key rotation.

## References

- Issue [#243](https://github.com/vymalo/lightbridge-code-intelligence/issues/243).
- [ADR-0017](0017-agent-runner-control-plane-bootstrap.md) — the bootstrap contract this hardens; its
  Consequences section is the direct precursor to this decision.
- [ADR-0002](0002-rust-control-plane-trust-boundary.md) — the trust boundary this token scheme stays
  inside (the control plane still owns every write; the runner still only proposes).
- `README.md`'s "Secrets a Job holds" table — the prior framing of this as a hardening target.
