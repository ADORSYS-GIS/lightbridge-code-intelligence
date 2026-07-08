# ADR-0061: SAST (opengrep) as a deterministic finding source in the review pipeline

- **Status:** Superseded by [ADR-0073](0073-sast-as-agent-tool.md)
- **Date:** 2026-06-27
- **Deciders:** @stephane-segning

## Context and Problem Statement

The review pipeline today produces findings one way: the native LLM agent investigates the PR diff and
buffers mediated `add_review_comment` actions, which the control plane validates against the diff and
flushes as one grouped review through the egress outbox ([ADR-0037](0037-agent-acts-via-mediated-tools.md),
[ADR-0059](0059-reconciler-owns-all-github-egress.md)). That signal is *probabilistic* — a re-run can
surface a different finding, miss one it found last time, or word it differently.

Static application security testing (SAST) is the complement: a rules engine (we use
[opengrep](https://github.com/opengrep/opengrep), the LGPL fork of Semgrep CE) finds known-bad patterns
**deterministically** — same code, same rules, same findings, every run, at CPU cost with no tokens.
We want that signal in the review. Two design questions had to be answered:

1. **Where does it run, and how does it reach the PR?**
2. **Should SAST output be fed to the LLM (which triages/curates it) or stand on its own?**

A tempting off-the-shelf answer is opengrep + [reviewdog](https://github.com/reviewdog/reviewdog)
posting directly to the PR. We reject that for the **product pipeline**: reviewdog is a *poster*, and
[ADR-0056](0056-control-plane-owns-the-posted-output.md) made the control plane the **single policy
owner** of everything that reaches a PR. A second posting channel would reintroduce exactly the
duplicate-channel class ADR-0056 removed. (reviewdog remains a fine choice for *this repo's own CI* —
a separate surface, out of scope here.)

## Decision

**Run opengrep in the agent-runner as a deterministic finding source whose findings flow through the
existing review channel — never a second poster — and make the LLM *aware* of those findings without
ever *gating* them.**

1. **opengrep runs in the runner**, the same subprocess pattern as Graphify
   (`services/agent-runner/src/indexer/graph.rs`): spawned over the checkout, **best-effort and
   non-fatal** (a missing binary, scan error, or timeout logs and continues — it never fails a review).
2. **Scoped to the PR's changed files.** opengrep is pointed only at the paths in the PR diff
   (`clone::pr_diff().files`), not the whole repo, so a review surfaces findings *on the change* rather
   than dumping every pre-existing repo finding into the out-of-scope section.
3. **Findings ride the existing buffer.** Each SARIF result is mapped to a `Finding` and buffered via
   the existing `POST /internal/tasks/{id}/review/inline` endpoint — the same buffer the agent writes
   to. The control plane's `finalize_review` then validates them against the diff, renders them, and
   enqueues them as part of the **one** grouped review (ADR-0059 outbox). **No new posting code, no new
   GitHub egress, the single-channel invariant (ADR-0056) preserved.**
4. **Deterministic, not LLM-gated.** SAST findings are posted on their own merit. The LLM does **not**
   decide whether a SAST finding is shown — laundering a reproducible signal through a stochastic filter
   would forfeit the very property that makes SAST worth having, and cost tokens to do it.
5. **LLM-aware (Phase 2).** A compact digest of the SAST findings *is* injected into the agent prompt,
   so the agent (a) doesn't redundantly re-report a line opengrep already flagged and (b) may choose to
   *deepen* a SAST lead (trace a tainted input, confirm exploitability). This is awareness, not a gate.
6. **Labeling.** SAST findings are visually distinguished as opengrep's (a `🔍 opengrep` marker in the
   title, `category = security` so they carry the red security badge, and the rule's help URL under
   Resources) rather than masqueraded as the agent's own.
7. **Opt-in, hermetic rules.** opengrep + a **pinned, vendored ruleset** are baked into the runner
   image; scans do not fetch rules from a registry at runtime (reproducibility + no runtime network
   dependency + no supply-chain surprise). The feature is gated behind a `review.sast` config block,
   default **off**, so the rollout is image-then-config and a deploy without the new image is unaffected.

## Consequences

- **Good:** the review gains a deterministic, token-free security pass that reuses 100% of the existing
  validation / diff-scoping / rendering / egress machinery. No trust-boundary change (opengrep stays in
  the untrusted runner, which still holds no GitHub key). reviewdog is deliberately *not* a dependency.
- **False positives** are handled by the deterministic levers — rule curation, diff-scoping, and
  `nosemgrep`/`opengrep-ignore` suppression comments — plus the per-repo rejected-findings memory
  (M1, [ADR-0044](0044-feedback-memory-m1.md)), **not** by LLM suppression.
- **Limit — v1 couples SAST to the review buffer (deliberately scoped).** SAST findings are buffered
  into the same `pending_review_actions` buffer as the agent's, which has three documented edges:
  - **Collision on an identical `(file, line)`:** last-write-wins (the buffer upserts on that key).
    SAST is buffered before the agent, so the agent's richer version wins a true collision; the Phase 2
    digest is what keeps collisions rare in the first place.
  - **Aborted run:** an `abort` clears the whole inline buffer (so an incomplete run posts only its
    honest note), which drops the SAST findings too; they reappear on the next run.
  - **Transport failure:** a run that never finalizes posts nothing, SAST included.
  Giving SAST an **independent flush** (so deterministic findings survive an LLM abort/failure) and a
  structured **`source` column** on `pending_review_actions`/`Finding` (for programmatic
  source-filtering and analytics, instead of the title marker) are noted follow-ups — neither is needed
  for the value v1 delivers.

## References

- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — mediated write actions + buffer/flush-on-finalize
  (the channel SAST findings ride).
- [ADR-0056](0056-control-plane-owns-the-posted-output.md) — the control plane is the single policy
  owner of PR output (why reviewdog is rejected for the product pipeline).
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) — the egress outbox (downstream of where SAST
  hooks in; untouched).
- [ADR-0019](0019-graphify-cli-structural-graph.md) — the Graphify subprocess pattern opengrep mirrors.
- [opengrep](https://github.com/opengrep/opengrep) · [reviewdog](https://github.com/reviewdog/reviewdog).
</content>
</invoke>
