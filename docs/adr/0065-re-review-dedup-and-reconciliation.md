# ADR-0065: Re-review must not duplicate — dedup on unchanged commits, reconcile by re-deriving

- **Status:** Accepted
- **Date:** 2026-06-28 (amended 2026-07-02)
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0040](0040-re-review-reads-prior-findings.md) feeds the agent its **prior review** on a re-run so it
**reconciles** instead of contradicting itself across runs (the original failure: two runs found different
P1s and the second flatly contradicted the first). That fixed contradiction — but a re-review on an
**unchanged commit** now produces a **carbon copy**.

Live evidence (webank-mobile#112): an `@mention` re-review ran on the **same `head_sha` (`f4c8411`)** as the
prior review 33h earlier and re-posted the **same 5 inline findings on the same 5 files**, titles differing
only in trivial wording. The verdict was honest (*"re-verified both P1s — both hold"*) but the PR was left
with **10 inline comments — 2× each identical finding**. Zero new information, pure noise, and a full
(expensive) agent run spent to reproduce a result.

How should a re-review behave when the code hasn't meaningfully changed?

## Decision Drivers

- **No duplicate comments.** Re-running must never leave two identical inline findings on a PR.
- **Keep the anti-contradiction win** of [ADR-0040](0040-re-review-reads-prior-findings.md).
- **Don't anchor.** *"Reconcile, don't contradict"* biases toward **restating** — it can re-emit a prior
  **false positive** unchecked (seen: *"persists from prior review"* on vymalo-shop#303–305) and suppress
  genuinely-new angles.
- **Cost.** A re-review of unchanged code shouldn't pay for a full retrieval+multi-turn run.
- The control plane already owns the posted output and persists the prior review's comments + findings
  ([ADR-0056](0056-control-plane-owns-the-posted-output.md)), so dedup has the data it needs.

## Considered Options

- **Option A — Unchanged-`head_sha` short-circuit.** If an `@mention` review targets the same `head_sha` as
  the last posted review (no new commits), **don't run the full agent**: post a terse verdict — *"no new
  commits since my last review (<link>); the N findings still stand"* — with an explicit keyword to force a
  full re-run.
- **Option B — Dedup at finalize.** Always run, but before posting, **drop inline findings byte-identical
  (normalized `(file, line, body)`) to the prior review's** and post only new/changed findings + a
  reconciliation note. Handles the "new commits, partial overlap" case too.
- **Option C — Re-derive-then-reconcile prompt.** Change the [ADR-0040](0040-re-review-reads-prior-findings.md)
  injection from *"reconcile, don't contradict"* to *"re-derive independently first, then reconcile — and
  **retract** any prior finding that no longer holds."* Orthogonal to A/B; addresses anchoring.
- **Option D — Status quo.** Full re-review every time (the duplicate above).

## Decision Outcome

> **Superseded by the [2026-07-02 amendment](#amendment--2026-07-02-accepted-as-implemented) below:**
> Option A was **dropped** and all-priors summarization added. The paragraph below is the original
> Proposed outcome, kept for the record.

Chosen: **A + B + C together.** **A** avoids the wasted run and the duplicate in the common case (re-review,
no new commits). **B** is the correctness backstop for when a run *does* happen (new commits with overlapping
findings) — the control plane dedups inline comments against the prior review so a finding is never posted
twice. **C** keeps [ADR-0040](0040-re-review-reads-prior-findings.md)'s anti-contradiction value while
breaking the echo: the agent re-derives, then reconciles, and actively **retracts** stale findings. This
ADR **refines ADR-0040**, it does not revert it — prior reviews are still injected; their *output* is now
constrained.

## Amendment — 2026-07-02 (Accepted, as implemented)

On review, the owner **revised** the decision. **Option A (unchanged-`head_sha` short-circuit) is
DROPPED.** The premise — "a re-review of unchanged code has nothing new to say" — is wrong for an agent
whose output is **non-deterministic**: repeated invocations on the same PR/commit legitimately surface
*different* real findings (that non-determinism is exactly what ADR-0040 documented). Short-circuiting
would suppress a genuinely-useful second look to save a run. **The reviewer must always run fully** and
be free to produce a different review; correctness is enforced at the *output*, not by skipping the run.
No force-re-run keyword is needed — there is nothing to force past.

What ships instead:

- **B (finalize dedup) — kept.** Before posting, drop any finding whose **normalized key** — `file`,
  `line`, and a **whitespace-collapsed + case-folded title** — matches one already posted on this PR by a
  prior Lightbridge review. The source of truth is our **persisted `reviews.findings`** (ADR-0022/0035),
  not the GitHub API. Dedup is scoped to priors on the **same `head_sha`**: `(file, line, title)` is only
  a safe identity *within one commit* — line numbers drift across commits, so a cross-commit match would
  be unsound and could drop a distinct finding. The count dropped is logged (`deduped_n`).

- **C (re-derive-then-reconcile) — kept and strengthened.** ADR-0040's *"reconcile, don't contradict"*
  **anchors** the model: a prior **false positive** gets *restated* unchecked (the poisoning observed on
  vymalo-shop#303–305 and webank-mobile#112). The injected block is reframed: prior findings are
  **UNVERIFIED HYPOTHESES** from an earlier automated pass, possibly wrong. The model must **re-derive its
  review from the diff first**, then reconcile — **explicitly retracting** any prior finding it cannot
  re-derive, and never inheriting one without re-verifying it against the code.

- **New: summarize ALL prior reviews, not just the latest.** ADR-0040 injected only the single most-recent
  review; the "Future" note there anticipated carrying more. We now fetch **every** prior review of the
  target (excluding the current task) and build the context block **deterministically in Rust** (no LLM
  call): the **latest** review keeps detail (verdict + findings, `PRIOR_FINDINGS_CAP = 30`), **older**
  reviews are compressed to **one line each** (chronological ordinal, one-line verdict, finding count +
  titles only). The whole block is capped (`PRIOR_BLOCK_CHAR_CAP`) with an **explicit truncation marker**
  rather than a silent cut, in the same idiom as the finding cap.

This refines — does not revert — ADR-0040: prior reviews are still injected; their framing (untrusted
hypotheses, re-derive-first) and their *output* (deduped at finalize) are now constrained.

### Consequences

- **Good** — a re-review on an unchanged commit posts a one-line verdict + a link, not 5 duplicate comments.
- **Good** — overlapping findings across commits are deduped, not stacked.
- **Good** — the agent can drop a prior false positive (C), which "reconcile, don't contradict" discouraged.
- **Good** — saves a full agent run when nothing changed (A).
- **Bad / watch** — "meaningfully changed" is `head_sha`-based; a force-rerun keyword is needed for "re-review
  anyway" (e.g. after a model/prompt change). The dedup key (B) must normalize whitespace/wording so trivial
  re-phrasings still match.
- **Neutral** — needs the prior review's `head_sha` + its posted comments, both already persisted.

## Pros and Cons of the Options

### A — unchanged-head short-circuit
- Good — cheapest; kills the common-case duplicate before any cost.
- Bad — needs a force keyword; doesn't help the new-commit-partial-overlap case (that's B).

### B — finalize dedup
- Good — always correct; covers partial overlap across commits.
- Bad — still pays for the run; dedup key must be robust to re-phrasing.

### C — re-derive-then-reconcile prompt
- Good — removes anchoring; lets stale findings be retracted.
- Bad — prompt-only, hard to guarantee; complements but doesn't replace A/B.

### D — status quo
- Bad — the duplicate this ADR exists to remove.

## More Information

- Refines: [ADR-0040](0040-re-review-reads-prior-findings.md) (prior-review injection).
- Lives in: [ADR-0056](0056-control-plane-owns-the-posted-output.md) (the control plane owns the post; dedup goes here).
- Evidence: ADORSYS-GIS/webank-mobile#112 (two identical reviews on `f4c8411`).
- Related: [ADR-0043](0043-review-finding-verification.md) (the refute pass C builds on for retraction).
