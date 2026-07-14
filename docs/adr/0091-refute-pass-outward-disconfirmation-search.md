# ADR-0091: The refute pass searches outward for disconfirming evidence on absence claims

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0043](0043-review-finding-verification.md)'s refute pass bounces a `finish` that carries an
outstanding P0/P1 finding and instructs the model to "re-verify each one against the exact evidence you
cited." That instruction is structurally self-confirming: re-reading the lines a finding already cites
can only ever reproduce the same read, never contradict it.

Issue [#304](https://github.com/vymalo/lightbridge-code-intelligence/issues/304) documents the live
failure. `ADORSYS-GIS/webank-mobile#145` (run `e82f7c4b-50ec-4bc4-942f-48cfc404b603`) posted a P1
blocker — *"Cancel request rejected without Idempotency-Key … every cancel attempt will be rejected with
400"* — anchored at `bff/cmd/server/main.go:580`. It was false: the mobile client's Dio interceptor
(`mobile/lib/core/network/api_client.dart:74`) injects the header on every mutation. The agent's evidence
chain stopped at the call site (`pending_p2p_repository.dart:30`, correctly noted "sends no header") and
never opened `api_client.dart` — unchanged, not in the diff, not in `engaged_files`. The refute pass then
re-read exactly the two files the finding already cited and rubber-stamped its own gap, 94 seconds later
reporting all findings "confirmed."

This is the same failure family as [ADR-0069](0069-review-tier-minimum-model-capability.md)'s honor-system
gate: a bounce mechanism that lets the model satisfy its letter (re-verify, then finish) without doing
the thing it exists to force (find evidence that could change the verdict).

## Decision Drivers

- **Kill this specific false-positive shape** — "X is never sent/set/present" claims, where the
  disconfirming file is structurally one dependency-hop away (an interceptor, middleware, base-options
  builder, or config default), not in the call site the finding cites.
- **Cost-bounded** ([ADR-0069](0069-review-tier-minimum-model-capability.md)'s ceiling): no new turn
  budget, still one-shot, still P0/P1-only.
- **Scoped, not a general "read more" mandate**: the ticket's explicit Out of Scope is "broad 'read
  every dependency' expansion." This only changes the refute pass's own one-shot nudge.
- **No DB/schema change** — this is prompt/policy content, not new mediated-tool surface.

## Decision

`RefuteGate` (`services/review-agent/src/policies/refute.rs`) now tracks each outstanding P0/P1
finding's own text (title/body/evidence), not just a count, plus the set of files the run has already
engaged (read, or already cited by a recorded finding) — mirroring `CoverageGate`'s `engaged` set
([ADR-0041](0041-full-diff-coverage-gate.md)).

1. **Absence-claim detection.** A finding is flagged if its title/body/evidence matches a broad
   substring list ("never sent", "not set", "doesn't include", "missing the", …). False negatives just
   fall back to the pre-existing generic re-verify nudge — over-matching is cheap, under-matching only
   loses the extra directive.
2. **A second, additive nudge clause for absence claims only.** When the one-shot bounce fires and at
   least one outstanding finding is an absence claim, the nudge names the finding(s), names the files
   already engaged (so the model doesn't spend the bounce re-reading them), and directs the search at
   the transport/interceptor/middleware/config-default layer, explicitly in files outside that engaged
   set — using the same tools already available (`read_file`, `lightbridge_graph_find_symbol`,
   `lightbridge_graph_get_callers`, `lightbridge_vector_semantic_search`).
3. **Non-absence findings are unaffected** — they keep the original "look at the real code, not your
   memory" re-verify instruction only, per the ticket's Out of Scope.
4. **Still one-shot, still P0/P1-gated.** No change to the bounce cap, the turn budget, or which
   findings trigger the pass at all — the change is confined to what the nudge says once it fires.

## Consequences

- **Good:** the nudge for this bug class now points at a search that can actually falsify the claim,
  instead of a search that structurally cannot. The already-engaged file list gives the model a concrete
  reason not to waste its one bounce turn re-reading what it already has.
- **Good:** additive and scoped — no behavior change for the majority of findings that aren't absence
  claims, and no new turn/cost surface (verified: `flows_run_review_matches_all_six_frozen_traces`
  golden test, which carries no P0/P1-bounce trace, is unaffected).
- **Bad / accepted:** the directive is still model-driven, not a hard guarantee — same limitation ADR-0043
  already accepted for the base refute pass. A determined-wrong or capability-floor model
  ([ADR-0069](0069-review-tier-minimum-model-capability.md)) can still satisfy the letter of the search
  and re-affirm; this raises the bar, it does not remove the honor-system property.
- **Neutral:** the absence-marker list is a substring heuristic, not a classifier — it will need upkeep
  as new false-positive shapes surface. Tracked informally; revisit if the marker list drifts stale.

## What this deliberately defers

- **Deterministic/tool-enforced disconfirmation** (e.g. a mediated `search_for_disconfirming_evidence`
  tool that fails a P0/P1 unless the model actually queries outside `engaged_files`) — a stronger
  guarantee than a nudge, but a new tool surface; out of scope per the ticket.
- **Generalizing beyond absence claims** to other confidently-wrong shapes (e.g. mis-stated invariants,
  incorrect type claims) — explicitly out of scope for this ticket; a candidate follow-on if the same
  failure family recurs outside "X is never sent/set."

## References

- Issue [#304](https://github.com/vymalo/lightbridge-code-intelligence/issues/304); epic
  [#252](https://github.com/vymalo/lightbridge-code-intelligence/issues/252) (review quality &
  reliability).
- [ADR-0043](0043-review-finding-verification.md) — the refute pass this refines.
- [ADR-0041](0041-full-diff-coverage-gate.md) — the `engaged` file-tracking pattern reused here.
- [ADR-0069](0069-review-tier-minimum-model-capability.md) — the same honor-system-gate failure family,
  and the bounce-cost ceiling this change stays within.
