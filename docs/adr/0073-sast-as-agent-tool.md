# ADR-0073: SAST (opengrep) becomes an agent-called `run_sast` tool, not a pre-agent pass

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** @stephane-segning

## Context and Problem Statement

[ADR-0061](0061-sast-deterministic-finding-source.md) added opengrep as a **deterministic** finding
source: `main.rs` runs `sast::scan` over the PR's changed files **before** the LLM agent starts, buffers
the findings straight into the review via the mediated `add_review_comment` channel
([ADR-0037](0037-agent-acts-via-mediated-tools.md)), and feeds the agent a `sast::digest` block (injected
as a static prompt block, [ADR-0070](0070-window-proportional-prompt-budgets.md)) so it doesn't
re-report them. The scan is gated only on a global `SastConfig` being enabled and a computable diff —
**not** on tier and **not** on run kind.

That unconditional pre-agent placement has a concrete cost we observed in the logs: opengrep runs at the
**start of every review runner**, including runs that turn out to be **conversational** rather than
review. An `@mention` carries the whole comment body as the command
([webhook.rs](../../services/control-plane/src/http/webhook.rs)), and the run kind (`review` vs `ask`,
[ADR-0033](0033-inbound-command-parsing-and-run-kinds.md)) is **emergent** — decided *inside* the agent
loop by which tools the agent calls ([ADR-0037](0037-agent-acts-via-mediated-tools.md)). But the SAST
pass runs *before* the loop, so a plain "@bot what does this function do?" on a PR still triggers a full
opengrep scan and injects a security digest into a prose answer. The pre-agent pass cannot tell a review
from a question, because at that point nobody can.

The deeper mismatch: SAST is the one capability the agent has **no control over**. Everything else the
agent does — retrieval, reading files, recording findings — is a **tool** it invokes as it reasons
([ADR-0026](0026-native-review-agent.md)/[ADR-0037](0037-agent-acts-via-mediated-tools.md)). SAST is
bolted on before the agent exists. Should SAST instead be a tool the agent calls, so it runs only when
the agent is actually doing a review and can scope/time it?

## Decision

**Replace the pre-agent SAST pass with a mediated `run_sast` tool the agent invokes, on both tiers.**
There is no automatic scan any more: opengrep runs only when the agent calls `run_sast`. This supersedes
ADR-0061's *delivery mechanism* (deterministic, always-before-the-agent, LLM-aware-not-gated) while
**keeping everything else ADR-0061 decided** — opengrep runs in the runner, rides the existing
`add_review_comment` mediated channel (no second poster, no reviewdog), language-scopes its ruleset,
parses SARIF, and carries `priority` + `category: security`.

Mechanics (as shipped — a per-crate extraction, R1e, moved the review agent into its own
`lci-review-agent` crate before this landed, so the tool lives one level down from where this ADR
originally sketched it):

- **New built-in tool `run_sast`**, registered as any other built-in review tool is
  (`services/review-agent/src/tools/sast.rs`, wired into `tool_defs()` / `known_tool_names()` /
  `tool_registry()` in `services/review-agent/src/tools.rs`). Its `call()` is today's pre-pass, moved
  verbatim (now in a shared `lci-agent-sast` crate both `agent-runner` and `review-agent` depend on):
  `scan` over the changed files → `buffer` (the same mediated `add_review_comment` writes) → return
  `digest`'s text as the **tool result** to the model (instead of a static prompt block). Optional
  `files` arg to scope the scan to a subset of the changed set; absent → scan all changed files.
  Idempotent: re-calling upserts by `(file, line)` exactly as the mediated channel already dedups. The
  pre-agent pass lived in `services/agent-runner/src/run.rs` (`run_sast_pass`, called from
  `perform_review`) — deleted outright, not deprecated.
  - The SAST anchor gate (#305, `SastAnchorGate`) previously received its `Vec<SastLead>` at loop
    construction, before the pre-pass's findings existed as a value. Since `run_sast` may not be called
    until mid-loop (or never), the gate now reads from a shared `SastLeadSink`
    (`Arc<Mutex<Vec<SastLead>>>`) the tool pushes into as it scans, drained at the top of every
    `after_turn_actions` call — the gate still catches a misanchored verdict recorded the SAME turn as
    the `run_sast` call that produced the real coordinates.
- **Selectable per tier** ([ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) +
  [ADR-0066](0066-deep-tier-external-knowledge-tools.md)): `run_sast` becomes a `ReviewTool` enum variant
  so an operator lists it in `review.<tier>.tools`. Because the fast path strips everything except
  write/finish/abort unless the tier declares an explicit `review.tools` allowlist, **both** `review.fast`
  and `review.deep` must include `run_sast` in their allowlists (an ai-helm-values change) for the tool to
  be offered. Where it isn't listed, that tier simply runs no SAST.
- **Prompt guidance** is the primary safeguard: the reviewer prompt instructs the agent to call
  `run_sast` early in a review (before `finish`) and not to re-report what it returns (the digest already
  says "recorded — retract with `retract_finding` if false"). This is the honor-system half ADR-0061
  deliberately avoided; see Consequences.
- **Remove** the pre-agent pass (`run_sast_pass` in `services/agent-runner/src/run.rs`), the
  `sast_digest` parameter threaded through `run_native_agent` / `build_messages`, and the static SAST
  prompt block + its `budgets.sast` share (ADR-0070) — the digest now arrives as a tool result, not
  injected context. `scan` / `buffer` / `digest` and `SastConfig` (still global) are **kept and reused**
  by the tool, now living in their own `lci-agent-sast` crate so both `agent-runner` (which resolves
  `SastConfig` from env/file config) and `lci-review-agent` (which dispatches the tool) can depend on
  them without a cycle.
- **The fast tier stays multi-turn.** `FAST_TIER_MAX_TURNS = 5` already gives the call-`run_sast`-then-act
  round trip room (fast is cheap via *no retrieval* + *no refute bounce*, not via a single turn), so a
  tool works on fast without changing its loop shape.

## Consequences

- **Good:** a conversational `ask` never scans — the waste that motivated this disappears, because SAST
  only fires when the agent decides it's reviewing. The agent can scope a scan to specific files, run it
  when it's ready to reason about the results, and the security digest no longer pollutes prose answers.
- **Good:** SAST stops consuming a static prompt-block budget (ADR-0070) on every run; the ~1.5% window
  share it reserved is freed, and the digest is spent only when actually produced.
- **Good:** SAST is now uniform with every other capability — a mediated tool, gated by the same per-tier
  `review.tools` allowlist as retrieval and knowledge tools, instead of a special pre-agent side channel.
- **Bad (the deliberate reversal):** SAST is now **LLM-gated**. ADR-0061 ran it before the agent
  *specifically so a weak or lazy model could not skip security scanning* — the exact "honor-system gate
  gets gamed by a weak model" failure family [ADR-0069](0069-review-tier-minimum-model-capability.md) was
  written about. A model that never calls `run_sast` gets **zero** deterministic coverage, silently. The
  accepted mitigation is prompt instruction only; there is no forced call (that option — "tool + forced
  first call" — was considered and rejected in favour of a pure tool). If gaming shows up in practice, a
  finish-time safeguard (bounce/disclose a `finish` that never called `run_sast` on a review run, in the
  spirit of ADR-0069's coverage gate) is the natural follow-up — noted, not built.
- **Neutral:** the "SAST posts regardless of the LLM" property (ADR-0061) and the buffer-before-the-agent
  upsert-collision ordering (a SAST finding was buffered first, so an agent finding on the same
  `(file, line)` won the upsert) both move inside the tool-call path. Dedup by `(file, line)` still holds;
  which finding wins a collision now depends on call order (agent-before-`run_sast` → SAST wins; after →
  agent wins). Low-stakes given the digest tells the agent not to re-report.
- **Neutral (operational):** shipping this without adding `run_sast` to `review.fast.tools` /
  `review.deep.tools` in ai-helm-values silently disables SAST on that tier. The values change is part of
  rollout, and its absence is legible (no `run_sast` calls in the transcript).

## Alternatives considered

- **Keep the pre-agent pass (status quo, ADR-0061).** Rejected per this decision: it scans conversational
  runs and cannot be scoped or timed by the agent. Its one real virtue — determinism — is what we are
  knowingly trading away.
- **Hybrid: `run_sast` tool on deep, keep the deterministic pass on fast.** Preserves the deterministic
  guarantee on the tier with the weakest model (where gaming is most likely) while giving deep the
  on-demand tool. Rejected in favour of a uniform model on the owner's call; the fast tier's 5-turn budget
  makes a tool viable there, and uniformity was preferred over a two-mode SAST.
- **Tool everywhere + forced first call** (auto-invoke `run_sast` once on a review's turn 0). Keeps
  determinism while still skipping pure asks. Rejected: it is largely the current pre-pass wearing a tool
  costume and reintroduces the up-front review-vs-ask classification ADR-0037 avoided; the owner chose a
  pure tool.
- **Agent re-emits SAST findings itself** (the tool only *returns* findings; the agent must
  `add_review_comment` each one to post it). Rejected: doubles the agent's work and lets it silently drop
  a real finding; keeping `run_sast` on the mediated buffer channel means a scanned finding is recorded
  the moment the tool runs, exactly as today.

## References

- [ADR-0061](0061-sast-deterministic-finding-source.md) — the deterministic pre-agent SAST pass this
  supersedes; the scan/buffer/digest/SARIF/language-scoping machinery is retained and reused.
- [ADR-0037](0037-agent-acts-via-mediated-tools.md) — the mediated-tool model `run_sast` joins; also the
  source of the emergent run-kind that made the pre-pass fire on conversational runs.
- [ADR-0062](0062-two-tier-review-fast-auto-deep-on-demand.md) + [ADR-0066](0066-deep-tier-external-knowledge-tools.md)
  — the per-tier `review.<tier>.tools` allowlist `run_sast` is selected through.
- [ADR-0069](0069-review-tier-minimum-model-capability.md) — the "honor-system gate gets gamed by a weak
  model" precedent; the risk this ADR knowingly accepts, and the shape of the follow-up safeguard if it
  materializes.
- [ADR-0070](0070-window-proportional-prompt-budgets.md) — the static SAST digest block + its window
  budget share, removed here.
