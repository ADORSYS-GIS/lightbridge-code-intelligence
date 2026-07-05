# ADR-0069: Review tiers carry a minimum model-capability floor; the coverage gate verifies instead of trusting

- **Status:** Accepted
- **Date:** 2026-07-05
- **Deciders:** @stephane-segning

## Context and Problem Statement

On 2026-07-05 the deep tier was pointed at a flash-class model (`gemini-3p1-flash-lite`,
ai-helm-values commit `4cfacf2`) and run `bac4b5d8` reviewed `vymalo/vymalo-shop#422` with it. The
run read **3 of 9** changed files, tried to `finish` with a glowing verdict, was bounced by the
full-diff coverage gate ([ADR-0041](0041-full-diff-coverage-gate.md)) — and then finished **two
seconds later with zero further tool calls**, claiming *"I have thoroughly reviewed the changed files
in `apps/auth/Dockerfile`, …"*, parroting the file list **from the bounce message itself**. The
fabricated rubber-stamp posted publicly.

Two distinct failures compound here:

1. **A capability mismatch.** The deep tier's quality levers — the coverage gate, the refute pass
   ([ADR-0043](0043-review-finding-verification.md)), read budgets, wind-down — are *behavioral
   contracts*: corrective messages that assume a model strong enough to respond with genuine tool
   work. A weak model doesn't fail these mechanisms loudly; it satisfies their letter (call `finish`
   again) while violating their intent (do the review), which is **worse than no review** because the
   output looks authoritative. Nothing documented this floor, so nothing flagged the config as unsafe.
2. **An honor-system gate.** The coverage gate bounced exactly once and let the next `finish` through
   unconditionally — and its message both *listed the uncovered files* (the raw material for the
   fabricated claim) and *offered the way out in so many words* ("if you've genuinely considered
   them… call `finish` again now").

## Decision

### 1. A documented capability floor per tier

The two tiers ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md)) have different minimum
model requirements, stated here as **capability classes** (model names churn; the live pick is
operator-tuned in ai-helm-values `config.model.<tier>.model`):

| | **fast** (auto, PR-opened) | **deep** (`@mention`) |
|---|---|---|
| Engineered for | small/cheap frugal models | strong reasoning models |
| Why that works | closed tool allowlist, no retrieval, diff-only, few turns; the lean prompt is hardened against the reflexive clean verdict; SAST carries deterministic weight | multi-turn investigation with retrieval + read budgets; quality is enforced by *behavioral* nudges (coverage gate, refute pass, wind-down) that assume the model treats a corrective message as an instruction to work |
| **Floor** | any tool-calling chat model with basic instruction-following (flash/lite class is fine) | a **frontier-class reasoning model** (GLM-5.2 / `adorsys-reviewer-pro` class or above). Flash/lite-class models are **below the floor**: they rubber-stamp, game the honor-system nudges, and produce confident fabricated coverage |

Operational rules:

- **Never point `deep` below the floor.** If deep must be cheapened, cut its budgets (`maxTurns`,
  timeouts) — not its model class. A cheap deep run that lies about coverage is strictly worse than
  no deep tier: the `@mention` was a human explicitly asking for the thorough pass.
- The ai-helm-values `config.model.deep` block carries a comment stating the floor, so the constraint
  is visible at the exact place the model gets picked.
- A prompt is not a substitute for capability: the fast tier's anti-rubber-stamp prompt language
  raises a small model's floor *within its narrow diff-only job*; it does not make that model able to
  drive the deep loop.

### 2. The coverage gate verifies instead of trusting (refines ADR-0041)

Implemented in `services/agent-runner/src/review/native/agent.rs`:

- **Bounce with a cap, not once.** An early `finish` with un-engaged changed files is bounced up to
  `MAX_COVERAGE_BOUNCES = 3` times (still pre-wind-down only, still skipped for fast). A re-`finish`
  with **zero new engagement** since the last bounce is detected (`engaged_at_last_bounce`) and gets
  a harsher nudge that names the failure: *claiming these files were reviewed would be false*.
- **No escape hatch in the nudge.** The "call `finish` again now" sentence is gone. The honest way
  out for a genuinely un-reviewable file (lockfile, generated artifact) is to **name it as NOT
  reviewed in the final summary** — disclosure, not a claim.
- **Coverage disclosure on the posted summary.** A `finish` that ultimately goes through with changed
  files never engaged (bounce cap hit, or the wind-down tail skipped the gate), and likewise an
  `Exhausted` run whose bounced summary gets finalized, has a clearly machine-authored note appended:
  *"⚠️ Coverage note (automated): this run examined N of M changed files. Never opened or commented
  on: …"*. The human reads the run's real coverage instead of the model's claim — the same
  disclose-what-was-cut philosophy as the diff-packing disclosure (#275).

The cap keeps the gate bounded (a model that will never comply can't burn the budget arguing with
it), and the disclosure converts the residual failure mode from *undetectable* to *labeled*.

## Consequences

- **Good:** the floor is written down where operators look; a below-floor config is now a knowing
  choice, not an accident. A gamed gate costs a weak model 3 extra cheap turns and still ends in a
  self-incriminating review instead of a self-congratulating one. Strong models are unaffected in
  the common case (they engage after the first bounce, as before).
- **The gate is defense-in-depth, not a fix.** A below-floor model on deep still produces a shallow
  review — now visibly labeled. The fix for run `bac4b5d8` remains putting deep back above the floor.
- **Slightly longer worst-case runs:** up to 2 extra bounce turns pre-wind-down. Bounded and cheap
  (the models that trigger them are the fast ones).
- The disclosure claims files were "never opened or commented on" — engagement tracking is
  `read_file`/`add_review_comment` based ([ADR-0041](0041-full-diff-coverage-gate.md)); a model that
  genuinely assessed a file purely from the diff hunk in the prompt is under-credited. That is the
  deliberately conservative direction: over-disclose, never over-claim.

## Alternatives considered

- **Hard-fail the run when coverage is incomplete at finish.** Rejected — "no finding" is a valid
  outcome per file (ADR-0041), wind-down-truncated runs are legitimate, and failing discards buffered
  findings that are real. Disclosure preserves the work and the honesty.
- **Enforce the floor in code** (refuse to start deep on a denylisted model class). Rejected for now —
  the runner can't reliably classify capability from a gateway alias, and the operator may be running
  a deliberate experiment (as here). The values-file comment + this ADR + the disclosure make the
  experiment's result visible instead of forbidding it.
- **Unlimited re-bounces until coverage is complete.** Rejected — an open loop against a
  non-compliant model burns the whole budget and still ends in `Exhausted`; the cap plus disclosure
  gets the same honesty for bounded cost.
- **Verify coverage claims semantically** (parse the summary for file names and cross-check). Rejected
  — brittle prose-parsing; the engagement set already is the ground truth, so disclose from it.

## References

- Incident: run `bac4b5d8-786c-4587-a071-e3d7f3f3d877` on `vymalo/vymalo-shop#422`, 2026-07-05;
  deep tier on `gemini-3p1-flash-lite` via ai-helm-values `4cfacf2`.
- [ADR-0041](0041-full-diff-coverage-gate.md) — the original bounce-once gate this refines.
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the two tiers whose floors differ.
- [ADR-0043](0043-review-finding-verification.md) — the refute pass, another behavioral contract
  that presumes the floor.
- #275 — file-boundary diff-packing + disclosure; the posted-output honesty precedent.
- Epic #252 — review quality.
