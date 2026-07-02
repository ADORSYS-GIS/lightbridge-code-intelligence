# ADR-0068: Reaction-driven review lifecycle — 👀 start, 👍 clean+silent, 👎 findings, 😕 failure

- **Status:** Accepted
- **Date:** 2026-07-02
- **Deciders:** @stephane-segning

## Context and Problem Statement

Today's GitHub feedback for a review is a mix of reactions and comments that don't map cleanly to what
actually happened:

- **👀 fires at webhook receipt** — [`react_seen`](../../services/control-plane/src/http/webhook.rs)
  enqueues 👀 the instant the webhook lands, before any work is scheduled. On an approved-but-busy repo
  (or one parked behind an in-flight index, [ADR-0055](0055-review-waits-for-index-readiness.md)) the 👀
  means "we received the event", not "we started reviewing" — a promise the queue hasn't kept yet.
- **🎉 on every posted review** — the reconciler ([ADR-0059](0059-reconciler-owns-all-github-egress.md))
  reacts 🎉 "hooray" unconditionally when it posts a review, whether the review found problems or not, so
  the reaction carries no verdict.
- **Reactions always land on the PR body**, even when the trigger was an `@mention` **comment**. A human
  who asked for a re-review on a thread gets the acknowledgment on the PR description, away from their
  request.
- **A clean pass still posts a comment.** ADR-0056's "never silent" guarantee forces a review post even
  when there are zero findings — [`finalize_review`](../../services/control-plane/src/http/internal.rs)
  falls back to `DEFAULT_CLEAN_SUMMARY` ("No issues found — the change looks good.") — so a clean PR gets a
  low-signal "looks good" review that the author has to read and dismiss.

The owner wants the reactions to *be* the status, driven by real lifecycle transitions, with a clean pass
that says nothing but 👍.

## Decision

**Make the GitHub feedback reaction-driven, keyed to real lifecycle transitions, on the trigger.**

| Reaction | Meaning | When | Target |
| --- | --- | --- | --- |
| 👀 `eyes` | seen **and work started** | the dispatcher launches the agent Job (queued→running-and-dispatched) | the trigger |
| 👍 `+1` | reviewed, **zero findings** — clean | finalize, run `finished` cleanly, no inline/deferred/out-of-scope findings | the trigger |
| 👎 `-1` | reviewed, **findings posted** | finalize, ≥1 finding | the trigger |
| 😕 `confused` | terminal failure | runner-reported `failed`/`timed_out` (or reaper on uncatchable kill) | the trigger |

- **👀 moves to *work started*.** No reaction at webhook receipt; the dispatcher enqueues 👀 the moment it
  launches the Job (the queued→running transition is the claim in
  [`claim_next_task`](../../services/control-plane/src/db.rs); the Job launch is the observable "work
  started"). It rides the egress outbox like every other reaction ([ADR-0059](0059-reconciler-owns-all-github-egress.md)),
  best-effort — a missing 👀 never fails a dispatch.
- **The trigger is the reaction target.** An automatic `pull_request opened` review reacts on the PR body
  (issue-level reaction, unchanged). An `@mention` reacts on the **triggering comment**. The comment id
  is persisted on the task (`tasks.trigger_comment_id`, nullable) and threaded through the outbox
  `reaction` payload (optional `comment_id`); the reconciler posts to
  `POST /repos/{owner}/{repo}/issues/comments/{comment_id}/reactions` when present, else the issue
  endpoint.
- **👍/👎 replace 🎉.** On completion, the verdict reaction is 👍 when the review found zero findings and
  👎 when findings were posted. The unconditional 🎉 in the reconciler's `deliver_review` is removed; the
  verdict reaction is enqueued at finalize, where the finding counts are known.
  - **❌ → 👎 mapping.** The owner asked for ❌ on findings, but GitHub's reaction set is fixed to eight
    contents (`+1`, `-1`, `laugh`, `confused`, `heart`, `hooray`, `rocket`, `eyes`) — there is **no ❌**.
    👎 (`-1`) is the agreed stand-in for "changes requested".
  - **One verdict per task.** Both verdicts share a single outbox dedup key
    (`<task>:reaction:verdict`, the content only in the payload), so a re-finalize that computes a
    *different* verdict (crash-then-requeue, or a stray retry against the cleared buffer) can never stack
    👍 **and** 👎 on the trigger — the first verdict wins.
- **A clean pass is silent — but only a *provably* clean one.** The runner reports how the run ended in
  the finalize request body (`outcome: "finished" | "exhausted" | "aborted"`), and the post is suppressed
  (👍 as the whole response) **only** for an explicit `finished` with zero inline/deferred/out-of-scope
  findings while reactions are enabled. Everything else **fails open to posting**:
  - **`aborted`** — the runner cleared its unverified findings and buffered an honest *"Couldn't complete
    this review: …"* note; that note **posts**, with 😕 and no verdict reaction (an apology is not a
    verdict). Without the outcome gate, an aborted run's zero findings would masquerade as a clean 👍.
  - **`exhausted` with zero findings** — the *"hit its step budget"* note (deep) / quick-pass banner
    (fast) **posts**, with no verdict reaction: "clean so far" from an incomplete pass must not read as
    "clean". Exhausted *with* findings posts them and reacts 👎 as usual.
  - **Missing/unknown outcome** (e.g. an older runner mid-rolling-deploy) — posts, no verdict reaction.
  - **Reactions disabled** (`review.reactions = false`) — nothing is suppressed: without the compensating
    👍 the clean pass would be *totally* silent, so the clean review posts exactly as before this ADR.
    (The toggle defaults to **on** when unset or when no config file is mounted.)

  This deliberately **supersedes ADR-0056's "never silent" for the explicitly-clean case**: the reaction
  is the acknowledgment, so the author isn't made to read a "looks good" comment. Applies to **both**
  tiers (fast auto + deep `@mention`). The review row is **still persisted** in `reviews` (verdict +
  summary + zero counts) — it feeds prior-review context
  ([ADR-0040](0040-re-review-reads-prior-findings.md)) and observability; only the GitHub post is
  suppressed. Normally the reconciler persists the row off the `review` intent; with no intent, finalize
  persists it directly — **insert-if-absent**, never clobbering a reconciler-written posted row (that
  would null `github_review_id` and break the [ADR-0035](0035-review-feedback-signal.md) feedback join).
  The silent branch is further **guarded for re-finalize**: if a `review` intent (any status) or a posted
  review already exists for the task, it no-ops instead of 👍-ing over the real review. On the silent
  path, persisting the row and enqueueing the 👍 are **fatal** (500 → the runner retries finalize,
  idempotently), because nothing else records the run there.
- **Scope: PR review tasks post-suppression; 👀 also covers mention-triggered issue tasks.** Non-review
  tasks — issue analysis, epics, direct `@mention` questions on a non-PR target — keep posting their
  reply; their reply *is* the deliverable. A mention-triggered **issue** task still gets its 👀
  work-started acknowledgment on the triggering comment (receipt-time 👀 was removed globally; issue asks
  must not lose their only acknowledgment). The failure path is unchanged: a terminal failure still posts
  the ADR-0056 failure notice + 😕, from serve on a runner-reported failure **and from the reaper on an
  uncatchable kill** — both retargeted to the trigger comment when the task was mention-triggered.

## Consequences

- **Good — reactions are honest status.** 👀 now means the review is actually running, not merely queued;
  👍/👎 carry the verdict; the acknowledgment sits on the human's request, not the PR description.
- **Good — no low-signal clean-pass comment.** A clean PR gets 👍 and nothing else, which is the whole
  point. Prior-review context and the console still see the verdict because the `reviews` row is persisted
  regardless of the post.
- **Tradeoff — a clean `@mention` question gets only 👍.** An `@mention` that embeds a *question* but
  yields zero findings will get **only 👍**, no textual answer — the silent-clean rule can't tell "clean
  review" from "clean review that also had a question". Accepted by the owner. (A pure question on a PR
  with *no* review verdict at all still posts its reply — the suppression is scoped to review tasks that
  produced a verdict.)
- **Tradeoff — supersedes ADR-0056's never-silent guarantee** for the clean case only. The failure path
  (notice + 😕) is untouched, so a *failed* review is never silent; only a *successful clean* one is.
- **Migration.** One additive, nullable column (`0022_task_trigger_comment.sql`); no backfill. Older tasks
  have `trigger_comment_id = NULL` and react on the PR/issue body exactly as before. The outbox `reaction`
  payload gains an optional `comment_id`; rows without it route to the issue endpoint, so in-flight rows
  drain unchanged during rollout.

## References

- [ADR-0056](0056-control-plane-owns-the-posted-output.md) — the "never silent" guarantee this supersedes
  for the clean case (the failure notice it introduced is untouched).
- [ADR-0059](0059-reconciler-owns-all-github-egress.md) — the egress outbox every reaction rides; this ADR
  moves 👀 to work-started, adds the comment-targeted reaction, and drops the unconditional 🎉.
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) — the fast/deep tiers; silent-clean applies
  to both.
- [ADR-0040](0040-re-review-reads-prior-findings.md) — prior-review context, which is why the clean review
  row is still persisted even when the post is suppressed.
- [ADR-0035](0035-review-feedback-signal.md) — the feedback join on `github_review_id`, which is why the
  clean-path persist is insert-if-absent (never clobber a posted row).
- GitHub reactions API — the fixed eight contents (no ❌), and the separate issue-comment reactions
  endpoint.
