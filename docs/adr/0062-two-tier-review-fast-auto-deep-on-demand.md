# ADR-0062: Two-tier review — a fast auto pass on every PR, a deep review on demand

- **Status:** Accepted
- **Date:** 2026-06-27
- **Deciders:** @stephane-segning

## Context and Problem Statement

The native review agent ([ADR-0037](0037-agent-acts-via-mediated-tools.md)) runs one heavyweight loop for
**every** trigger: clone → (reuse index) → risk-first investigation over graph/vector retrieval +
`read_file` → verification/refute → grouped review. On a real repo with `reasoning_effort` set, that is a
**multi-minute-to-~25-minute** job. It produces excellent, repo-aware reviews — but running it
automatically on **every** opened PR is too slow and too costly for the signal most PRs need (a trivial
version-bump pays the same tax as a subtle concurrency change).

This session pinned the cost precisely — and it is **not** the model or the gateway:

- Same Envoy gateway, same DeepSeek-V4-Flash, local repro: `reasoning_effort` `none → medium` took a
  review from **1m38s → 4m** ([ADR-0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md)).
  The dominant terms are **`reasoning_effort` + turn count + retrieval depth**, not the model id and not
  gateway contention.
- So the lever to make reviews cheap is the **loop shape** (tools / effort / turns / timeout), not a
  model swap. Swapping models is actively *harmful* as a per-tier lever: `stream`/timeout/budget are all
  coupled to the model (ADR-0060), so two models = double the coupling-maintenance trap.

We also now have a **deterministic, near-instant** finding source — SAST via opengrep
([ADR-0061](0061-sast-deterministic-finding-source.md)) — that needs no LLM and no retrieval.

## Decision

**Split review into two tiers, keyed solely by the trigger. One model, two loop shapes.**

### Fast tier — automatic, on `pull_request opened` (PR targets only)
- **SAST (opengrep) is the backbone** — deterministic, no LLM, ~instant.
- **Plus exactly ONE diff-only LLM turn** — no retrieval tools registered (no graph/vector), **no agentic
  loop** (a hard 1-turn cap, not just a low `max_turns`), `reasoning_effort` none/low, short request +
  job timeout. It turns the SAST findings + the raw diff into a human-readable verdict and a cheap
  logic/quality sanity-check.
- Output: ONE grouped PR review (SAST findings + the single-turn verdict), single-channel per
  [ADR-0056](0056-control-plane-owns-the-posted-output.md).
- Target wall-clock: **≲ 2 min**.

### Deep tier — manual, on any `@mention`
- The current heavyweight loop, unchanged: full graph + vector retrieval, `read_file`, `reasoning_effort`
  medium, generous `max_turns`, **streaming on so the per-chunk idle timeout governs**, and a **long job
  timeout (2h is acceptable)** — it is user-requested and async, so it can take its time.
- On a **PR** → a deep repo-aware review. On an **issue** → a conversational answer (the
  [ADR-0033](0033-inbound-command-parsing-and-run-kinds.md) issue/answer path is **retained**). The
  `@mention` body is free-form; deep mode handles it per target.

### Cross-cutting
- **Per-tier config — independent blocks (amended 2026-06-27).** This ADR originally said "one model,
  two loop shapes" — vary tools/effort/turns, never the model. **Superseded in practice:** the operator
  wants a cheap fast model + a strong deep model (e.g. GLM-5.2 on `@mention`), so each tier gets a
  **fully-independent config block** (`review.fast` / `review.deep` in ai-helm-values — own model,
  gateway, prompt, reasoning budget, timeout). The runner accepts BOTH the flat `review.*` (legacy: both
  tiers share it) and the nested blocks, so it deploys before the values are restructured (transition-
  safe, `deny_unknown_fields`). The structural fast behavior (a short, turn-capped diff-only pass with no
  retrieval — see the amendment) is keyed on the tier, independent of which model the fast block names.
- **Same system prompt for both.** The persona/standards are constant; only the toolset and budget
  differ. Caveat: the prompt's "how you investigate" section assumes retrieval — the fast tier simply
  does not register those tools (the model only ever sees the tools it has), and the fast-tier prompt
  must not *promise* tools it lacks. The factual `TOOL_PROTOCOL` (code) already varies by registered
  tools.
- **The "we're on it" acknowledgement is comment-free.** A **👀 reaction** on the trigger plus a **Check
  Run** ("Lightbridge — reviewing…" → "done / N findings"), using the existing `Checks: Read/Write`
  permission. A status *comment* is explicitly rejected — it would re-introduce the multi-channel clutter
  ADR-0056 / #226 just removed.

### Mechanism
- A **`tier`** (`fast` | `deep`) on the task, derived from the trigger: `pull_request opened` → `fast`;
  `@mention` → `deep`. Carried in the task context to the runner.
- **Per-tier runner config** (enabled tool-set, `reasoning_effort`, `max_turns`, request timeout, job
  `activeDeadlineSeconds`) — two blocks in `ai-helm-values`, resolved by the runner from the task's tier,
  reusing the [ADR-0051](0051-per-model-config.md) per-config machinery.
- The fast tier runs SAST, registers no retrieval tools, executes a single capped LLM turn, and
  finalizes — it never enters the investigation/verification loop.

## Consequences

- **Good:** every PR gets a sub-2-min deterministic + light-LLM signal; the expensive deep review is
  deliberate and on-demand, so cost is bounded (no 25-min job per push); the 2h ceiling is safe because
  it applies only to the user-requested deep tier.
- **The fast tier will miss logic bugs that need repo context** — by design. Set expectations in the
  Check Run / verdict ("fast pass — `@lightbridge review` for a deep, repo-aware review").
- **Two code paths** (fast vs deep) in the runner; modest added complexity, mitigated by sharing
  everything except the registered tool-set and the budget. SAST is already wired (ADR-0061), so the fast
  tier is mostly "constrain the existing loop + always run SAST + single-turn cap."
- **Per-PR LLM cost is non-zero** (one short turn each) — the price of a readable verdict over pure SAST.
  (SAST-only auto was considered and rejected: it gives no logic/quality read and no human verdict.)
- Keeps the issue/answer surface (ADR-0033) and the single-channel PR output (ADR-0056) intact.

## Alternatives considered

- **Weaker model on auto, stronger on manual.** Rejected — the model isn't the cost driver, and a
  per-tier model doubles the `stream`/timeout/budget coupling burden (ADR-0060).
- **SAST-only fast tier (no LLM).** Cheapest, fully deterministic, but no logic/quality signal and no
  human-readable verdict on auto. Rejected in favour of SAST + one capped turn.
- **A "reviewing…" status comment.** Rejected — re-introduces the multi-channel clutter ADR-0056 / #226
  removed; a reaction + Check Run conveys the same with no comment noise.
- **Disable auto review entirely (manual-only).** Considered; rejected because a cheap deterministic
  auto signal on every PR is worth keeping once it no longer costs a full deep review.

## References

- [ADR-0033](0033-inbound-command-parsing-and-run-kinds.md) — run kinds + targets (issue/answer path retained).
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — the mediated-tools agent loop both tiers share.
- [ADR-0039](0039-agent-llm-resilience-and-observability.md) — timeouts/streaming the deep tier relies on.
- [ADR-0051](0051-per-model-config.md) — per-config machinery reused for per-tier config.
- [ADR-0056](0056-control-plane-owns-the-posted-output.md) — single-channel PR output the ack must not break.
- [ADR-0060](0060-capture-model-reasoning-and-glm-5-2-latency-finding.md) — the cost diagnosis (effort/turns/retrieval, not model/gateway).
- [ADR-0061](0061-sast-deterministic-finding-source.md) — SAST/opengrep, the fast tier's backbone.

## Amendment (2026-06-28) — fast-tier hardening: dedicated prompt, config-driven tools, real-handle framing

Live dogfood of the first fast-tier rollout (vymalo-shop #303/#304/#305) worked end-to-end — findings
posted inline, no retrieval leaked — but surfaced three rough edges, all rooted in the fast tier **reusing
the deep system prompt**. The deep prompt tells the model to *investigate first* (search / graph /
`read_file`), so on a tier where those tools don't exist, M2.7 opened each run by **calling tools that get
refused** (turns 0–2 on #304), only then reviewed the diff, and so **ran out of its turn budget before
`finish`** — landing in the `Exhausted` backstop every time instead of producing a clean verdict. Two
consequences followed: (a) the exhausted-pass note was a generic banner that **didn't acknowledge the
findings** the run had actually posted, and it hardcoded the wrong **`@lightbridge`** handle (the real App
is `lightbridge-assistant`); (b) without repo access the model **over-rated unverifiable concerns as P0/P1**
(a client-side Flutter route "auth" P1 on #303 that a client route cannot actually gate).

Three changes, keeping the ADR's decision intact:

1. **Dedicated fast system prompt** (`config.reviewSystemPromptFast` → `review-system-fast.md`, pointed at
   by `review.fast.system_prompt_file`). It never mentions retrieval/`read_file`, tells the model to review
   the diff directly, record findings, and **always `finish` with a verdict** (even if clean), and — the
   calibration fix — to **raise only what the diff proves**, phrasing the unverifiable as a P2 question, not
   a P0/P1. The deep tier keeps the full `reviewSystemPrompt`.
   - **Anti-noise rule (added 2026-06-28, dogfood vymalo-shop#316).** Early fast passes produced calibrated
     *verdicts* but still recorded content-free "findings" (a P2 *"comment-only change, no functional
     impact"*) because the lean prompt said "record findings" without forbidding non-findings — a rule the
     deep prompt already had. The fast prompt now states: **a finding is a provable defect/risk on a changed
     line; a clean diff means zero findings + a clean verdict; never record "looks fine" / "no functional
     change" / "comment-only change" / a summary as a finding** (`add_review_comment` is not a notepad).
     Prompt-only change in ai-helm-values — no runner/chart change.
2. **Config-driven per-tier tool allowlist** (`review.<tier>.tools`). The exact tool names a tier offers are
   now declared in `ai-helm-values` (fast = `[add_review_comment, finish, abort]`) instead of relying on the
   runner's hardcoded wind-down set. It is a **closed enum** (`ReviewTool`), so an unknown name **fails at
   config parse** with serde naming the valid variants — no free-form string to hand-validate; the
   fast-tier non-offered-tool refusal guard still backstops a hallucinated call, and the allowlist is now
   honoured in the wind-down tail too (it derives from the restricted set). (Tools were already hidden from
   fast mode — this externalizes the set so an operator tunes each tier from the ConfigMap.)

   **The configurable tools** (the values for `review.<tier>.tools` — the operator-facing surface):

   | Tool | What it does | Kind |
   |---|---|---|
   | `lightbridge_vector_semantic_search` | Semantic (embedding) search over the indexed repo | retrieval |
   | `lightbridge_graph_find_symbol` | Locate a symbol in the code graph | retrieval |
   | `lightbridge_graph_get_callers` | Find callers of a symbol in the code graph | retrieval |
   | `read_file` | Read a file slice from the checkout | retrieval |
   | `add_review_comment` | Record one inline finding on a diff line | write |
   | `retract_finding` | Drop a previously recorded finding (refute pass) | write |
   | `add_comment` | Post a non-inline reply/remark | write |
   | `report_progress` | Log an internal progress note (not posted) | control |
   | `finish` | End the run with a verdict + post the buffer | control |
   | `abort` | End the run without posting (can't produce a result) | control |

   A tier with no allowlist uses the built-in default: the **full** surface for DEEP; the
   write/`finish`/`abort` wind-down set for FAST. An allowlist must be non-empty and should include
   `finish` (and usually `abort`) so the run can converge.
3. **Control-plane-owned fast framing.** The "🅵 quick pass — mention @handle for a deeper review" body is
   now rendered at `finalize_review` (`render_fast_body`), where the **real** App handle lives
   (`GITHUB_APP_HANDLE`), instead of by the runner (which couldn't know it). Keyed on the task `tier`, it
   marks **every** fast review as a quick pass (a blockquote banner distinct from the deep review's heading),
   appends the model's verdict when present, and posts the inline findings either way. The runner no longer
   sets a fast summary.

**Deploy ordering** (the `deny_unknown_fields` rule — the runner rejects an agent.json with a field it
doesn't know): ship the **runner image**
carrying `review.<tier>.tools` first, then the **ai-helm** chart (renders the field + the second prompt
file), then the **ai-helm-values** that set them. The fast prompt alone needs no new runner (it rides the
existing `system_prompt_file`); only the `tools` field gates on the new image.

## Amendment (2026-07-03) — file-boundary diff packing + coverage disclosure

**Problem.** The diff pasted into the prompt was capped by a single **byte cut**
(`truncate_on_boundary(&pr.diff, review.max_diff_chars)`, default 60 000). On a large PR this sliced
mid-file with no awareness of file boundaries, and rendered files in raw `git diff` order — so a lockfile
ahead of source burned budget before any code was shown, and everything past the cut was silently absent
with nothing telling the model. Observed on
[vymalo#274](https://github.com/vymalo/lightbridge-code-intelligence/pull/274#discussion_r3518142422): a
132 KB diff was cut at byte 60 000, exactly **278 bytes before** `pub fn set_owner_only`, whose call site
*was* visible — so the fast pass honestly (but wrongly, on the fast tier's own contract) filed a **P1**
asking to "verify" a definition it was simply never given, while ~55 % of the PR (all of `tui/*`, `cli.rs`,
`config.rs`, `main.rs`, the ADR) never entered the prompt. `Cargo.lock` had eaten the first 12.6 KB.

**Change** (runner-only; `services/agent-runner/src/review/native/diff.rs`, a pure, unit-tested module —
no config, chart, or control-plane change):

1. **Truncate on file boundaries, not bytes.** The diff is split into per-file `diff --git …` sections and
   packed **whole** until the next won't fit. A file is shown completely or listed as not-shown — never cut
   mid-hunk. A single section larger than the whole budget is boundary-truncated only when it would
   otherwise blank the diff.
2. **Deprioritise generated / lock-file noise.** Lockfiles (`Cargo.lock`, `pnpm-lock.yaml`, `go.sum`, …),
   `*.min.js`, `*.map`, `*.snap`, etc. are set aside and *listed* rather than rendered, freeing the budget
   for source. (A PR that changes *only* such files still renders them, so the diff isn't blank.)
3. **Disclose coverage in the prompt.** Files not shown — both the noise list and any source omitted for
   budget — are named in an explicit block, with the instruction: *you have not seen these changes; do not
   raise a finding about them; if a visible line depends on one, treat it as an unverifiable question (at
   most P2), not a defect; state in your verdict which files you could not review.* This both stops the P1
   misfire and lets the model's own verdict (which flows into `render_fast_body` / `render_body`) carry
   honest coverage into the posted review — no cross-service plumbing.

**Deliberately not done here:** a *deterministic* coverage line rendered control-plane-side. It would need
a dedicated coverage field threaded runner → control-plane → DB → `render_*_body` (the summary upsert key
is shared with the model's `finish` verdict, so setting one clobbers the other), a meaningfully larger
change touching the ADR-0068 finalize path. The prompt-side disclosure above covers the observed failure;
the deterministic line is a follow-up if the model-authored coverage statement proves unreliable in
practice.

## Amendment (2026-07-22) — fast-tier parity: tools and gates unified, budget stays the differentiator

**Problem.** A live fast-tier review on
[ai-helm-values#111](https://github.com/ADORSYS-GIS/ai-helm-values/pull/111) demonstrated exactly what
this ADR designed: a diff-only pass that flagged a cross-file assumption (whether `charts/lakefs-secrets`
generates `lakefs-app-secret`) as an unverifiable P2 hedge instead of confirming or refuting it — because
it structurally could not look. Users flagged this as the fast tier being "stupid" by design, not a bug.
Investigation confirmed the "no retrieval" framing above (Decision, Mechanism) was already **stale**
before this amendment: on the live OpenCode path (ADR-0097), the mediated MCP surface
(`services/review-mcp/src/main.rs`) registers the full retrieval tool set for **both** tiers regardless of
`review.<tier>.tools` — the fast tier's "no repo access" behavior came entirely from (a) three `fast: bool`
short-circuits in `CoverageGate`/`RefuteGate`/`SastAnchorGate` that skipped bounce/refute/disclosure
discipline for fast, and (b) a system prompt telling the model not to bother. Neither is a hard tool wall.

**Decision.** Fast tier stops being structurally dumber. It becomes a **plain mechanical copy of deep** —
same tools, same `CoverageGate`/`RefuteGate`/`SastAnchorGate` mechanics (all three gates had their `fast`
parameter removed entirely; `services/review-agent/src/policies/{coverage,refute,sast_anchor}.rs`) —
differentiated from deep by exactly three things:

1. **A weaker/cheaper model**, unchanged, still deliberately below [ADR-0069](0069-review-tier-minimum-model-capability.md)'s reasoning floor (that ADR's own concern is addressed head-on in its companion amendment, not silently contradicted).
2. **A smaller re-prompt-cycle budget** (`review.<tier>.max_cycles`, ai-helm-values `config.model.<tier>.maxCycles`) — see the correction below for why this, not opencode's own step cap, is the mechanism.
3. **A rewritten fast system prompt** (`reviewSystemPromptFast`) dropping the now-false "you have no repository access… those calls are refused" framing, reframed around efficient, budget-conscious investigation instead of an assumed tool wall.

**Correction to "Mechanism" above:** "the fast tier runs SAST, registers no retrieval tools, executes a
single capped LLM turn, and finalizes — it never enters the investigation/verification loop" is no longer
the design. Tools are un-gated on the live path (only `run_sast` remains explicitly allowlisted per tier,
ADR-0073); the investigation/verification loop now runs for fast too, bounded by budget, not by mechanism.

**A real dead end, worth recording so it isn't re-attempted uninformed:** the original plan for item 2
was to retire the Rust-side re-prompt ceiling entirely and adopt opencode's own native per-agent step cap
(`maxSteps`) as the sole budget mechanism — "if we adopt opencode, adopt its params." A real driven e2e
test against the pinned opencode binary disproved this: `agent.build.maxSteps` is schema-accepted
(`opencode debug config` resolves it) but does **not** functionally cap anything over ACP — a session
against a provider that never finishes made 600+ round-trips in under a minute with `maxSteps: 3` set
(`services/agent-runner/src/review/opencode.rs`'s `agent_build_max_steps_does_not_cap_a_never_finishing_model`
e2e, kept as a permanent regression canary — it should start FAILING if a future opencode version fixes
this, which is the signal to revisit). So the Rust-side ceiling stays, renamed conceptually from a
hardcoded `MAX_REVIEW_CYCLES` constant to a tier-configurable `review.<tier>.max_cycles` field — it is the
real stuck-model backstop on the OpenCode path, same job as before, now tunable per tier instead of one
shared number. `review.<tier>.max_turns` is unrelated and unchanged: it only ever fed `CoverageGate`'s
wind-down nudge heuristic, never opencode's own execution.

**Also fixed in the same change, discovered by the same e2e-proof methodology:** the checked-in
`integrations/opencode/config/review.jsonc` set `prompt`/`description`/`mode` on an `agent.review` block
that — like the `tools` finding already documented in [ADR-0097](0097-review-runs-on-opencode.md) — was
silently ignored, because the live ACP session runs opencode's default `build` agent, not a same-named
custom one. This meant the carefully tier-differentiated system prompt may never have reached the model
in production since the OpenCode cutover. Fixed by moving `prompt`/`description` onto `agent.build`,
proven against the real binary (`agent_build_prompt_reaches_the_real_wire` e2e). Fixing this in the same
change (not a separate ticket) because this amendment's own deliverable — a rewritten fast prompt — would
have been silently inert otherwise.
