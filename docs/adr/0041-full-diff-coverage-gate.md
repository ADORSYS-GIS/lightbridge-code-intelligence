# ADR-0041: A full-diff coverage gate before wind-down

- **Status:** Accepted (bounce semantics refined by [ADR-0069](0069-review-tier-minimum-model-capability.md): bounce-once → capped re-bounces + coverage disclosure, after a weak model gamed the one-shot bounce)
- **Date:** 2026-06-23
- **Deciders:** @stephane-segning

## Context and Problem Statement

The native review agent ([ADR-0037](0037-agent-acts-via-mediated-tools.md)) receives the whole PR diff
in its prompt, yet in practice a run tends to find **one** issue and call `finish`. Observed live on
`vymalo/vymalo-shop#241`: two runs on the same PR each surfaced a *different* real P1 in the *same
file* (a connection leak in one run, a non-numeric-`exp` bug in the other) — the union only emerged
across runs. A single run does not reliably cover the whole change.

The wind-down convergence ([#173](https://github.com/adorsys-gis/lightbridge-code-intelligence/pull/173))
deliberately solves the *opposite* failure — a run that never stops — by dropping the investigation
tools near the budget tail so the model must converge to `finish`. So any fix for under-coverage must
not push the model to keep investigating in the wind-down tail, or it re-opens the rabbit hole #173
exists to close.

## Decision Drivers

- **One run should account for the whole change**, across all dimensions (correctness, security,
  quality, style, performance) — not stop at the first finding.
- **Must not fight #173.** The convergence tail is sacred; the gate may only act *before* wind-down.
- **Bounded cost.** A nudge can cost at most a small, fixed number of extra turns — never an open loop.
- **No false sense of a hard guarantee.** "No finding" is a *valid* outcome for a changed file, so we
  cannot require a finding per file. The lever is forcing the model to *account for* every changed file,
  not to comment on each.

## Decision

Track which changed files the agent has **engaged** — opened with `read_file` or recorded a finding on
(`add_review_comment`). The first time the model calls `finish` **before the wind-down boundary** with
changed files it has neither opened nor commented on, **bounce it once**: inject a one-shot message
listing the un-engaged files and asking it to review each across all dimensions before finishing, then
let the loop continue. The bounce is one-shot (`coverage_bounced`), so the next `finish` always goes
through — the gate costs at most one extra turn.

Properties:

- **Pre-wind-down only** (`turn < winddown_turn`). After the boundary, #173's convergence wins and the
  gate is inert — it can never reopen investigation in the tail.
- **Bounce-once.** Worst case is a single extra round-trip even on a large PR.
- **Soft, not mechanical.** The gate accounts for coverage; it does not assert a finding per file. A
  file the model genuinely reviewed and found clean gets one reflective nudge, then `finish` proceeds.
- **No-diff / no-change runs are unaffected** — an empty changed-file set never bounces.

The companion [`TOOL_PROTOCOL`] stays minimal (it is factual, coupled to the tool API); the *richness*
of the review — covering all dimensions, not being terse — is steered by the operator prompt
(ai-helm `config.reviewSystemPrompt`, per ADR-0037), which is tuned alongside this change (D, #137).

## Consequences

- **Good:** a single run is pushed to account for the entire diff; combined with the operator-prompt
  emphasis it produces broader, less one-note reviews; the cost is bounded and the #173 tail is
  untouched.
- **Cost:** at most one extra chat round-trip on runs that try to finish early with un-engaged files;
  on a fully-covered run there is no bounce (verified by test — covered → 2 round-trips, un-engaged →
  3).
- **Limitation:** engagement is proxied by `read_file` / `add_review_comment` targets. A file reviewed
  purely from the in-prompt diff (never opened, found clean) reads as un-engaged and draws one nudge —
  acceptable, since the nudge is bounded and harmless, and the alternative (parsing "did the model
  reason about file X") is unobservable. This is a heuristic that improves coverage, not a proof of it.
- **Pairs with [ADR-0040](0040-re-review-reads-prior-findings.md):** B widens a *single* run's coverage;
  A keeps *successive* runs coherent. Together they target the "one different finding per run" pattern
  from both ends.

## References

- Epic [#137](https://github.com/adorsys-gis/lightbridge-code-intelligence/issues/137).
- [#173](https://github.com/adorsys-gis/lightbridge-code-intelligence/pull/173) — wind-down convergence
  (the constraint this gate must not violate).
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — agent loop, tool surface, prompt ownership.
- [ADR-0040](0040-re-review-reads-prior-findings.md) — the companion consistency fix.
